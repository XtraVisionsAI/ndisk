//! 内容缓存：按块落盘。
//!
//! 布局：`<root>/<mount_id>/blocks/<item_id>/<etag_hex>/<block_no>`，
//! 脏文件：`<root>/<mount_id>/dirty/<item_id>`。
//! 任意 etag 变化即整体失效；块齐全即视为"已缓存"，该状态驱动 Finder 角标。

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::cache::meta::MetaStore;
use crate::types::*;
use crate::Result;

pub struct ContentCache {
    root: PathBuf,
    block_size: u64,
    meta: Arc<MetaStore>,
}

impl ContentCache {
    pub fn new(root: PathBuf, block_size: u64, meta: Arc<MetaStore>) -> Result<Self> {
        std::fs::create_dir_all(root.join("blocks"))?;
        std::fs::create_dir_all(root.join("dirty"))?;
        Ok(Self {
            root,
            block_size,
            meta,
        })
    }

    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    pub fn block_range(&self, block_no: u64, file_size: u64) -> Range<u64> {
        let start = block_no * self.block_size;
        start..(start + self.block_size).min(file_size)
    }

    pub fn block_count(&self, file_size: u64) -> u64 {
        if file_size == 0 {
            0
        } else {
            file_size.div_ceil(self.block_size)
        }
    }

    fn block_path(&self, id: ItemId, etag: &str, block_no: u64) -> PathBuf {
        self.root
            .join("blocks")
            .join(id.0.to_string())
            .join(hex(etag))
            .join(block_no.to_string())
    }

    pub fn dirty_path(&self, id: ItemId) -> PathBuf {
        self.root.join("dirty").join(id.0.to_string())
    }

    pub fn has_block(&self, id: ItemId, etag: &str, block_no: u64) -> bool {
        self.block_path(id, etag, block_no).is_file()
    }

    pub async fn read_block(&self, id: ItemId, etag: &str, block_no: u64) -> Result<Option<Bytes>> {
        let p = self.block_path(id, etag, block_no);
        match tokio::fs::read(&p).await {
            Ok(v) => {
                self.meta.touch_block(id, etag, block_no)?;
                Ok(Some(Bytes::from(v)))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 原子写入一个块（先写临时文件再 rename）。
    pub async fn write_block(
        &self,
        id: ItemId,
        etag: &str,
        block_no: u64,
        data: &[u8],
    ) -> Result<()> {
        let p = self.block_path(id, etag, block_no);
        if let Some(dir) = p.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        let tmp = p.with_extension("tmp");
        let mut f = tokio::fs::File::create(&tmp).await?;
        f.write_all(data).await?;
        f.sync_data().await?;
        tokio::fs::rename(&tmp, &p).await?;
        self.meta
            .record_block(id, etag, block_no, data.len() as u64)?;
        Ok(())
    }

    /// 计算本地状态：Cloud / Partial / Cached（不含 Dirty/Uploading，由 writeback 层叠加）。
    pub fn local_state(&self, id: ItemId, version: &str, size: u64) -> Result<LocalState> {
        let have = self.meta.cached_blocks(id, version)?.len() as u64;
        let need = self.block_count(size);
        Ok(if have == 0 && need > 0 {
            LocalState::Cloud
        } else if have >= need {
            LocalState::Cached
        } else {
            LocalState::Partial
        })
    }

    /// 删除某节点的所有本地块（所有 etag 版本）。
    pub async fn evict(&self, id: ItemId) -> Result<()> {
        let dir = self.root.join("blocks").join(id.0.to_string());
        if dir.exists() {
            tokio::fs::remove_dir_all(&dir).await?;
        }
        self.meta.forget_blocks(id)?;
        Ok(())
    }

    /// 清理旧 etag 的块，只保留 `keep_etag`。
    pub async fn drop_stale_versions(&self, id: ItemId, keep_etag: &str) -> Result<()> {
        let dir = self.root.join("blocks").join(id.0.to_string());
        let keep = hex(keep_etag);
        let mut rd = match tokio::fs::read_dir(&dir).await {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        while let Some(ent) = rd.next_entry().await? {
            if ent.file_name().to_string_lossy() != keep {
                tokio::fs::remove_dir_all(ent.path()).await?;
            }
        }
        Ok(())
    }

    // ---- dirty files（写路径）----

    /// 把已缓存的块拼成完整脏文件，作为写入起点。要求所有块齐全。
    pub async fn materialize_dirty(&self, id: ItemId, etag: &str, size: u64) -> Result<PathBuf> {
        let dst = self.dirty_path(id);
        let mut out = tokio::fs::File::create(&dst).await?;
        for b in 0..self.block_count(size) {
            let data = tokio::fs::read(self.block_path(id, etag, b)).await?;
            out.write_all(&data).await?;
        }
        out.flush().await?;
        Ok(dst)
    }

    pub async fn read_dirty(&self, id: ItemId, range: Range<u64>) -> Result<Bytes> {
        let mut f = tokio::fs::File::open(self.dirty_path(id)).await?;
        f.seek(std::io::SeekFrom::Start(range.start)).await?;
        let mut buf = vec![0u8; (range.end - range.start) as usize];
        let n = read_full(&mut f, &mut buf).await?;
        buf.truncate(n);
        Ok(Bytes::from(buf))
    }

    pub async fn remove_dirty(&self, id: ItemId) -> Result<()> {
        match tokio::fs::remove_file(self.dirty_path(id)).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.into()),
        }
    }
}

async fn read_full(f: &mut tokio::fs::File, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = f.read(&mut buf[n..]).await?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// etag 可能含引号与 `-`，用 hex 作为目录名最稳妥。
fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

#[allow(dead_code)]
fn _assert_path(_: &Path) {}
