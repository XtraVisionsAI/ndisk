//! fuser 实现骨架。M1 阶段补全。

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyEntry, Request};
use ndisk_core::{Engine, ItemId, Kind, Node};

const TTL: Duration = Duration::from_secs(1);

pub struct NdiskFs {
    engine: Arc<Engine>,
    rt: tokio::runtime::Handle,
}

impl NdiskFs {
    fn attr(&self, n: &Node) -> FileAttr {
        let now = std::time::SystemTime::now();
        FileAttr {
            ino: n.id.0,
            size: n.size,
            blocks: n.size.div_ceil(512),
            atime: now,
            mtime: n.mtime.unwrap_or(now),
            ctime: n.mtime.unwrap_or(now),
            crtime: n.mtime.unwrap_or(now),
            kind: if n.kind == Kind::Dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if n.kind == Kind::Dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }
}

impl Filesystem for NdiskFs {
    fn lookup(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &std::ffi::OsStr,
        reply: ReplyEntry,
    ) {
        let name = name.to_string_lossy().to_string();
        let engine = self.engine.clone();
        match self.rt.block_on(engine.vfs.lookup(ItemId(parent), &name)) {
            Ok(n) => reply.entry(&TTL, &self.attr(&n), 0),
            Err(e) => reply.error(e.to_errno()),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.engine.vfs.get(ItemId(ino)) {
            Ok(n) => reply.attr(&TTL, &self.attr(&n)),
            Err(e) => reply.error(e.to_errno()),
        }
    }

    // TODO(M1): readdir / open / read / write / release / create / mkdir / unlink / rmdir / rename / setattr
}

pub fn mount(engine: Arc<Engine>, mountpoint: &Path) -> ndisk_core::Result<()> {
    let rt = tokio::runtime::Handle::current();
    let fs = NdiskFs { engine, rt };
    let opts = vec![
        fuser::MountOption::FSName("ndisk".into()),
        fuser::MountOption::DefaultPermissions,
        fuser::MountOption::NoAtime,
    ];
    fuser::mount2(fs, mountpoint, &opts).map_err(ndisk_core::Error::Io)
}
