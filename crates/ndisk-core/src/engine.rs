//! 引擎门面：平台适配层只与本模块交互。

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use bytes::Bytes;

use crate::backend::DynBackend;
use crate::cache::{ContentCache, MetaStore};
use crate::config::MountConfig;
use crate::sched::{retry, Scheduler};
use crate::types::*;
use crate::vfs::Vfs;
use crate::writeback::Writeback;
use crate::{Error, Result};

pub struct Engine {
    pub cfg: MountConfig,
    pub vfs: Arc<Vfs>,
    pub writeback: Writeback,
    pub sched: Scheduler,
}

impl Engine {
    /// `cache_root` 为该挂载点专用目录（平台层负责决定位置）。
    pub fn open(cfg: MountConfig, backend: DynBackend, cache_root: PathBuf) -> Result<Self> {
        cfg.validate()?;
        let meta = Arc::new(MetaStore::open(&cache_root.join("meta.sqlite"))?);
        let content = Arc::new(ContentCache::new(
            cache_root,
            cfg.cache.block_size,
            meta.clone(),
        )?);
        let prefix = match &cfg.backend {
            crate::config::BackendConfig::S3(s) => s.prefix.clone(),
            crate::config::BackendConfig::Opendal { .. } => String::new(),
        };
        let vfs = Arc::new(Vfs::new(backend, meta, content, cfg.cache.clone(), prefix));
        let sched = Scheduler::new(&cfg.network);
        let writeback = Writeback::new(vfs.clone(), sched.clone(), cfg.cache.clone());
        Ok(Self {
            cfg,
            vfs,
            writeback,
            sched,
        })
    }

    /// 节点视图，叠加写路径状态：脏文件存在时大小取脏文件，状态为 Dirty / Uploading。
    pub fn stat(&self, id: ItemId) -> Result<Node> {
        let mut n = self.vfs.get(id)?;
        if n.kind == Kind::File {
            let dirty = self.vfs.content().dirty_path(id);
            if let Ok(m) = std::fs::metadata(&dirty) {
                n.size = m.len();
                n.mtime = m.modified().ok();
                n.local = match self.vfs.meta().upload_get(id)? {
                    Some(rec) if rec.state == crate::cache::meta::UploadState::Uploading => {
                        LocalState::Uploading
                    }
                    _ => LocalState::Dirty,
                };
            }
        }
        Ok(n)
    }

