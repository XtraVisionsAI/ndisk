//! 写回与断点续传。
//!
//! 写入永远先落到脏文件；close 后登记到 `uploads` 表并排队。分片上传每完成一个 part
//! 就持久化进度，进程重启后 `resume_all` 用同一个 upload_id 继续。

use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::cache::meta::{UploadRecord, UploadState};
use crate::config::CacheConfig;
use crate::sched::{retry, Scheduler};
use crate::types::*;
use crate::vfs::Vfs;
use crate::{Error, Result};

pub struct Writeback {
    vfs: Arc<Vfs>,
    sched: Scheduler,
    cfg: CacheConfig,
}

impl Writeback {
    pub fn new(vfs: Arc<Vfs>, sched: Scheduler, cfg: CacheConfig) -> Self {
        Self { vfs, sched, cfg }
    }

    /// 打开文件准备写入：确保脏文件存在（远端有内容则先拉全量）。
    pub async fn open_for_write(&self, id: ItemId) -> Result<std::path::PathBuf> {
        let n = self.vfs.get(id)?;
        let path = self.vfs.content().dirty_path(id);
        if path.exists() {
            return Ok(path);
        }
        match &n.etag {
            Some(etag) if n.size > 0 => {
                crate::engine::ensure_blocks(
                    &self.vfs,
                    &self.sched,
                    &n,
                    0..self.vfs.content().block_count(n.size),
                )
                .await?;
                self.vfs
                    .content()
                    .materialize_dirty(id, etag, n.size)
                    .await?;
            }
            _ => {
                tokio::fs::File::create(&path).await?;
            }
        }
        Ok(path)
    }

    /// close 后调用：登记上传任务。真正的上传由 `flush` / `resume_all` 驱动。
    pub async fn enqueue(&self, id: ItemId) -> Result<()> {
        let n = self.vfs.get(id)?;
        let size = tokio::fs::metadata(self.vfs.content().dirty_path(id))
            .await?
            .len();
        // 若已有进行中的分片上传，先中止（内容变了）。
        if let Some(old) = self.vfs.meta().upload_get(id)? {
            if let Some(up) = &old.upload_id {
                let _ = self
                    .vfs
                    .backend()
                    .multipart_abort(&old.remote_key, up)
                    .await;
            }
        }
        let rec = UploadRecord {
            item_id: id,
            remote_key: n.remote_key.clone(),
            upload_id: None,
            part_size: self.cfg.part_size,
            parts_done: vec![],
            base_etag: n.etag.clone(),
            state: UploadState::Queued,
        };
        self.vfs.meta().upload_put(&rec)?;
        tracing::debug!(item = id.0, size, "enqueued upload");
        Ok(())
    }

    /// 执行一个节点的上传（可被重复调用以续传）。
    pub async fn flush(&self, id: ItemId) -> Result<Entry> {
        let mut rec = self
            .vfs
            .meta()
            .upload_get(id)?
            .ok_or_else(|| Error::NotFound(format!("upload {}", id.0)))?;
        let path = self.vfs.content().dirty_path(id);
        let size = tokio::fs::metadata(&path).await?.len();
        let _permit = self.sched.upload.acquire().await.expect("semaphore closed");
        rec.state = UploadState::Uploading;
        self.vfs.meta().upload_put(&rec)?;

        let backend = self.vfs.backend().clone();
        let opts = PutOptions {
            if_match: rec.base_etag.clone(),
            ..Default::default()
        };

        let result: Result<Entry> =
            if size < self.cfg.multipart_threshold || !backend.capabilities().multipart {
                self.sched.upload_limit.acquire(size).await;
                let data = Bytes::from(tokio::fs::read(&path).await?);
                retry(5, || {
                    backend.put(&rec.remote_key, data.clone(), opts.clone())
                })
                .await
            } else {
                self.multipart(&mut rec, &path, size, opts).await
            };

        match result {
            Ok(entry) => {
                let n = self.vfs.get(id)?;
                self.vfs
                    .meta()
                    .upsert(n.parent, &n.name, &entry, &entry.key)?;
                self.vfs.meta().upload_remove(id)?;
                // 用脏文件内容直接填充新 etag 的块缓存，避免重复下载。
                if let Some(etag) = &entry.etag {
                    self.promote_dirty_to_blocks(id, etag, &path, size).await?;
                    self.vfs.content().drop_stale_versions(id, etag).await?;
                }
                self.vfs.content().remove_dirty(id).await?;
                Ok(entry)
            }
            Err(e) => {
                rec.state = UploadState::Failed;
                self.vfs.meta().upload_put(&rec)?;
                Err(e)
            }
        }
    }

    async fn multipart(
        &self,
        rec: &mut UploadRecord,
        path: &std::path::Path,
        size: u64,
        opts: PutOptions,
    ) -> Result<Entry> {
        let backend = self.vfs.backend().clone();
        let upload_id = match &rec.upload_id {
            Some(u) => u.clone(),
            None => {
                let u = backend.multipart_start(&rec.remote_key, opts).await?;
                rec.upload_id = Some(u.clone());
                rec.parts_done.clear();
                self.vfs.meta().upload_put(rec)?;
                u
            }
        };
        let part_size = rec.part_size;
        let total_parts = size.div_ceil(part_size) as u32;
        let mut f = tokio::fs::File::open(path).await?;
        for part_no in 1..=total_parts {
            if rec.parts_done.iter().any(|(p, _)| *p == part_no) {
                continue;
            }
            let start = (part_no as u64 - 1) * part_size;
            let len = (size - start).min(part_size);
            f.seek(std::io::SeekFrom::Start(start)).await?;
            let mut buf = vec![0u8; len as usize];
            f.read_exact(&mut buf).await?;
            self.sched.upload_limit.acquire(len).await;
            let data = Bytes::from(buf);
            let etag = retry(5, || {
                backend.multipart_put_part(&rec.remote_key, &upload_id, part_no, data.clone())
            })
            .await?;
            rec.parts_done.push((part_no, etag));
            self.vfs.meta().upload_put(rec)?; // 进度落盘，断电后可续传
        }
        rec.parts_done.sort_by_key(|(p, _)| *p);
        backend
            .multipart_complete(&rec.remote_key, &upload_id, &rec.parts_done)
            .await
    }

    async fn promote_dirty_to_blocks(
        &self,
        id: ItemId,
        etag: &str,
        path: &std::path::Path,
        size: u64,
    ) -> Result<()> {
        let content = self.vfs.content();
        let mut f = tokio::fs::File::open(path).await?;
        for b in 0..content.block_count(size) {
            let r = content.block_range(b, size);
            let mut buf = vec![0u8; (r.end - r.start) as usize];
            f.read_exact(&mut buf).await?;
            content.write_block(id, etag, b, &buf).await?;
        }
        Ok(())
    }

    /// 启动时调用：恢复所有未完成上传。
    pub async fn resume_all(&self) -> Result<Vec<(ItemId, Result<Entry>)>> {
        let mut out = Vec::new();
        for rec in self.vfs.meta().uploads_pending()? {
            let r = self.flush(rec.item_id).await;
            out.push((rec.item_id, r));
        }
        Ok(out)
    }
}
