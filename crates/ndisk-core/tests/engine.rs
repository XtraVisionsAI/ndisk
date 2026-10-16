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
        }),
        cache: CacheConfig {
            block_size,
            multipart_threshold: 4096,
            part_size: 5 * 1024 * 1024,
            ..Default::default()
        },
        network: Default::default(),
        read_only: false,
    }
}

fn setup(mem: Arc<Mem>, block_size: u64) -> (Engine, tempdir::Dir) {
    let dir = tempdir::Dir::new();
    let e = Engine::open(config(block_size), mem, dir.path.clone()).unwrap();
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
