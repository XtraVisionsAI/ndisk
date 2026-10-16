//! 用内存后端做端到端验证：列目录、读、写、分片上传、断点续传、重命名、驱逐。

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use ndisk_core::config::*;
use ndisk_core::*;

#[derive(Default)]
struct Mem {
    objects: Mutex<BTreeMap<String, (Bytes, String)>>,
    uploads: Mutex<BTreeMap<String, BTreeMap<u32, Bytes>>>,
    /// 模拟"上传第 N 个分片时进程崩溃"。
    fail_part_after: AtomicUsize,
    parts_seen: AtomicUsize,
    fail_armed: AtomicBool,
}

fn etag(b: &[u8]) -> String {
    let mut h: u64 = 1469598103934665603;
    for x in b {
        h ^= *x as u64;
        h = h.wrapping_mul(1099511628211);
    }
    format!("{h:016x}")
}

#[async_trait]
impl Backend for Mem {
    fn name(&self) -> &'static str {
        "mem"
    }
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            multipart: true,
            range_read: true,
            etag: true,
            ..Default::default()
        }
    }

    async fn list(&self, prefix: &str, _cursor: Option<&str>) -> Result<ListPage> {
        let objs = self.objects.lock().unwrap();
        let mut entries = Vec::new();
        let mut dirs = std::collections::BTreeSet::new();
        for (k, (data, tag)) in objs.iter() {
            if !k.starts_with(prefix) || k == prefix {
                continue;
            }
            let rest = &k[prefix.len()..];
            if let Some(i) = rest.find('/') {
                dirs.insert(format!("{prefix}{}/", &rest[..i]));
            } else {
                entries.push(Entry {
                    key: k.clone(),
                    kind: Kind::File,
                    size: data.len() as u64,
                    mtime: Some(SystemTime::now()),
                    etag: Some(tag.clone()),
                    version: None,
                });
            }
        }
        for d in dirs {
            entries.push(Entry {
                key: d,
                kind: Kind::Dir,
                size: 0,
                mtime: None,
                etag: None,
                version: None,
            });
        }
        Ok(ListPage {
            entries,
            next: None,
        })
    }
    async fn stat(&self, key: &str) -> Result<Option<Entry>> {
        let objs = self.objects.lock().unwrap();
        if let Some((d, t)) = objs.get(key) {
            let kind = if key.ends_with('/') {
                Kind::Dir
            } else {
                Kind::File
            };
            return Ok(Some(Entry {
                key: key.into(),
                kind,
                size: d.len() as u64,
                mtime: None,
                etag: Some(t.clone()),
                version: None,
            }));
        }
        if key.ends_with('/') && objs.keys().any(|k| k.starts_with(key)) {
            return Ok(Some(Entry {
                key: key.into(),
                kind: Kind::Dir,
                size: 0,
                mtime: None,
                etag: None,
                version: None,
            }));
        }
        Ok(None)
    }
    async fn read(&self, key: &str, range: Range<u64>) -> Result<Bytes> {
        let objs = self.objects.lock().unwrap();
        let (d, _) = objs.get(key).ok_or_else(|| Error::NotFound(key.into()))?;
        Ok(d.slice(range.start as usize..(range.end as usize).min(d.len())))
    }
    async fn put(&self, key: &str, data: Bytes, opts: PutOptions) -> Result<Entry> {
        let mut objs = self.objects.lock().unwrap();
        if let Some(exp) = &opts.if_match {
            let cur = objs.get(key).map(|(_, t)| t.clone());
            if cur.as_deref() != Some(exp.as_str()) && !(exp.is_empty() && cur.is_none()) {
                return Err(Error::Conflict {
                    key: key.into(),
                    expected: Some(exp.clone()),
                    actual: cur,
                });
            }
        }
        let t = etag(&data);
        let size = data.len() as u64;
        objs.insert(key.into(), (data, t.clone()));
        Ok(Entry {
            key: key.into(),
            kind: if key.ends_with('/') {
                Kind::Dir
            } else {
                Kind::File
            },
            size,
            mtime: None,
            etag: Some(t),
            version: None,
        })
    }
    async fn multipart_start(&self, key: &str, _o: PutOptions) -> Result<UploadId> {
        let id = format!("up-{key}");
        self.uploads
            .lock()
            .unwrap()
            .insert(id.clone(), BTreeMap::new());
        Ok(UploadId(id))
    }
    async fn multipart_put_part(
        &self,
        _k: &str,
        up: &UploadId,
        n: u32,
        data: Bytes,
    ) -> Result<PartEtag> {
        let seen = self.parts_seen.fetch_add(1, Ordering::SeqCst) + 1;
        if self.fail_armed.load(Ordering::SeqCst)
            && seen > self.fail_part_after.load(Ordering::SeqCst)
        {
            self.fail_armed.store(false, Ordering::SeqCst);
            return Err(Error::PermissionDenied("simulated crash".into()));
        }
        let t = etag(&data);
        self.uploads
            .lock()
            .unwrap()
            .get_mut(&up.0)
            .unwrap()
            .insert(n, data);
        Ok(PartEtag(t))
    }
    async fn multipart_complete(
        &self,
        key: &str,
        up: &UploadId,
        parts: &[(u32, PartEtag)],
    ) -> Result<Entry> {
        let m = self.uploads.lock().unwrap().remove(&up.0).unwrap();
        let mut all = Vec::new();
        for (n, _) in parts {
            all.extend_from_slice(&m[n]);
        }
        self.put(key, Bytes::from(all), PutOptions::default()).await
    }
    async fn multipart_abort(&self, _k: &str, up: &UploadId) -> Result<()> {
        self.uploads.lock().unwrap().remove(&up.0);
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<()> {
        self.objects.lock().unwrap().remove(key);
        Ok(())
    }
    async fn copy(&self, from: &str, to: &str) -> Result<Entry> {
        let v = self
            .objects
            .lock()
            .unwrap()
            .get(from)
            .cloned()
            .ok_or_else(|| Error::NotFound(from.into()))?;
        self.put(to, v.0, PutOptions::default()).await
    }
}

