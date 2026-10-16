//! Linux FUSE 适配层。inode 即 `ItemId`。
//!
//! 只在 Linux 编译真正的 FUSE 实现；其他平台保留签名以便 workspace 统一构建。
//! 映射关系：
//! - lookup/getattr/readdir → `Vfs::lookup` / `Engine::stat` / `Vfs::list`
//! - read → 已打开的脏文件优先，否则 `Engine::read`
//! - create/mkdir/unlink/rmdir/rename → `Vfs::*`
//! - open(write)/write/release → `Writeback::open_for_write` / 直接写脏文件 / `enqueue` + 后台 `flush`
//! - fsync → 同步 `flush`
//! - 错误 → `Error::to_errno`

#[cfg(target_os = "linux")]
pub mod fs;

#[cfg(target_os = "linux")]
pub use fs::mount;

/// 挂载选项。
#[derive(Debug, Clone, Default)]
pub struct MountOptions {
    /// 允许其他用户访问（需要 /etc/fuse.conf 中 user_allow_other）。
    pub allow_other: bool,
    /// 卸载后自动清理挂载点。
    pub auto_unmount: bool,
}

#[cfg(not(target_os = "linux"))]
pub fn mount(
    _engine: std::sync::Arc<ndisk_core::Engine>,
    _rt: tokio::runtime::Handle,
    _mountpoint: &std::path::Path,
    _opts: MountOptions,
) -> ndisk_core::Result<()> {
    Err(ndisk_core::Error::Unsupported(
        "FUSE adapter is only available on Linux",
    ))
}
