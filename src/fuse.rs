use std::ffi::OsStr;

use fuser::{
    AccessFlags, Errno, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, KernelConfig, LockOwner, OpenAccMode, OpenFlags, ReplyAttr,
    ReplyData, ReplyDirectory, ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, Request,
};

use crate::mount::Mount;
use crate::types::Kind;

/// FUSE adapter around [`Mount`].
pub struct RustyBucket {
    mount: Mount,
}

impl RustyBucket {
    pub fn new(mount: Mount) -> Self {
        Self { mount }
    }

    fn kind_file_type(kind: Kind) -> FileType {
        match kind {
            Kind::File => FileType::RegularFile,
            Kind::Dir => FileType::Directory,
        }
    }
}

fn name_str(name: &OsStr) -> Option<&str> {
    name.to_str()
}

impl Filesystem for RustyBucket {
    fn init(&mut self, _req: &Request, config: &mut KernelConfig) -> std::io::Result<()> {
        let _ = config.set_max_readahead(self.mount.config.readahead.min(u32::MAX as u64) as u32);
        Ok(())
    }

    fn lookup(&self, _req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let Some(name) = name_str(name) else {
            reply.error(Errno::ENOENT);
            return;
        };

        match self.mount.lookup(parent.0, name) {
            Ok(inode) => {
                let attr = self.mount.file_attr(&inode);
                reply.entry(&self.mount.ttl(), &attr, Generation(0));
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn forget(&self, _req: &Request, ino: INodeNo, nlookup: u64) {
        self.mount.forget(ino.0, nlookup);
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        match self.mount.getattr(ino.0) {
            Ok(inode) => {
                let attr = self.mount.file_attr(&inode);
                reply.attr(&self.mount.ttl(), &attr);
            }
            Err(e) => reply.error(e.errno()),
        }
    }

    fn open(&self, _req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        if flags.acc_mode() != OpenAccMode::O_RDONLY {
            reply.error(Errno::EROFS);
            return;
        }

        match self.mount.open(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn read(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        match self.mount.read(fh.0, offset, size) {
            Ok(data) => reply.data(&data),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _lock_owner: LockOwner,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.mount.release_fh(fh.0);
        reply.ok();
    }

    fn fsync(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn opendir(&self, _req: &Request, ino: INodeNo, _flags: OpenFlags, reply: ReplyOpen) {
        match self.mount.opendir(ino.0) {
            Ok(fh) => reply.opened(FileHandle(fh), FopenFlags::empty()),
            Err(e) => reply.error(e.errno()),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let Some(dh) = self.mount.get_dir(fh.0) else {
            reply.error(Errno::EBADF);
            return;
        };

        let entries = match self.mount.readdir_entries(&dh, offset as usize) {
            Ok(entries) => entries,
            Err(e) => {
                reply.error(e.errno());
                return;
            }
        };

        for (i, entry) in entries.iter().enumerate().skip(offset as usize) {
            let next = i as u64 + 1;
            if reply.add(
                INodeNo(entry.ino),
                next,
                RustyBucket::kind_file_type(entry.kind),
                &entry.name,
            ) {
                break;
            }
        }

        reply.ok();
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        reply: ReplyEmpty,
    ) {
        self.mount.release_dir(fh.0);
        reply.ok();
    }

    fn fsyncdir(
        &self,
        _req: &Request,
        _ino: INodeNo,
        _fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        reply.statfs(0, 0, 0, 0, 0, 4096, 255, 4096);
    }

    fn access(&self, _req: &Request, _ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        if mask.contains(AccessFlags::W_OK) {
            reply.error(Errno::EACCES);
        } else {
            reply.ok();
        }
    }
}