fn config(block_size: u64) -> MountConfig {
    config_with(
        block_size,
        CachePolicy::Ttl(std::time::Duration::from_secs(3600)),
        1 << 40,
    )
}

fn config_with(block_size: u64, policy: CachePolicy, max_bytes: u64) -> MountConfig {
    MountConfig {
        id: "t".into(),
        display_name: "t".into(),
        backend: BackendConfig::S3(S3Config {
            bucket: "b".into(),
            prefix: "data".into(),
            region: None,
            endpoint: None,
            force_path_style: false,
            auth: S3Auth::Default,
            dir_markers: true,
            copy_threshold: None,
        }),
        cache: Some(CacheConfig {
            block_size,
            policy,
            max_bytes,
            multipart_threshold: 4096,
            part_size: 5 * 1024 * 1024,
            ..Default::default()
        }),
        network: None,
        read_only: false,
        overrides: Default::default(),
    }
}

fn setup(mem: Arc<Mem>, block_size: u64) -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let e = Engine::open(
        config(block_size),
        mem,
        dir.path.join("data"),
        dir.path.join("cache"),
    )
    .unwrap();
    (e, dir)
}

mod tempdir {
    pub struct Dir {
        pub path: std::path::PathBuf,
    }
    impl Dir {
        pub fn new() -> Self {
            let p =
                std::env::temp_dir().join(format!("ndisk-test-{}-{}", std::process::id(), rand()));
            std::fs::create_dir_all(&p).unwrap();
            Self { path: p }
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
    fn rand() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64
    }
}

async fn resolve(e: &Engine, path: &str) -> ItemId {
    let mut id = ItemId::ROOT;
    for s in path.split('/').filter(|s| !s.is_empty()) {
        id = e.vfs.lookup(id, s).await.unwrap().id;
    }
    id
}

#[tokio::test]
async fn list_read_and_cache_state() {
    let mem = Arc::new(Mem::default());
    let body: Vec<u8> = (0..3000u32).map(|i| (i % 251) as u8).collect();
    mem.put(
        "data/a/hello.bin",
        Bytes::from(body.clone()),
        Default::default(),
    )
    .await
    .unwrap();
    mem.put(
        "data/top.txt",
        Bytes::from_static(b"hi"),
        Default::default(),
    )
    .await
    .unwrap();
    let (e, _d) = setup(mem, 1024);

    let root = e.vfs.list(ItemId::ROOT, false).await.unwrap();
    let names: Vec<_> = root.iter().map(|n| n.name.as_str()).collect();
    assert_eq!(names, vec!["a", "top.txt"]);

    let id = resolve(&e, "a/hello.bin").await;
    assert_eq!(e.vfs.get(id).unwrap().local, LocalState::Cloud);
    // 跨块部分读取
    let got = e.read(id, 1000..1100).await.unwrap();
    assert_eq!(&got[..], &body[1000..1100]);
    assert_eq!(e.vfs.get(id).unwrap().local, LocalState::Partial);
    e.download(id).await.unwrap();
    assert_eq!(e.vfs.get(id).unwrap().local, LocalState::Cached);
    e.evict(id).await.unwrap();
    assert_eq!(e.vfs.get(id).unwrap().local, LocalState::Cloud);
    e.pin(id).await.unwrap();
    assert_eq!(e.vfs.get(id).unwrap().local, LocalState::Pinned);
}

#[tokio::test]
async fn write_small_then_multipart_with_resume() {
    let mem = Arc::new(Mem::default());
    let (e, _d) = setup(mem.clone(), 1024 * 1024);

    // 小文件：整体 put
    let f = e.vfs.create_file(ItemId::ROOT, "small.txt").await.unwrap();
    let p = e.writeback.open_for_write(f.id).await.unwrap();
    std::fs::write(&p, b"hello world").unwrap();
    e.writeback.enqueue(f.id).await.unwrap();
    e.writeback.flush(f.id).await.unwrap();
    assert_eq!(
        &mem.objects.lock().unwrap()["data/small.txt"].0[..],
        b"hello world"
    );
    // 上传后脏文件转为块缓存，直接可读
    assert_eq!(e.vfs.get(f.id).unwrap().local, LocalState::Cached);
    assert_eq!(&e.read(f.id, 0..5).await.unwrap()[..], b"hello");

    // 大文件：分片上传，模拟第 2 个分片后崩溃，再续传
    let big: Vec<u8> = (0..(12 * 1024 * 1024u32)).map(|i| (i % 97) as u8).collect();
    let g = e.vfs.create_file(ItemId::ROOT, "big.bin").await.unwrap();
    let p = e.writeback.open_for_write(g.id).await.unwrap();
    std::fs::write(&p, &big).unwrap();
    e.writeback.enqueue(g.id).await.unwrap();
    mem.fail_part_after.store(1, Ordering::SeqCst);
    mem.fail_armed.store(true, Ordering::SeqCst);
    assert!(e.writeback.flush(g.id).await.is_err());
    let rec = e.vfs.meta().upload_get(g.id).unwrap().unwrap();
    assert_eq!(rec.parts_done.len(), 1, "progress must be persisted");
    let before = mem.parts_seen.load(Ordering::SeqCst);
    e.resume_uploads().await.unwrap();
    let after = mem.parts_seen.load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        2,
        "only the remaining 2 parts should be re-sent"
    );
    assert_eq!(
        mem.objects.lock().unwrap()["data/big.bin"].0.len(),
        big.len()
    );
    assert!(e.vfs.meta().upload_get(g.id).unwrap().is_none());
}

