use std::io;

/// 核心引擎统一错误类型。
///
/// 平台层需要把这些错误映射为各自的系统错误：
/// FUSE 映射为 errno，File Provider 映射为 `NSFileProviderError`。
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("already exists: {0}")]
    AlreadyExists(String),
    #[error("not a directory: {0}")]
    NotADirectory(String),
    #[error("is a directory: {0}")]
    IsADirectory(String),
    #[error("directory not empty: {0}")]
    DirectoryNotEmpty(String),
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    /// 远端对象已被他人修改（ETag 不匹配，HTTP 412）。
    #[error("conflict on {key}: expected etag {expected:?}, remote has {actual:?}")]
    Conflict {
        key: String,
        expected: Option<String>,
        actual: Option<String>,
    },
    /// 当前离线且请求内容未缓存。
    #[error("offline and content not cached: {0}")]
    Offline(String),
    #[error("backend error: {0}")]
    Backend(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("config error: {0}")]
    Config(String),
    #[error("unsupported operation: {0}")]
    Unsupported(&'static str),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn backend<E>(e: E) -> Self
    where
        E: std::error::Error + Send + Sync + 'static,
    {
        Error::Backend(Box::new(e))
    }

    /// 是否值得按指数退避重试（网络抖动、5xx、限流）。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Backend(_) | Error::Io(_))
    }

    /// 映射为 POSIX errno，供 FUSE 层使用。
    pub fn to_errno(&self) -> i32 {
        match self {
            Error::NotFound(_) => libc_consts::ENOENT,
            Error::AlreadyExists(_) => libc_consts::EEXIST,
            Error::NotADirectory(_) => libc_consts::ENOTDIR,
            Error::IsADirectory(_) => libc_consts::EISDIR,
            Error::DirectoryNotEmpty(_) => libc_consts::ENOTEMPTY,
            Error::PermissionDenied(_) => libc_consts::EACCES,
            Error::Conflict { .. } => libc_consts::ESTALE,
            Error::Offline(_) => libc_consts::ENETDOWN,
            Error::Unsupported(_) => libc_consts::ENOSYS,
            Error::Backend(_) | Error::Io(_) | Error::Db(_) | Error::Config(_) => libc_consts::EIO,
        }
    }
}

/// 不依赖 libc crate 的 errno 常量（macOS 与 Linux 取值一致的子集）。
mod libc_consts {
    pub const ENOENT: i32 = 2;
    pub const EIO: i32 = 5;
    pub const EACCES: i32 = 13;
    pub const EEXIST: i32 = 17;
    pub const ENOTDIR: i32 = 20;
    pub const EISDIR: i32 = 21;
    pub const ENOSYS: i32 = 38;
    pub const ENOTEMPTY: i32 = 39;
    pub const ESTALE: i32 = 116;
    pub const ENETDOWN: i32 = 100;
}
