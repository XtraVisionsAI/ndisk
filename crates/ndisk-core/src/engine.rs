//! 引擎门面：平台适配层只与本模块交互。

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

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
            cfg.cache.max_bytes,
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
        let etag = n
            .etag
            .as_deref()
            .ok_or_else(|| Error::NotFound(n.name.clone()))?;
        let mut out = Vec::with_capacity((end - range.start) as usize);
        for b in first..=last {
            let data = self
                .vfs
                .content()
                .read_block(id, etag, b)
                .await?
                .ok_or_else(|| Error::Io(std::io::Error::other("block vanished")))?;
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

    // ---- 缓存控制（右键菜单）----

    pub async fn pin(&self, id: ItemId) -> Result<()> {
        self.vfs.meta().set_pinned(id, true)?;
        self.download(id).await
    }

    pub fn unpin(&self, id: ItemId) -> Result<()> {
        self.vfs.meta().set_pinned(id, false)
    }

    /// 清理本地缓存（有未上传修改时拒绝）。
    pub async fn evict(&self, id: ItemId) -> Result<()> {
        if self.vfs.meta().upload_get(id)?.is_some() {
            return Err(Error::PermissionDenied("item has pending upload".into()));
        }
        self.vfs.content().evict(id).await
    }

    /// 后台维护：容量约束 + TTL 驱逐。平台层定期调用。
    pub async fn maintain(&self) -> Result<()> {
        self.vfs.content().enforce_capacity().await?;
        // TODO: 按 CachePolicy::Ttl 驱逐超期块（需要 blocks.last_used 与节点策略联合查询）。
        Ok(())
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

/// 确保节点在 `blocks` 范围内的块都已在本地。并发受 scheduler 限制。
pub(crate) async fn ensure_blocks(
    vfs: &Arc<Vfs>,
    sched: &Scheduler,
    n: &Node,
    blocks: Range<u64>,
) -> Result<()> {
    let Some(etag) = n.etag.as_deref() else {
        return Ok(());
    };
    let content = vfs.content();
    let mut tasks = tokio::task::JoinSet::new();
    for b in blocks {
        if content.has_block(n.id, etag, b) {
            continue;
        }
        let vfs = vfs.clone();
        let sched = sched.clone();
        let n = n.clone();
        let etag = etag.to_string();
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