#[tokio::test]
async fn rename_and_delete_keep_item_id() {
    let mem = Arc::new(Mem::default());
    mem.put(
        "data/x/one.txt",
        Bytes::from_static(b"1"),
        Default::default(),
    )
    .await
    .unwrap();
    let (e, _d) = setup(mem.clone(), 1024);
    let x = resolve(&e, "x").await;
    let one = resolve(&e, "x/one.txt").await;
    let y = e.vfs.mkdir(ItemId::ROOT, "y").await.unwrap();
    let moved = e.vfs.rename(one, y.id, "uno.txt").await.unwrap();
    assert_eq!(moved.id, one);
    assert_eq!(moved.remote_key, "data/y/uno.txt");
    assert!(mem.objects.lock().unwrap().contains_key("data/y/uno.txt"));
    assert!(!mem.objects.lock().unwrap().contains_key("data/x/one.txt"));
    // 目录整体重命名
    let renamed = e.vfs.rename(y.id, ItemId::ROOT, "z").await.unwrap();
    assert_eq!(renamed.id, y.id);
    assert_eq!(e.vfs.get(one).unwrap().remote_key, "data/z/uno.txt");
    assert!(e.vfs.list(x, true).await.unwrap().is_empty());
    e.vfs.delete(one).await.unwrap();
    assert!(e.vfs.get(one).is_err());
    let (changes, _) = e.vfs.changes_since(ChangeAnchor(0)).unwrap();
    assert!(changes
        .iter()
        .any(|c| matches!(c, Change::Delete(d) if *d == one)));
}

