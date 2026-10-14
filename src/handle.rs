use std::time::SystemTime;

use parking_lot::Mutex;

use crate::error::Result;
use crate::mount::{Inode, Mount};
use crate::types::Kind;

#[derive(Clone, Debug)]
pub struct DirEntry {
    pub ino: u64,
    pub kind: Kind,
    pub name: String,
}

struct ReadBuf {
    offset: u64,
    data: Vec<u8>,
}

/// An open file. Keeps a read-ahead buffer so sequential reads share a single
/// ranged `GetObject` request.
pub struct FileHandle {
    pub fh: u64,
    pub ino: u64,
    pub key: String,
    pub size: u64,
    buf: Mutex<Option<ReadBuf>>,
}

impl FileHandle {
    pub fn new(fh: u64, ino: u64, key: String, size: u64) -> Self {
        Self {
            fh,
            ino,
            key,
            size,
            buf: Mutex::new(None),
        }
    }

    pub fn read(&self, mount: &Mount, offset: u64, size: u32) -> Result<Vec<u8>> {
        let file_size = self.size;
        if offset >= file_size || size == 0 {
            return Ok(Vec::new());
        }

        let want = ((file_size - offset).min(size as u64)) as usize;
        let mut out = Vec::with_capacity(want);
        let mut pos = offset;

        while out.len() < want {
            let mut guard = self.buf.lock();

            let need_fetch = match guard.as_ref() {
                Some(buf) => !(buf.offset <= pos && pos < buf.offset + buf.data.len() as u64),
                None => true,
            };

            if need_fetch {
                let remaining = file_size - pos;
                let chunk = mount.config.readahead.min(remaining).max(1);
                let data = mount.get_range(&self.key, pos, chunk)?;
                if data.is_empty() {
                    break;
                }
                *guard = Some(ReadBuf { offset: pos, data });
            }

            let buf = guard.as_ref().expect("buffer populated");
            let start = (pos - buf.offset) as usize;
            let available = buf.data.len().saturating_sub(start);
            if available == 0 {
                break;
            }
            let take = available.min(want - out.len());
            out.extend_from_slice(&buf.data[start..start + take]);
            pos += take as u64;
        }

        Ok(out)
    }
}

/// An open directory. Entries are fetched lazily, page by page, via
/// `ListObjectsV2` and cached for the lifetime of the handle.
pub struct DirHandle {
    pub fh: u64,
    pub ino: u64,
    pub prefix: String,
    entries: Mutex<Vec<DirEntry>>,
    token: Mutex<Option<String>>,
    done: Mutex<bool>,
}

impl DirHandle {
    pub fn new(fh: u64, inode: &Inode) -> Self {
        let entries = vec![
            DirEntry {
                ino: inode.id,
                kind: Kind::Dir,
                name: ".".to_string(),
            },
            DirEntry {
                ino: inode.parent,
                kind: Kind::Dir,
                name: "..".to_string(),
            },
        ];

        Self {
            fh,
            ino: inode.id,
            prefix: inode.prefix.clone(),
            entries: Mutex::new(entries),
            token: Mutex::new(None),
            done: Mutex::new(false),
        }
    }

    pub fn snapshot(&self) -> Vec<DirEntry> {
        self.entries.lock().clone()
    }

    /// Ensure the cache holds at least `want` entries, fetching pages as needed.
    pub fn fill_until(&self, mount: &Mount, want: usize) -> Result<()> {
        loop {
            if *self.done.lock() {
                return Ok(());
            }
            if self.entries.lock().len() > want {
                return Ok(());
            }

            let token = self.token.lock().clone();
            let page = mount.list(&self.prefix, Some("/"), None, token.as_deref(), None)?;

            {
                let mut entries = self.entries.lock();

                for prefix in &page.prefixes {
                    let name = strip_prefix(prefix, &self.prefix);
                    let name = name.trim_end_matches('/');
                    if name.is_empty() || name.contains('/') {
                        continue;
                    }
                    let inode = mount.intern(
                        self.ino,
                        name,
                        Kind::Dir,
                        prefix.clone(),
                        0,
                        SystemTime::now(),
                    );
                    entries.push(DirEntry {
                        ino: inode.id,
                        kind: Kind::Dir,
                        name: name.to_string(),
                    });
                }

                for (key, meta) in &page.items {
                    if key == &self.prefix || key.ends_with('/') {
                        continue;
                    }
                    let name = strip_prefix(key, &self.prefix);
                    if name.is_empty() || name.contains('/') {
                        continue;
                    }
                    let inode = mount.intern(
                        self.ino,
                        name,
                        Kind::File,
                        key.clone(),
                        meta.size,
                        meta.mtime,
                    );
                    entries.push(DirEntry {
                        ino: inode.id,
                        kind: Kind::File,
                        name: name.to_string(),
                    });
                }
            }

            *self.token.lock() = page.next_token;
            *self.done.lock() = !page.is_truncated;
        }
    }
}

fn strip_prefix<'a>(key: &'a str, prefix: &str) -> &'a str {
    key.strip_prefix(prefix).unwrap_or(key)
}
