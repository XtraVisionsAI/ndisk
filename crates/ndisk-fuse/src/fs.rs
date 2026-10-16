//! fuser 实现。
//!
//! fuser 的回调是同步的，而引擎是 async：本模块在 FUSE 分发线程上用 `Handle::block_on`
//! 等待引擎结果，因此 `mount` 必须在 tokio runtime 之外的线程调用（CLI 用 `std::thread`）。

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fuser::{
    FileAttr, FileType, Filesystem, KernelConfig, MountOption, ReplyAttr, ReplyCreate, ReplyData,
    ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request, TimeOrNow,
};
use ndisk_core::{Engine, Error, ItemId, Kind, Node};

use crate::MountOptions;

const TTL: Duration = Duration::from_secs(1);
const BLOCK: u32 = 4096;

/// 一个打开的文件句柄。
struct Handle {
    id: ItemId,
    /// 以写模式打开时的脏文件；读模式为 None，读取走块缓存。
    dirty: Option<File>,
    written: bool,
}

pub struct NdiskFs {
    engine: Arc<Engine>,
    rt: tokio::runtime::Handle,
    handles: HashMap<u64, Handle>,
    next_fh: u64,
    uid: u32,
    gid: u32,
}

impl NdiskFs {
    fn attr(&self, n: &Node) -> FileAttr {
        let now = SystemTime::now();
        let t = n.mtime.unwrap_or(now);
        let dir = n.kind == Kind::Dir;
        FileAttr {
            ino: n.id.0,
            size: if dir { 0 } else { n.size },
            blocks: n.size.div_ceil(512),
            atime: t,
            mtime: t,
            ctime: t,
            crtime: t,
            kind: if dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            },
            perm: if dir { 0o755 } else { 0o644 },
            nlink: 1,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK,
            flags: 0,
        }
    }

    fn block<F, T>(&self, f: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        self.rt.block_on(f)
    }

    fn alloc_fh(&mut self, h: Handle) -> u64 {
        self.next_fh += 1;
        let fh = self.next_fh;
        self.handles.insert(fh, h);
        fh
    }

    fn name(name: &OsStr) -> Result<&str, i32> {
        name.to_str().ok_or(libc::EINVAL)
    }

    /// 以写模式打开：拿到脏文件（远端内容按需拉全量）。
    fn open_write(&self, id: ItemId, truncate: bool) -> Result<File, i32> {
        self.engine.ensure_writable().map_err(|e| e.to_errno())?;
        let engine = self.engine.clone();
        let path = self
            .block(engine.writeback.open_for_write(id))
            .map_err(|e| e.to_errno())?;
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .truncate(truncate)
            .open(&path)
            .map_err(io_errno)?;
        Ok(f)
    }

    /// 写入结束：登记写回并在后台上传。
    fn finish_write(&self, id: ItemId) -> Result<(), i32> {
        let engine = self.engine.clone();
        self.block(engine.writeback.enqueue(id))
            .map_err(|e| e.to_errno())?;
        self.engine.spawn_flush(id);
        Ok(())
    }
}

fn io_errno(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(libc::EIO)
}

fn errno(e: Error) -> i32 {
    e.to_errno()
}