fn setup_with(mem: Arc<Mem>, cfg: MountConfig) -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let e = Engine::open(cfg, mem, dir.path.join("data"), dir.path.join("cache")).unwrap();
    (e, dir)
}

async fn seed(mem: &Mem, keys: &[&str], size: usize) {
    for k in keys {
        mem.put(k, Bytes::from(vec![7u8; size]), Default::default())
            .await
            .unwrap();
    }
}

const H: std::time::Duration = std::time::Duration::from_secs(3600);

#[tokio::test]
async fn ttl_eviction_respects_inheritance_and_pins() {
    let mem = Arc::new(Mem::default());
    seed(
        &mem,
        &[
            "data/a.bin",
            "data/keep/b.bin",
            "data/hot/c.bin",
            "data/pin.bin",
        ],
        3000,
    )
    .await;
    // 默认策略：1 小时 TTL
    let (e, _d) = setup_with(mem, config_with(1024, CachePolicy::Ttl(H), 1 << 40));
    let a = resolve(&e, "a.bin").await;
    let keep = resolve(&e, "keep").await;
    let b = resolve(&e, "keep/b.bin").await;
    let hot = resolve(&e, "hot").await;
    let c = resolve(&e, "hot/c.bin").await;
    let pin = resolve(&e, "pin.bin").await;
    // keep/ 目录覆盖为 24 小时；hot/ 覆盖为 None（读完即回收）
    e.set_policy(keep, Some(CachePolicy::Ttl(24 * H)))
        .await
        .unwrap();
    e.set_policy(hot, Some(CachePolicy::None)).await.unwrap();
    for id in [a, b, c] {
        e.download(id).await.unwrap();
    }
    e.pin(pin).await.unwrap();
    assert_eq!(
        e.effective_policy(b).unwrap(),
        CachePolicy::Ttl(24 * H),
        "inherits from parent dir"
    );
    assert_eq!(e.effective_policy(pin).unwrap(), CachePolicy::Always);

    let now = SystemTime::now();
    // 30 秒后：都不驱逐（None 有 60 秒宽限期）
    let r = e
        .maintain_at(now + std::time::Duration::from_secs(30))
        .await
        .unwrap();
    assert_eq!(r.ttl_evicted, 0);
    // 2 分钟后：hot/c.bin 被回收
    let r = e
        .maintain_at(now + std::time::Duration::from_secs(120))
        .await
        .unwrap();
    assert_eq!(r.ttl_evicted, 1);
    assert_eq!(e.vfs.get(c).unwrap().local, LocalState::Cloud);
    // 2 小时后：a.bin 过期；keep/b.bin（24h）与 pin.bin 保留
    let r = e.maintain_at(now + 2 * H).await.unwrap();
    assert_eq!(r.ttl_evicted, 1);
    assert_eq!(e.vfs.get(a).unwrap().local, LocalState::Cloud);
    assert_eq!(e.vfs.get(b).unwrap().local, LocalState::Cached);
    assert_eq!(e.stat(pin).unwrap().local, LocalState::Pinned);
    // 25 小时后：keep/b.bin 也过期；pin.bin 仍在
    e.maintain_at(now + 25 * H).await.unwrap();
    assert_eq!(e.vfs.get(b).unwrap().local, LocalState::Cloud);
    assert_eq!(
        e.vfs
            .content()
            .local_state(pin, &e.vfs.get(pin).unwrap().cache_key(), 3000)
            .unwrap(),
        LocalState::Cached
    );
    // 取消固定后按默认 TTL 驱逐
    e.unpin(pin).unwrap();
    assert_eq!(e.effective_policy(pin).unwrap(), CachePolicy::Ttl(H));
    let r = e.maintain_at(now + 2 * H).await.unwrap();
    assert_eq!(r.ttl_evicted, 1);
    assert_eq!(e.vfs.get(pin).unwrap().local, LocalState::Cloud);
}

