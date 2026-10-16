use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// 本地稳定标识。File Provider 的 `itemIdentifier` 与 FUSE 的 inode 都映射到它，
/// 重命名/移动不改变 ItemId。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemId(pub u64);

impl ItemId {
    pub const ROOT: ItemId = ItemId(1);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    File,
    Dir,
}

/// 后端返回的一个对象/前缀。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// 后端中的完整键（S3 object key）。目录以 `/` 结尾。
    pub key: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    /// 缓存失效与冲突检测的唯一依据。
    pub etag: Option<String>,
    /// 支持版本控制的后端返回的版本号。
    pub version: Option<String>,
}

impl Entry {
    pub fn name(&self) -> &str {
        self.key
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("")
    }

    /// 内容缓存的版本键。优先 etag；部分 S3 兼容实现不返回 etag，退化为 mtime+size。
    pub fn cache_key(&self) -> String {
        cache_key(self.etag.as_deref(), self.mtime, self.size)
    }
}

pub(crate) fn cache_key(etag: Option<&str>, mtime: Option<SystemTime>, size: u64) -> String {
    match etag {
        Some(e) => e.to_string(),
        None => {
            let secs = mtime
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            format!("m{secs}s{size}")
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ListPage {
    pub entries: Vec<Entry>,
    /// 下一页游标；`None` 表示结束。
    pub next: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Capabilities {
    pub native_rename: bool,
    pub range_read: bool,
    pub multipart: bool,
    pub etag: bool,
    pub conditional_put: bool,
    pub versioning: bool,
}

#[derive(Debug, Clone, Default)]
pub struct PutOptions {
    /// 仅当远端 etag 等于该值时才写入（If-Match）。`Some("")` 表示要求对象不存在。
    pub if_match: Option<String>,
    pub content_type: Option<String>,
    pub mtime: Option<SystemTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartEtag(pub String);

/// 内容缓存策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode", content = "value")]
pub enum CachePolicy {
    /// 最后访问超过该时长后由后台驱逐器删除。
    Ttl(#[serde(with = "humantime_duration")] Duration),
    /// 固定：不驱逐，元数据变化时主动预取。
    Always,
    /// 读完即可回收。
    None,
}

impl Default for CachePolicy {
    fn default() -> Self {
        CachePolicy::Ttl(Duration::from_secs(7 * 24 * 3600))
    }
}

/// 文件在本地的缓存状态，驱动 Finder 角标与 CLI 输出。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalState {
    /// 仅云端，本地没有任何块。
    Cloud,
    /// 部分块已下载。
    Partial,
    /// 当前 etag 下所有块齐全。
    Cached,
    /// 本地有修改尚未上传。
    Dirty,
    /// 正在上传。
    Uploading,
    /// 被固定为始终缓存且已齐全。
    Pinned,
}

/// VFS 对外暴露的节点视图。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: ItemId,
    pub parent: ItemId,
    pub name: String,
    pub kind: Kind,
    pub size: u64,
    pub mtime: Option<SystemTime>,
    pub etag: Option<String>,
    pub remote_key: String,
    pub local: LocalState,
    pub policy: Option<CachePolicy>,
}

impl Node {
    /// 见 [`Entry::cache_key`]。
    pub fn cache_key(&self) -> String {
        cache_key(self.etag.as_deref(), self.mtime, self.size)
    }
}

/// 变更序号，供 File Provider `enumerateChanges(from:)` 使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ChangeAnchor(pub u64);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Change {
    Upsert(Node),
    Delete(ItemId),
}

mod humantime_duration {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_secs().serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}
