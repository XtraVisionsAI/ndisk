//! 存储后端抽象。后端只做对象级操作，不理解目录树与缓存。

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::types::*;
use crate::Result;

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// 便于日志与 UI 展示的后端名称，如 `s3`。
    fn name(&self) -> &'static str;

    fn capabilities(&self) -> Capabilities;

    /// 按前缀分页列举直接子项（相当于 `Delimiter=/`）。`prefix` 为空或以 `/` 结尾。
    async fn list(&self, prefix: &str, cursor: Option<&str>) -> Result<ListPage>;

    /// 不存在时返回 `Ok(None)`。对隐式目录（仅有前缀、无标记对象）也应尽量返回 `Dir`。
    async fn stat(&self, key: &str) -> Result<Option<Entry>>;

    async fn read(&self, key: &str, range: Range<u64>) -> Result<Bytes>;

    /// 整体写入小对象。
    async fn put(&self, key: &str, data: Bytes, opts: PutOptions) -> Result<Entry>;

    async fn multipart_start(&self, key: &str, opts: PutOptions) -> Result<UploadId>;
    async fn multipart_put_part(
        &self,
        key: &str,
        upload: &UploadId,
        part_number: u32,
        data: Bytes,
    ) -> Result<PartEtag>;
    async fn multipart_complete(
        &self,
        key: &str,
        upload: &UploadId,
        parts: &[(u32, PartEtag)],
    ) -> Result<Entry>;
    async fn multipart_abort(&self, key: &str, upload: &UploadId) -> Result<()>;

    async fn delete(&self, key: &str) -> Result<()>;

    /// 服务端复制。大对象由后端自行决定是否走分片复制。
    async fn copy(&self, from: &str, to: &str) -> Result<Entry>;

    /// 重命名/移动单个文件。默认实现为 copy + delete；有原生 rename 的后端应覆盖它
    /// （并声明 `native_rename`）。目录由上层逐个子项递归移动。
    async fn rename(&self, from: &str, to: &str) -> Result<Entry> {
        let e = self.copy(from, to).await?;
        self.delete(from).await?;
        Ok(e)
    }

    /// 从本地文件整体上传（不走 [`Backend::multipart_start`] 的分片续传路径时使用）。
    /// 默认读入内存后调用 `put`；能流式写入的后端应覆盖它，避免大文件占满内存。
    async fn put_file(&self, key: &str, path: &Path, opts: PutOptions) -> Result<Entry> {
        let data = Bytes::from(tokio::fs::read(path).await?);
        self.put(key, data, opts).await
    }

    /// 创建目录标记对象（如 S3 的零字节 `dir/`）。默认实现用 `put` 完成。
    async fn mkdir(&self, key: &str) -> Result<Entry> {
        let key = if key.ends_with('/') {
            key.to_string()
        } else {
            format!("{key}/")
        };
        self.put(&key, Bytes::new(), PutOptions::default()).await
    }
}

pub type DynBackend = Arc<dyn Backend>;