#[tokio::test]
async fn capacity_eviction_is_lru_and_skips_dirty_and_pinned() {
    let mem = Arc::new(Mem::default());
    seed(
        &mem,
        &["data/1.bin", "data/2.bin", "data/3.bin", "data/4.bin"],
        4096,
    )
    .await;
    // 上限 10 KiB，四个 4 KiB 文件
    let (e, _d) = setup_with(mem, config_with(1024, CachePolicy::Ttl(24 * H), 10 * 1024));
    let ids: Vec<_> = futures_join(&e, &["1.bin", "2.bin", "3.bin", "4.bin"]).await;
    e.pin(ids[0]).await.unwrap(); // 最旧，但固定
    for id in &ids[1..] {
        e.download(*id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(1100)).await; // last_used 精度为秒
    }
    // 2.bin 有本地修改：不能驱逐
    let p = e.writeback.open_for_write(ids[1]).await.unwrap();
    std::fs::write(&p, b"local edit").unwrap();
    let r = e.maintain().await.unwrap();
    // 总量 16 KiB > 10 KiB：跳过 1（固定）与 2（脏），驱逐最旧的 3，降到 12 KiB 仍超，再驱逐 4
    assert_eq!(r.capacity_evicted, 2);
    assert_eq!(e.vfs.get(ids[2]).unwrap().local, LocalState::Cloud);
    assert_eq!(e.vfs.get(ids[3]).unwrap().local, LocalState::Cloud);
    assert_eq!(e.stat(ids[1]).unwrap().local, LocalState::Dirty);
    assert!(
        e.evict(ids[1]).await.is_err(),
        "manual evict of dirty file refused"
    );
}

#[tokio::test]
async fn pinned_directory_prefetches_new_children() {
    let mem = Arc::new(Mem::default());
    seed(&mem, &["data/p/x.bin"], 2000).await;
    let cfg = {
        let mut c = config_with(1024, CachePolicy::Ttl(H), 1 << 40);
        c.cache.as_mut().unwrap().dir_ttl = std::time::Duration::ZERO; // 每次维护都重新列举
        c
    };
    let (e, _d) = setup_with(mem.clone(), cfg);
    let p = resolve(&e, "p").await;
    e.pin(p).await.unwrap();
    let x = resolve(&e, "p/x.bin").await;
    assert_eq!(
        e.vfs.get(x).unwrap().local,
        LocalState::Cached,
        "existing child downloaded on pin"
    );
    // 远端新增文件，维护时自动预取
    seed(&mem, &["data/p/sub/y.bin"], 2000).await;
    let r = e.maintain().await.unwrap();
    assert_eq!(r.prefetched, 1);
    let y = resolve(&e, "p/sub/y.bin").await;
    assert_eq!(e.vfs.get(y).unwrap().local, LocalState::Cached);
    assert_eq!(e.effective_policy(y).unwrap(), CachePolicy::Always);
    // 固定目录下的文件不被 TTL 驱逐
    let r = e.maintain_at(SystemTime::now() + 48 * H).await.unwrap();
    assert_eq!(r.ttl_evicted, 0);
}

async fn futures_join(e: &Engine, names: &[&str]) -> Vec<ItemId> {
    let mut out = Vec::new();
    for n in names {
        out.push(resolve(e, n).await);
    }
    out
}

