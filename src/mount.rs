use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use fuser::{FileAttr, FileType, INodeNo};
use lru::LruCache;
use parking_lot::{Mutex, RwLock};

use crate::backend::{FileMeta, ListResult, S3Backend};
use crate::config::FsConfig;
use crate::error::{Error, Result};
use crate::handle::{DirEntry, DirHandle, FileHandle};

pub const ROOT_INO: u64 = 1;

const CACHE_ENTRIES: usize = 4096;

/// A resolved type/prefix mapping used by the type cache.
#[derive(Clone, Debug)]
struct CachedEntry {
    kind: FileType,
    prefix: String,
    size: u64,
    mtime: SystemTime,
}

/// A single node in the filesystem tree. Directories correspond to common
/// prefixes in the bucket; files correspond to objects.
pub struct Inode {
    pub id: u64,
    pub parent: u64,
    pub name: String,
    pub kind: FileType,
    /// Full S3 key for files, or the prefix (ending in `/`) for directories.
    pub prefix: String,
    pub attr: Mutex<FileMeta>,
    pub stat_time: Mutex<Option<Instant>>,
    /// Child inode ids (directories only). Used to reclaim the inodes that
    /// `readdir` interns but the kernel never looks up (the "inode leak" fixed
    /// upstream in goofys PR #786).
    pub children: Mutex<Vec<u64>>,
    /// Number of outstanding kernel lookups. Inodes interned by `readdir`
    /// start at zero and may be reclaimed eagerly.
    pub lookup_count: AtomicU64,
}

impl Inode {
    fn is_dir(&self) -> bool {
        self.kind == FileType::Directory
    }
}

/// The mounted filesystem state: inode table, caches and open handles.
pub struct Mount {
    pub backend: S3Backend,
    pub rt: tokio::runtime::Handle,
    pub config: FsConfig,

    inodes: RwLock<HashMap<u64, Arc<Inode>>>,
    child_index: RwLock<HashMap<(u64, String), u64>>,
    next_ino: AtomicU64,

    pub files: Mutex<HashMap<u64, Arc<FileHandle>>>,
    pub dirs: Mutex<HashMap<u64, Arc<DirHandle>>>,
    next_fh: AtomicU64,

    stat_cache: Mutex<LruCache<String, (Instant, FileMeta)>>,
    lookup_cache: Mutex<LruCache<(u64, String), (Instant, CachedEntry)>>,
}

impl Mount {
    pub fn new(backend: S3Backend, rt: tokio::runtime::Handle, config: FsConfig) -> Self {
        let root = Inode {
            id: ROOT_INO,
            parent: ROOT_INO,
            name: String::new(),
            kind: FileType::Directory,
            prefix: config.prefix.clone(),
            attr: Mutex::new(FileMeta {
                size: 0,
                mtime: SystemTime::now(),
            }),
            stat_time: Mutex::new(None),
            children: Mutex::new(Vec::new()),
            lookup_count: AtomicU64::new(0),
        };

        let mut inodes = HashMap::new();
        inodes.insert(ROOT_INO, Arc::new(root));

        let cap = NonZeroUsize::new(CACHE_ENTRIES).expect("non-zero");
        Mount {
            backend,
            rt,
            config,
            inodes: RwLock::new(inodes),
            child_index: RwLock::new(HashMap::new()),
            next_ino: AtomicU64::new(ROOT_INO + 1),
            files: Mutex::new(HashMap::new()),
            dirs: Mutex::new(HashMap::new()),
            next_fh: AtomicU64::new(1),
            stat_cache: Mutex::new(LruCache::new(cap)),
            lookup_cache: Mutex::new(LruCache::new(cap)),
        }
    }

