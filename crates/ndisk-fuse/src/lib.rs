//! Linux FUSE 适配层。inode 即 `ItemId`。
//!
//! 只在 Linux 编译真正的 FUSE 实现；其他平台保留类型签名以便 workspace 统一构建。
//! 映射关系：
//! - lookup/getattr/readdir → `Vfs::lookup/get/list`
//! - read → `Engine::read`
//! - create/mkdir/unlink/rmdir/rename → `Vfs::*`
//! - open(write)/write/release → `Writeback::open_for_write` / 直接写脏文件 / `Writeback::enqueue`
//! - 错误 → `Error::to_errno`

#[cfg(target_os = "linux")]
pub mod fs;

#[cfg(target_os = "linux")]
pub use fs::mount;

#[cfg(not(target_os = "linux"))]
pub fn mount(
    _engine: std::sync::Arc<ndisk_core::Engine>,
    _mountpoint: &std::path::Path,
) -> ndisk_core::Result<()> {
    Err(ndisk_core::Error::Unsupported(
        "FUSE adapter is only available on Linux",
    ))
}