#[tokio::test]
async fn system_owned_copies_export_import_and_ttl_candidates() {
    let mem = Arc::new(Mem::default());
    let body: Vec<u8> = (0..5000u32).map(|i| (i % 13) as u8).collect();
    mem.put("data/f.bin", Bytes::from(body.clone()), Default::default())
        .await
        .unwrap();
    mem.put("data/k/g.bin", Bytes::from_static(b"g"), Default::default())
        .await
        .unwrap();
    let (e, d) = setup_with(mem.clone(), config_with(1024, CachePolicy::Ttl(H), 1 << 40));
    let f = resolve(&e, "f.bin").await;
    let k = resolve(&e, "k").await;
    let g = resolve(&e, "k/g.bin").await;

    // 导出：内容正确，且导出后块缓存被清掉（系统持有副本）
    let out = d.path.join("export/f.bin");
    e.export_to(f, &out, false).await.unwrap();
    assert_eq!(std::fs::read(&out).unwrap(), body);
    assert_eq!(e.vfs.get(f).unwrap().local, LocalState::Cloud);
    e.pin(k).await.unwrap();
    e.export_to(g, &d.path.join("export/g.bin"), false)
        .await
        .unwrap();
    assert_eq!(
        e.vfs.get(g).unwrap().local,
        LocalState::Cached,
        "pinned content keeps its blocks"
    );

    // TTL 候选：1 小时后 f 到期，固定目录下的 g 不到期
    let now = SystemTime::now();
    assert!(e.eviction_candidates(now).unwrap().is_empty());
    assert_eq!(e.eviction_candidates(now + 2 * H).unwrap(), vec![f]);
    // 系统上报的集合会包含根容器与目录：它们不能成为驱逐候选
    e.vfs
        .meta()
        .sync_materialized(&[ItemId::ROOT, k, f, g])
        .unwrap();
    assert_eq!(e.eviction_candidates(now + 2 * H).unwrap(), vec![f]);
    // 系统报告的集合中没有 f（已被系统驱逐）
    e.vfs.meta().sync_materialized(&[g]).unwrap();
    assert!(e.eviction_candidates(now + 2 * H).unwrap().is_empty());

    // 导入：用外部文件替换内容并上传
    let src = d.path.join("new.txt");
    std::fs::write(&src, b"replaced").unwrap();
    let n = e.import_and_upload(f, &src).await.unwrap();
    assert_eq!(n.size, 8);
    assert_eq!(
        &mem.objects.lock().unwrap()["data/f.bin"].0[..],
        b"replaced"
    );
    let out2 = d.path.join("export/f2.bin");
    e.export_to(f, &out2, false).await.unwrap();
    assert_eq!(std::fs::read(&out2).unwrap(), b"replaced");
}

#[tokio::test]
async fn conflict_creates_conflicted_copy_and_keeps_remote() {
    let mem = Arc::new(Mem::default());
    mem.put(
        "data/doc.txt",
        Bytes::from_static(b"v1"),
        Default::default(),
    )
    .await
    .unwrap();
    let (e, d) = setup_with(mem.clone(), config_with(1024, CachePolicy::Ttl(H), 1 << 40));
    let id = resolve(&e, "doc.txt").await;
    // 别处把远端改成 v2
    mem.put(
        "data/doc.txt",
        Bytes::from_static(b"v2-remote"),
        Default::default(),
    )
    .await
    .unwrap();
    // 我们基于 v1 的本地编辑上传 → If-Match 不匹配 → 冲突
    let src = d.path.join("local.txt");
    std::fs::write(&src, b"v2-local").unwrap();
    let n = e.import_and_upload(id, &src).await.unwrap();
    assert_eq!(n.id, id);
    assert_eq!(n.size, 9, "original item now reflects the remote version");
    assert_eq!(&e.read(id, 0..9).await.unwrap()[..], b"v2-remote");
    let children = e.vfs.list(ItemId::ROOT, false).await.unwrap();
    let copy = children
        .iter()
        .find(|c| c.name.starts_with("doc (conflicted copy ") && c.name.ends_with(").txt"))
        .expect("conflicted copy exists");
    assert_eq!(
        &mem.objects.lock().unwrap()[&copy.remote_key].0[..],
        b"v2-local"
    );
    assert_eq!(
        &mem.objects.lock().unwrap()["data/doc.txt"].0[..],
        b"v2-remote"
    );
    assert!(e.vfs.meta().uploads_pending().unwrap().is_empty());
}

