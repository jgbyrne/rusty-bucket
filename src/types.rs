use std::time::SystemTime;

/// Whether an object is a regular file (an S3 object) or a directory (a common
/// prefix, possibly implicit).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

/// Metadata for a single S3 object.
#[derive(Clone, Debug)]
pub struct FileMeta {
    pub size: u64,
    pub mtime: SystemTime,
}