    pub fn ttl(&self) -> std::time::Duration {
        self.config.stat_cache_ttl
    }

    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        self.rt.block_on(fut)
    }

    pub fn head(&self, key: &str) -> Result<FileMeta> {
        self.block_on(self.backend.head(key))
    }

    pub fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        self.block_on(self.backend.get_range(key, start, len))
    }

    pub fn list_page(&self, prefix: &str, token: Option<&str>) -> Result<ListResult> {
        self.block_on(self.backend.list_page(prefix, token))
    }

    pub fn dir_has_entries(&self, prefix: &str) -> Result<bool> {
        self.block_on(self.backend.dir_has_entries(prefix))
    }

    pub fn get(&self, id: u64) -> Option<Arc<Inode>> {
        self.inodes.read().get(&id).cloned()
    }

    /// Intern an inode for `parent/name`, reusing an existing one if present.
    #[allow(clippy::too_many_arguments)]
    pub fn intern(
        &self,
        parent: u64,
        name: &str,
        kind: FileType,
        prefix: String,
        size: u64,
        mtime: SystemTime,
    ) -> Arc<Inode> {
        if let Some(id) = self
            .child_index
            .read()
            .get(&(parent, name.to_string()))
            .copied()
        {
            if let Some(inode) = self.get(id) {
                return inode;
            }
        }

        let id = self.next_ino.fetch_add(1, Ordering::SeqCst);
        let inode = Arc::new(Inode {
            id,
            parent,
            name: name.to_string(),
            kind,
            prefix,
            attr: Mutex::new(FileMeta { size, mtime }),
            stat_time: Mutex::new(if kind == FileType::RegularFile {
                Some(Instant::now())
            } else {
                None
            }),
            children: Mutex::new(Vec::new()),
            lookup_count: AtomicU64::new(0),
        });

        self.inodes.write().insert(id, inode.clone());
        self.child_index
            .write()
            .insert((parent, name.to_string()), id);

        if kind == FileType::Directory {
            if let Some(parent_inode) = self.get(parent) {
                parent_inode.children.lock().push(id);
            }
        }

        inode
    }

    pub fn file_attr(&self, inode: &Inode) -> FileAttr {
        let attr = *inode.attr.lock();
        let perm = if inode.is_dir() {
            self.config.dir_mode
        } else {
            self.config.file_mode
        };

        FileAttr {
            ino: INodeNo(inode.id),
            size: attr.size,
            blocks: attr.size.div_ceil(512),
            atime: attr.mtime,
            mtime: attr.mtime,
            ctime: attr.mtime,
            crtime: attr.mtime,
            kind: inode.kind,
            perm: perm as u16,
            nlink: if inode.is_dir() { 2 } else { 1 },
            uid: self.config.uid,
            gid: self.config.gid,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    fn store_attr(&self, inode: &Inode, meta: &FileMeta) {
        *inode.attr.lock() = *meta;
        *inode.stat_time.lock() = Some(Instant::now());
        if inode.kind == FileType::RegularFile {
            self.stat_cache
                .lock()
                .put(inode.prefix.clone(), (Instant::now(), *meta));
        }
    }

    /// Refresh a file's attributes from S3 if the stat cache has expired.
    fn refresh_if_stale(&self, inode: &Inode) -> Result<()> {
        if inode.kind != FileType::RegularFile {
            return Ok(());
        }

        let stale = inode
            .stat_time
            .lock()
            .is_none_or(|t| t.elapsed() > self.config.stat_cache_ttl);

        if !stale {
            return Ok(());
        }

        // Fast path: a fresh entry in the shared stat cache.
        if let Some((when, attr)) = self.stat_cache.lock().get(&inode.prefix).copied() {
            if when.elapsed() <= self.config.stat_cache_ttl {
                *inode.attr.lock() = attr;
                *inode.stat_time.lock() = Some(when);
                return Ok(());
            }
        }

        let meta = self.head(&inode.prefix)?;
        self.store_attr(inode, &meta);
        Ok(())
    }

    fn cache_lookup(&self, parent: u64, name: &str, entry: &CachedEntry) {
        self.lookup_cache
            .lock()
            .put((parent, name.to_string()), (Instant::now(), entry.clone()));
    }

    /// Resolve a directory entry to an inode.
    pub fn lookup(&self, parent_id: u64, name: &str) -> Result<Arc<Inode>> {
        let parent = self.get(parent_id).ok_or(Error::NotFound)?;
        if !parent.is_dir() {
            return Err(Error::NotDir);
        }

        if let Some(id) = self
            .child_index
            .read()
            .get(&(parent_id, name.to_string()))
            .copied()
        {
            if let Some(inode) = self.get(id) {
                self.refresh_if_stale(&inode)?;
                inode.lookup_count.fetch_add(1, Ordering::SeqCst);
                return Ok(inode);
            }
        }

        // Type cache: avoid a round trip for a recently resolved name.
        let cached = self
            .lookup_cache
            .lock()
            .get(&(parent_id, name.to_string()))
            .and_then(|(when, entry)| {
                (when.elapsed() <= self.config.type_cache_ttl).then(|| entry.clone())
            });
        if let Some(entry) = cached {
            let inode = self.intern(
                parent_id,
                name,
                entry.kind,
                entry.prefix,
                entry.size,
                entry.mtime,
            );
            inode.lookup_count.fetch_add(1, Ordering::SeqCst);
            return Ok(inode);
        }

        let full = format!("{}{}", parent.prefix, name);

        match self.head(&full) {
            Ok(meta) => {
                let inode = self.intern(
                    parent_id,
                    name,
                    FileType::RegularFile,
                    full,
                    meta.size,
                    meta.mtime,
                );
                self.store_attr(&inode, &meta);
                self.cache_lookup(
                    parent_id,
                    name,
                    &CachedEntry {
                        kind: FileType::RegularFile,
                        prefix: inode.prefix.clone(),
                        size: meta.size,
                        mtime: meta.mtime,
                    },
                );
                inode.lookup_count.fetch_add(1, Ordering::SeqCst);
                Ok(inode)
            }
            Err(Error::NotFound) if self.config.implicit_dir => {
                let dir_prefix = format!("{full}/");
                if !self.dir_has_entries(&dir_prefix)? {
                    Err(Error::NotFound)
                } else {
                    let mtime = SystemTime::now();
                    let inode = self.intern(
                        parent_id,
                        name,
                        FileType::Directory,
                        dir_prefix.clone(),
                        0,
                        mtime,
                    );
                    self.cache_lookup(
                        parent_id,
                        name,
                        &CachedEntry {
                            kind: FileType::Directory,
                            prefix: dir_prefix,
                            size: 0,
                            mtime,
                        },
                    );
                    inode.lookup_count.fetch_add(1, Ordering::SeqCst);
                    Ok(inode)
                }
            }
            Err(e) => Err(e),
        }
    }

    /// `getattr`: resolve an inode and refresh stale attributes.
    pub fn getattr(&self, ino: u64) -> Result<Arc<Inode>> {
        let inode = self.get(ino).ok_or(Error::NotFound)?;
        self.refresh_if_stale(&inode)?;
        Ok(inode)
    }

    /// Drop references taken by the kernel. Reclaims inodes that were interned
    /// by `readdir` but never looked up, preventing unbounded growth.
    pub fn forget(&self, ino: u64, nlookup: u64) {
        if ino == ROOT_INO {
            return;
        }

        let Some(inode) = self.get(ino) else {
            return;
        };

        let remaining = inode
            .lookup_count
            .load(Ordering::SeqCst)
            .saturating_sub(nlookup);
        inode.lookup_count.store(remaining, Ordering::SeqCst);
        if remaining > 0 {
            return;
        }

        // Reclaim children that only exist because of `readdir` (lookup_count 0).
        if inode.is_dir() {
            let children = inode.children.lock().clone();
            for child in children {
                if let Some(child_inode) = self.get(child) {
                    if child_inode.lookup_count.load(Ordering::SeqCst) == 0 {
                        self.forget(child, u64::MAX);
                    }
                }
            }
        }

        self.inodes.write().remove(&ino);
        if !inode.name.is_empty() {
            self.child_index
                .write()
                .remove(&(inode.parent, inode.name.clone()));
        }
        if let Some(parent) = self.get(inode.parent) {
            parent.children.lock().retain(|&id| id != ino);
        }
    }

    pub fn open(&self, ino: u64) -> Result<u64> {
        let inode = self.getattr(ino)?;
        if inode.is_dir() {
            return Err(Error::IsDir);
        }

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        let size = inode.attr.lock().size;
        let handle = Arc::new(FileHandle::new(inode.prefix.clone(), size));
        self.files.lock().insert(fh, handle);
        Ok(fh)
    }

    pub fn read(&self, fh: u64, offset: u64, size: u32) -> Result<Vec<u8>> {
        let handle = self
            .files
            .lock()
            .get(&fh)
            .cloned()
            .ok_or_else(|| Error::Invalid("unknown file handle".into()))?;
        handle.read(self, offset, size)
    }

    pub fn release_fh(&self, fh: u64) {
        self.files.lock().remove(&fh);
    }

    pub fn opendir(&self, ino: u64) -> Result<u64> {
        let inode = self.get(ino).ok_or(Error::NotFound)?;
        if !inode.is_dir() {
            return Err(Error::NotDir);
        }

        let fh = self.next_fh.fetch_add(1, Ordering::SeqCst);
        let handle = Arc::new(DirHandle::new(&inode));
        self.dirs.lock().insert(fh, handle);
        Ok(fh)
    }

    pub fn get_dir(&self, fh: u64) -> Option<Arc<DirHandle>> {
        self.dirs.lock().get(&fh).cloned()
    }

    pub fn release_dir(&self, fh: u64) {
        self.dirs.lock().remove(&fh);
    }

    /// Fill a directory handle's cache with at least `want` entries.
    pub fn readdir_entries(&self, dh: &DirHandle, want: usize) -> Result<Vec<DirEntry>> {
        dh.fill_until(self, want)?;
        Ok(dh.snapshot())
    }
}