#[tokio::test]
async fn refresh_hot_dirs_discovers_remote_changes() {
    let mem = Arc::new(Mem::default());
    mem.put("data/a/x.txt", Bytes::from_static(b"x"), Default::default())
        .await
        .unwrap();
    mem.put("data/b/y.txt", Bytes::from_static(b"y"), Default::default())
        .await
        .unwrap();
    let (e, _d) = setup_with(mem.clone(), config_with(1024, CachePolicy::Ttl(H), 1 << 40));
    let a = resolve(&e, "a").await;
    e.vfs.list(a, false).await.unwrap(); // a 是热目录；b 只被 lookup 过，未列举
    let _b = resolve(&e, "b").await;
    mem.put(
        "data/a/new.txt",
        Bytes::from_static(b"n"),
        Default::default(),
    )
    .await
    .unwrap();
    mem.put(
        "data/b/new.txt",
        Bytes::from_static(b"n"),
        Default::default(),
    )
    .await
    .unwrap();
    let changes = e
        .refresh_hot_dirs(std::time::Duration::from_secs(600))
        .await
        .unwrap();
    assert!(changes >= 1, "a/new.txt discovered");
    assert!(e.vfs.meta().lookup(a, "new.txt").unwrap().is_some());
    let b = resolve(&e, "b").await;
    assert!(
        e.vfs.meta().lookup(b, "new.txt").unwrap().is_none(),
        "cold dir b not refreshed"
    );
    assert_eq!(
        e.refresh_hot_dirs(std::time::Duration::from_secs(600))
            .await
            .unwrap(),
        0,
        "no spurious changes"
    );
}

#[test]
fn conflict_name_format() {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_792_157_405); // 2026-10-16 13:30:05 UTC
    assert_eq!(
        ndisk_core::engine::conflict_name("report.txt", t),
        "report (conflicted copy 2026-10-16 13-30-05).txt"
    );
    assert_eq!(
        ndisk_core::engine::conflict_name("README", t),
        "README (conflicted copy 2026-10-16 13-30-05)"
    );
    assert_eq!(
        ndisk_core::engine::conflict_name(".env", t),
        ".env (conflicted copy 2026-10-16 13-30-05)"
    );
}

#[test]
fn global_settings_apply_and_overrides() {
    use ndisk_core::config::*;
    let mut g = GlobalSettings::default();
    g.cache.max_bytes = 30 << 30;
    g.cache.policy = CachePolicy::Ttl(H);
    g.network.proxy = Some("env".into());
    let toml = r#"
id = "m"
display_name = "M"
read_only = false
[backend]
type = "s3"
bucket = "b"
[backend.auth]
method = "default"
[overrides]
policy = { mode = "always" }
"#;
    let m = MountConfig::from_toml(toml).unwrap().apply(&g, 3);
    assert_eq!(
        m.cache().max_bytes,
        10 << 30,
        "global cap shared by 3 mounts"
    );
    assert_eq!(m.cache().policy, CachePolicy::Always, "mount override wins");
    assert_eq!(m.network().proxy.as_deref(), Some("env"));
    // 无覆盖时用全局策略
    let m2 = MountConfig::from_toml(toml.replace("policy = { mode = \"always\" }", "").as_str())
        .unwrap()
        .apply(&g, 1);
    assert_eq!(m2.cache().policy, CachePolicy::Ttl(H));
    // 往返：overrides 为空时不写出
    assert!(!m2.to_toml().unwrap().contains("[overrides]"));
    assert!(GlobalSettings::from_toml(&g.to_toml().unwrap())
        .unwrap()
        .network
        .proxy
        .is_some());
}