    pub fn ensure_writable(&self) -> Result<()> {
        if self.cfg.read_only {
            Err(Error::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// 在后台执行上传，不阻塞调用方（FUSE release / File Provider modifyItem 之后调用）。
    pub fn spawn_flush(self: &Arc<Self>, id: ItemId) {
        let me = self.clone();
        tokio::spawn(async move {
            match me.writeback.flush(id).await {
                Ok(e) => tracing::info!(item = id.0, key = %e.key, "upload complete"),
                Err(e) => {
                    tracing::warn!(item = id.0, error = %e, "upload failed; will retry on resume")
                }
            }
        });
    }

    // ---- 读路径 ----

    /// 读取文件内容：命中脏文件则读脏文件，否则按块从缓存/后端读取。
    pub async fn read(&self, id: ItemId, range: Range<u64>) -> Result<Bytes> {
        let n = self.vfs.get(id)?;
        if n.kind != Kind::File {
            return Err(Error::IsADirectory(n.name));
        }
        if self.vfs.content().dirty_path(id).exists() {
            return self.vfs.content().read_dirty(id, range).await;
        }
        let end = range.end.min(n.size);
        if range.start >= end {
            return Ok(Bytes::new());
        }
        let bs = self.vfs.content().block_size();
        let first = range.start / bs;
        let last = (end - 1) / bs;
        ensure_blocks(&self.vfs, &self.sched, &n, first..last + 1).await?;
        let version = n.cache_key();
        let mut out = Vec::with_capacity((end - range.start) as usize);
        for b in first..=last {
            // 驱逐器可能在 ensure 与读取之间删掉块：补拉一次。
            let data = match self.vfs.content().read_block(id, &version, b).await? {
                Some(d) => d,
                None => {
                    ensure_blocks(&self.vfs, &self.sched, &n, b..b + 1).await?;
                    self.vfs
                        .content()
                        .read_block(id, &version, b)
                        .await?
                        .ok_or_else(|| Error::Io(std::io::Error::other("block vanished")))?
                }
            };
            let br = self.vfs.content().block_range(b, n.size);
            let s = range.start.max(br.start) - br.start;
            let e = end.min(br.end) - br.start;
            out.extend_from_slice(&data[s as usize..e as usize]);
        }
        Ok(Bytes::from(out))
    }

    /// 完整下载（File Provider `fetchContents` / 右键"立即下载"）。
    pub async fn download(&self, id: ItemId) -> Result<()> {
        let n = self.vfs.get(id)?;
        ensure_blocks(
            &self.vfs,
            &self.sched,
            &n,
            0..self.vfs.content().block_count(n.size),
        )
        .await
    }

    /// 把文件完整内容导出到 `dst`（File Provider `fetchContents`）。
    ///
    /// 有未上传的本地修改时导出脏文件。`keep_cache = false` 时导出后清掉块缓存：
    /// 这类平台由系统持有副本，避免双份占用。导出成功即登记为"已下载"。
    pub async fn export_to(
        &self,
        id: ItemId,
        dst: &std::path::Path,
        keep_cache: bool,
    ) -> Result<Node> {
        let n = self.stat(id)?;
        if n.kind != Kind::File {
            return Err(Error::IsADirectory(n.name));
        }
        if let Some(dir) = dst.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let dirty = self.vfs.content().dirty_path(id);
        if dirty.exists() {
            let _guard = self.writeback.lock(id).await;
            tokio::fs::copy(&dirty, dst).await?;
        } else {
            let base = self.vfs.get(id)?;
            self.download(id).await?;
            self.vfs
                .content()
                .concat_blocks_to(id, &base.cache_key(), base.size, dst)
                .await?;
            if !keep_cache && self.effective_policy(id)? != CachePolicy::Always {
                self.vfs.content().evict(id).await?;
            }
        }
        self.vfs.meta().mark_materialized(id)?;
        Ok(n)
    }

    /// 用外部文件替换内容并上传，返回上传后的节点（File Provider createItem / modifyItem）。
    pub async fn import_and_upload(&self, id: ItemId, src: &std::path::Path) -> Result<Node> {
        self.ensure_writable()?;
        self.writeback.import(id, src).await?;
        match self.writeback.flush(id).await {
            Ok(_) => self.stat(id),
            Err(Error::Conflict { .. }) => {
                self.resolve_conflict(id).await?;
                self.stat(id)
            }
            Err(e) => Err(e),
        }
    }

    /// 远端在我们编辑期间被修改：本地内容另存为同目录下的冲突副本并上传，原条目改为远端最新版本。
    /// 返回冲突副本节点。
    pub async fn resolve_conflict(&self, id: ItemId) -> Result<Node> {
        let n = self.vfs.get(id)?;
        let dirty = self.vfs.content().dirty_path(id);
        let copy_name = conflict_name(&n.name, SystemTime::now());
        let copy = self.vfs.create_file(n.parent, &copy_name).await?;
        {
            let _guard = self.writeback.lock(copy.id).await;
            tokio::fs::copy(&dirty, self.vfs.content().dirty_path(copy.id)).await?;
        }
        // 原条目：丢弃本地修改，回到远端版本。
        self.vfs.meta().upload_remove(id)?;
        self.vfs.content().remove_dirty(id).await?;
        self.vfs.content().evict(id).await?;
        if let Some(e) = self.vfs.backend().stat(&n.remote_key).await? {
            self.vfs.meta().upsert(n.parent, &n.name, &e, &e.key)?;
        }
        // 冲突副本：新对象，不带 If-Match。
        self.writeback.enqueue(copy.id).await?;
        self.writeback.flush(copy.id).await?;
        tracing::warn!(item = id.0, copy = %copy_name, "remote changed during edit; saved local version as conflicted copy");
        self.vfs.get(copy.id)
    }

    /// 刷新最近 `within` 内被列举过的目录，发现远端变更。返回新产生的变更条数。
    pub async fn refresh_hot_dirs(&self, within: Duration) -> Result<u64> {
        let since = SystemTime::now()
            .checked_sub(within)
            .unwrap_or(std::time::UNIX_EPOCH);
        let before = self.vfs.meta().latest_anchor()?;
        for dir in self.vfs.meta().recently_listed_dirs(since)? {
            if let Err(e) = self.vfs.list(dir, true).await {
                tracing::warn!(dir = dir.0, error = %e, "refresh failed");
            }
        }
        Ok(self.vfs.meta().latest_anchor()?.0 - before.0)
    }

    /// 系统持有副本的平台：TTL 已到期、应当请求系统驱逐的文件。
    /// TTL 从下载时刻起算；始终缓存与有未上传修改的文件不在其中。
    pub fn eviction_candidates(&self, now: SystemTime) -> Result<Vec<ItemId>> {
        let mut out = Vec::new();
        for (id, at) in self.vfs.meta().materialized_items()? {
            let Some(node) = self.vfs.meta().get(id)? else {
                self.vfs.meta().mark_evicted(id)?;
                continue;
            };
            // 系统上报的集合包含根容器与目录；只有文件才有可驱逐的内容。
            if node.kind != Kind::File {
                continue;
            }
            if self.has_local_changes(id)? {
                continue;
            }
            let age = now.duration_since(at).unwrap_or_default();
            let expired = match self.effective_policy(id)? {
                CachePolicy::Always => false,
                CachePolicy::Ttl(d) => age > d,
                CachePolicy::None => age > NONE_POLICY_GRACE,
            };
            if expired {
                out.push(id);
            }
        }
        Ok(out)
    }

    // ---- 缓存控制（右键菜单 / CLI）----

    /// 固定为始终缓存。目录会递归固定当前已知的所有后代并下载。
    pub async fn pin(&self, id: ItemId) -> Result<()> {
        self.set_policy(id, Some(CachePolicy::Always)).await
    }

    /// 取消固定，回到继承策略。已下载的内容保留，之后按策略自然驱逐。
    pub fn unpin(&self, id: ItemId) -> Result<()> {
        self.vfs.meta().set_pinned(id, false)?;
        if matches!(self.vfs.get(id)?.policy, Some(CachePolicy::Always)) {
            self.vfs.meta().set_policy(id, None)?;
        }
        Ok(())
    }

    /// 设置节点的缓存策略覆盖；`None` 表示继承父目录或挂载默认值。
    /// 设为 `Always` 时立即把内容预取到块缓存（FUSE / CLI 场景）。
    pub async fn set_policy(&self, id: ItemId, policy: Option<CachePolicy>) -> Result<()> {
        let always = matches!(policy, Some(CachePolicy::Always));
        self.set_policy_only(id, policy)?;
        if always {
            self.prefetch_tree(id, true).await?;
        }
        Ok(())
    }

    /// 只记录策略，不预取。系统持有副本的平台（File Provider）由系统按 `contentPolicy` 下载。
    pub fn set_policy_only(&self, id: ItemId, policy: Option<CachePolicy>) -> Result<()> {
        let always = matches!(policy, Some(CachePolicy::Always));
        self.vfs.meta().set_policy(id, policy.as_ref())?;
        self.vfs.meta().set_pinned(id, always)?;
        Ok(())
    }

    /// 节点的生效策略：固定标记 > 自身覆盖 > 最近祖先覆盖 > 挂载默认。
    pub fn effective_policy(&self, id: ItemId) -> Result<CachePolicy> {
        let mut cur = Some(id);
        while let Some(i) = cur {
            let Some(n) = self.vfs.meta().get(i)? else {
                break;
            };
            if n.local == LocalState::Pinned {
                return Ok(CachePolicy::Always);
            }
            if let Some(p) = n.policy {
                return Ok(p);
            }
            cur = (i != ItemId::ROOT).then_some(n.parent);
        }
        Ok(self.cfg.cache.policy)
    }

    /// 清理本地缓存（有未上传修改时拒绝）。目录递归清理所有后代。
    pub async fn evict(&self, id: ItemId) -> Result<()> {
        let n = self.vfs.get(id)?;
        if n.kind == Kind::Dir {
            for c in self.vfs.meta().children(id)? {
                Box::pin(self.evict(c.id)).await?;
            }
            return Ok(());
        }
        if self.has_local_changes(id)? {
            return Err(Error::PermissionDenied("item has pending upload".into()));
        }
        self.vfs.content().evict(id).await
    }

    fn has_local_changes(&self, id: ItemId) -> Result<bool> {
        Ok(self.vfs.meta().upload_get(id)?.is_some() || self.vfs.content().dirty_path(id).exists())
    }

    /// 下载文件；`recursive` 时对目录递归下载（使用目录 TTL 内的列举结果）。
    async fn prefetch_tree(&self, id: ItemId, recursive: bool) -> Result<u64> {
        let n = self.vfs.get(id)?;
        match n.kind {
            Kind::File => {
                if self.is_complete(&n)? {
                    return Ok(0);
                }
                self.download(id).await?;
                Ok(1)
            }
            Kind::Dir if recursive => {
                let mut count = 0;
                for c in self.vfs.list(id, false).await? {
                    count += Box::pin(self.prefetch_tree(c.id, true)).await?;
                }
                Ok(count)
            }
            Kind::Dir => Ok(0),
        }
    }

    fn is_complete(&self, n: &Node) -> Result<bool> {
        Ok(self
            .vfs
            .content()
            .local_state(n.id, &n.cache_key(), n.size)?
            == LocalState::Cached)
    }

    /// 后台维护，平台层定期调用。
    pub async fn maintain(&self) -> Result<MaintainReport> {
        self.maintain_at(SystemTime::now()).await
    }

    /// 不做固定预取的维护（系统持有副本的平台使用）。
    pub async fn maintain_blocks(&self) -> Result<MaintainReport> {
        self.maintain_inner(SystemTime::now(), false).await
    }

    /// 以给定"当前时间"执行一次维护（便于测试）：
    /// 1. TTL：生效策略为 `Ttl(d)` 且最近访问早于 `now - d` 的文件整体驱逐；`None` 策略使用短宽限期；
    /// 2. 容量：剩余占用仍超过 `max_bytes` 时按 LRU 驱逐未受保护的文件；
    /// 3. 预取：固定的文件与目录中尚未完整缓存的文件补齐下载。
    ///
    /// 始终缓存的文件、有未上传修改的文件不会被驱逐。
    pub async fn maintain_at(&self, now: SystemTime) -> Result<MaintainReport> {
        self.maintain_inner(now, true).await
    }

    async fn maintain_inner(
        &self,
        now: SystemTime,
        prefetch_pinned: bool,
    ) -> Result<MaintainReport> {
        let mut report = MaintainReport::default();
        let mut remaining = Vec::new();
        for item in self.vfs.meta().cached_items()? {
            if self.vfs.meta().get(item.id)?.is_none() {
                // 节点已删除但块记录残留
                self.vfs.content().evict(item.id).await?;
                continue;
            }
            if self.has_local_changes(item.id)? {
                continue;
            }
            let idle = now.duration_since(item.last_used).unwrap_or_default();
            let expired = match self.effective_policy(item.id)? {
                CachePolicy::Always => continue,
                CachePolicy::Ttl(d) => idle > d,
                CachePolicy::None => idle > NONE_POLICY_GRACE,
            };
            if expired {
                self.vfs.content().evict(item.id).await?;
                report.ttl_evicted += 1;
                report.bytes_freed += item.bytes;
            } else {
                remaining.push(item);
            }
        }

        let mut total = self.vfs.meta().total_cached_bytes()?;
        for item in remaining {
            if total <= self.cfg.cache.max_bytes {
                break;
            }
            self.vfs.content().evict(item.id).await?;
            total = total.saturating_sub(item.bytes);
            report.capacity_evicted += 1;
            report.bytes_freed += item.bytes;
        }

        let pinned = if prefetch_pinned {
            self.vfs.meta().pinned_items()?
        } else {
            Vec::new()
        };
        for id in pinned {
            match self.prefetch_tree(id, true).await {
                Ok(n) => report.prefetched += n,
                Err(e) => tracing::warn!(item = id.0, error = %e, "prefetch of pinned item failed"),
            }
        }
        report.cached_bytes = self.vfs.meta().total_cached_bytes()?;
        Ok(report)
    }

    pub async fn resume_uploads(&self) -> Result<()> {
        for (id, r) in self.writeback.resume_all().await? {
            if let Err(e) = r {
                tracing::warn!(item = id.0, error = %e, "resume upload failed");
            }
        }
        Ok(())
    }
}

/// 冲突副本命名：`report.txt` → `report (conflicted copy 2026-10-16 21-40-05).txt`。
pub fn conflict_name(name: &str, at: SystemTime) -> String {
    let secs = at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // 本地时间需要 tz 库；文件名用 UTC 足够，且无额外依赖。
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // 民用日历换算（Howard Hinnant 算法）
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    let stamp = format!("{y:04}-{mo:02}-{d:02} {h:02}-{m:02}-{s:02}");
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} (conflicted copy {stamp}){}", &name[..i], &name[i..]),
        _ => format!("{name} (conflicted copy {stamp})"),
    }
}

/// `CachePolicy::None` 的宽限期：避免正在顺序读取的文件被立刻回收。
pub const NONE_POLICY_GRACE: Duration = Duration::from_secs(60);

/// 一次维护的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct MaintainReport {
    pub ttl_evicted: u64,
    pub capacity_evicted: u64,
    pub bytes_freed: u64,
    pub prefetched: u64,
    pub cached_bytes: u64,
}

/// 确保节点在 `blocks` 范围内的块都已在本地。并发受 scheduler 限制。
pub(crate) async fn ensure_blocks(
    vfs: &Arc<Vfs>,
    sched: &Scheduler,
    n: &Node,
    blocks: Range<u64>,
) -> Result<()> {
    if n.kind != Kind::File {
        return Ok(());
    }
    let version = n.cache_key();
    let content = vfs.content();
    let mut tasks = tokio::task::JoinSet::new();
    for b in blocks {
        if content.has_block(n.id, &version, b) {
            continue;
        }
        let vfs = vfs.clone();
        let sched = sched.clone();
        let n = n.clone();
        let etag = version.clone();
        tasks.spawn(async move {
            let _permit = sched.download.acquire().await.expect("semaphore closed");
            let r = vfs.content().block_range(b, n.size);
            sched.download_limit.acquire(r.end - r.start).await;
            let backend = vfs.backend().clone();
            let key = n.remote_key.clone();
            let data = retry(5, || backend.read(&key, r.clone())).await?;
            vfs.content().write_block(n.id, &etag, b, &data).await
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.map_err(|e| Error::Io(std::io::Error::other(e)))??;
    }
    Ok(())
}