impl Filesystem for NdiskFs {
    fn init(&mut self, _req: &Request<'_>, config: &mut KernelConfig) -> Result<(), libc::c_int> {
        // 允许更大的读写请求，减少回调次数。
        let _ = config.set_max_write(1024 * 1024);
        let _ = config.set_max_readahead(1024 * 1024);
        Ok(())
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name = match Self::name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let engine = self.engine.clone();
        match self
            .block(engine.vfs.lookup(ItemId(parent), name))
            .and_then(|n| engine.stat(n.id))
        {
            Ok(n) => reply.entry(&TTL, &self.attr(&n), 0),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match self.engine.stat(ItemId(ino)) {
            Ok(n) => reply.attr(&TTL, &self.attr(&n)),
            Err(e) => reply.error(errno(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<TimeOrNow>,
        _mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<u64>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let id = ItemId(ino);
        if let Some(size) = size {
            // truncate：优先用已打开的写句柄，否则临时打开一次写会话。
            let via_handle = fh
                .and_then(|fh| self.handles.get_mut(&fh))
                .filter(|h| h.dirty.is_some());
            let r = match via_handle {
                Some(h) => {
                    h.written = true;
                    h.dirty.as_ref().unwrap().set_len(size).map_err(io_errno)
                }
                None => self
                    .open_write(id, false)
                    .and_then(|f| f.set_len(size).map_err(io_errno))
                    .and_then(|_| self.finish_write(id)),
            };
            if let Err(e) = r {
                return reply.error(e);
            }
        }
        // mode/uid/gid/时间：对象存储不保存，直接接受并返回当前属性。
        match self.engine.stat(id) {
            Ok(n) => reply.attr(&TTL, &self.attr(&n)),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        if let Err(e) = self.engine.ensure_writable() {
            return reply.error(errno(e));
        }
        let name = match Self::name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let engine = self.engine.clone();
        match self.block(engine.vfs.mkdir(ItemId(parent), name)) {
            Ok(n) => reply.entry(&TTL, &self.attr(&n), 0),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, Kind::File, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        self.remove(parent, name, Kind::Dir, reply);
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        flags: u32,
        reply: ReplyEmpty,
    ) {
        if let Err(e) = self.engine.ensure_writable() {
            return reply.error(errno(e));
        }
        let (name, newname) = match (Self::name(name), Self::name(newname)) {
            (Ok(a), Ok(b)) => (a, b),
            _ => return reply.error(libc::EINVAL),
        };
        let engine = self.engine.clone();
        let r = self.block(async {
            let src = engine.vfs.lookup(ItemId(parent), name).await?;
            // 目标已存在：POSIX 要求覆盖（RENAME_NOREPLACE 除外）。
            if let Ok(dst) = engine.vfs.lookup(ItemId(newparent), newname).await {
                if flags & libc::RENAME_NOREPLACE != 0 {
                    return Err(Error::AlreadyExists(newname.to_string()));
                }
                if dst.id != src.id {
                    engine.vfs.delete(dst.id).await?;
                }
            }
            engine
                .vfs
                .rename(src.id, ItemId(newparent), newname)
                .await
                .map(|_| ())
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let id = ItemId(ino);
        let acc = flags & libc::O_ACCMODE;
        let writing = acc == libc::O_WRONLY || acc == libc::O_RDWR;
        let dirty = if writing {
            match self.open_write(id, flags & libc::O_TRUNC != 0) {
                Ok(f) => Some(f),
                Err(e) => return reply.error(e),
            }
        } else {
            match self.engine.stat(id) {
                Ok(n) if n.kind == Kind::Dir => return reply.error(libc::EISDIR),
                Ok(_) => None,
                Err(e) => return reply.error(errno(e)),
            }
        };
        let fh = self.alloc_fh(Handle {
            id,
            dirty,
            written: flags & libc::O_TRUNC != 0,
        });
        reply.opened(fh, 0);
    }

    #[allow(clippy::too_many_arguments)]
    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let offset = offset.max(0) as u64;
        if let Some(f) = self.handles.get(&fh).and_then(|h| h.dirty.as_ref()) {
            let mut buf = vec![0u8; size as usize];
            let mut n = 0;
            while n < buf.len() {
                match f.read_at(&mut buf[n..], offset + n as u64) {
                    Ok(0) => break,
                    Ok(k) => n += k,
                    Err(e) => return reply.error(io_errno(e)),
                }
            }
            return reply.data(&buf[..n]);
        }
        let engine = self.engine.clone();
        match self.block(engine.read(ItemId(ino), offset..offset + size as u64)) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(errno(e)),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let Some(h) = self.handles.get_mut(&fh) else {
            return reply.error(libc::EBADF);
        };
        let Some(f) = h.dirty.as_ref() else {
            return reply.error(libc::EBADF);
        };
        match f.write_all_at(data, offset.max(0) as u64) {
            Ok(()) => {
                h.written = true;
                reply.written(data.len() as u32)
            }
            Err(e) => reply.error(io_errno(e)),
        }
    }

    fn flush(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _lock_owner: u64,
        reply: ReplyEmpty,
    ) {
        // close() 时的 flush：数据已在本地脏文件，不阻塞等待上传。
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let Some(h) = self.handles.remove(&fh) else {
            return reply.error(libc::EBADF);
        };
        if let Some(f) = h.dirty {
            drop(f);
            if h.written {
                if let Err(e) = self.finish_write(h.id) {
                    return reply.error(e);
                }
            }
        }
        reply.ok();
    }

    fn fsync(&mut self, _req: &Request<'_>, ino: u64, fh: u64, _datasync: bool, reply: ReplyEmpty) {
        // fsync 语义：返回时数据必须已持久化到远端。
        let id = ItemId(ino);
        let written = self.handles.get(&fh).map(|h| h.written).unwrap_or(false);
        if written {
            if let Some(f) = self.handles.get(&fh).and_then(|h| h.dirty.as_ref()) {
                if let Err(e) = f.sync_all() {
                    return reply.error(io_errno(e));
                }
            }
        }
        let engine = self.engine.clone();
        let r = self.block(async {
            if written {
                engine.writeback.enqueue(id).await?;
            }
            if engine.writeback.is_pending(id)? {
                engine.writeback.flush(id).await?;
            }
            Ok::<(), Error>(())
        });
        if let Some(h) = self.handles.get_mut(&fh) {
            h.written = false;
        }
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.engine.stat(ItemId(ino)) {
            Ok(n) if n.kind == Kind::Dir => reply.opened(0, 0),
            Ok(_) => reply.error(libc::ENOTDIR),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let dir = ItemId(ino);
        let engine = self.engine.clone();
        let children = match self.block(engine.vfs.list(dir, false)) {
            Ok(c) => c,
            Err(e) => return reply.error(errno(e)),
        };
        let parent = match engine.vfs.get(dir) {
            Ok(n) if n.id != ItemId::ROOT => n.parent.0,
            _ => ItemId::ROOT.0,
        };
        // offset 是"下一个要返回的条目索引"：0 ".", 1 "..", 2.. children
        let mut entries: Vec<(u64, FileType, String)> = vec![
            (ino, FileType::Directory, ".".into()),
            (parent, FileType::Directory, "..".into()),
        ];
        entries.extend(children.into_iter().map(|c| {
            (
                c.id.0,
                if c.kind == Kind::Dir {
                    FileType::Directory
                } else {
                    FileType::RegularFile
                },
                c.name,
            )
        }));
        for (i, (ino, kind, name)) in entries.into_iter().enumerate().skip(offset.max(0) as usize) {
            if reply.add(ino, (i + 1) as i64, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: ReplyStatfs) {
        // 对象存储没有容量概念，报告一个很大的虚拟容量。
        let total: u64 = 1 << 40; // 1 PiB in 1 MiB blocks
        reply.statfs(total, total, total, 1 << 32, 1 << 32, 1 << 20, 255, 1 << 20);
    }

    fn access(&mut self, _req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        if mask & libc::W_OK != 0 && self.engine.cfg.read_only {
            return reply.error(libc::EROFS);
        }
        match self.engine.stat(ItemId(ino)) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        if let Err(e) = self.engine.ensure_writable() {
            return reply.error(errno(e));
        }
        let name = match Self::name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let engine = self.engine.clone();
        let node = match self.block(engine.vfs.create_file(ItemId(parent), name)) {
            Ok(n) => n,
            Err(e) => return reply.error(errno(e)),
        };
        let f = match self.open_write(node.id, flags & libc::O_TRUNC != 0) {
            Ok(f) => f,
            Err(e) => return reply.error(e),
        };
        // 新建的空文件也要上传，让远端尽快出现该对象。
        let fh = self.alloc_fh(Handle {
            id: node.id,
            dirty: Some(f),
            written: true,
        });
        let attr = self.attr(&node);
        reply.created(&TTL, &attr, 0, fh, 0);
    }
}

impl NdiskFs {
    fn remove(&mut self, parent: u64, name: &OsStr, kind: Kind, reply: ReplyEmpty) {
        if let Err(e) = self.engine.ensure_writable() {
            return reply.error(errno(e));
        }
        let name = match Self::name(name) {
            Ok(n) => n,
            Err(e) => return reply.error(e),
        };
        let engine = self.engine.clone();
        let r = self.block(async {
            let n = engine.vfs.lookup(ItemId(parent), name).await?;
            match (kind, n.kind) {
                (Kind::File, Kind::Dir) => return Err(Error::IsADirectory(name.to_string())),
                (Kind::Dir, Kind::File) => return Err(Error::NotADirectory(name.to_string())),
                _ => {}
            }
            engine.vfs.delete(n.id).await
        });
        match r {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(errno(e)),
        }
    }
}

/// 阻塞当前线程直到卸载。必须在 tokio runtime 线程之外调用。
pub fn mount(
    engine: Arc<Engine>,
    rt: tokio::runtime::Handle,
    mountpoint: &Path,
    opts: MountOptions,
) -> ndisk_core::Result<()> {
    let fs = NdiskFs {
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        engine,
        rt,
        handles: HashMap::new(),
        next_fh: 0,
    };
    let mut mo = vec![
        MountOption::FSName("ndisk".into()),
        MountOption::Subtype("ndisk".into()),
        MountOption::DefaultPermissions,
        MountOption::NoAtime,
    ];
    if opts.allow_other {
        mo.push(MountOption::AllowOther);
    }
    if opts.auto_unmount {
        mo.push(MountOption::AutoUnmount);
    }
    fuser::mount2(fs, mountpoint, &mo).map_err(ndisk_core::Error::Io)
}
