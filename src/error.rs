use std::fmt;

use fuser::Errno;

/// Crate-wide error type. Each variant maps to a POSIX errno returned to the
/// kernel.
#[derive(Debug)]
pub enum Error {
    NotFound,
    NotDir,
    IsDir,
    ReadOnly,
    PermissionDenied,
    Io(String),
    Backend(String),
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn errno(&self) -> Errno {
        match self {
            Error::NotFound => Errno::ENOENT,
            Error::NotDir => Errno::ENOTDIR,
            Error::IsDir => Errno::EISDIR,
            Error::ReadOnly => Errno::EROFS,
            Error::PermissionDenied => Errno::EACCES,
            Error::Io(_) => Errno::EIO,
            Error::Backend(_) => Errno::EIO,
            Error::Invalid(_) => Errno::EINVAL,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::NotFound => write!(f, "not found"),
            Error::NotDir => write!(f, "not a directory"),
            Error::IsDir => write!(f, "is a directory"),
            Error::ReadOnly => write!(f, "read-only filesystem"),
            Error::PermissionDenied => write!(f, "permission denied"),
            Error::Io(msg) => write!(f, "I/O error: {msg}"),
            Error::Backend(msg) => write!(f, "backend error: {msg}"),
            Error::Invalid(msg) => write!(f, "invalid argument: {msg}"),
        }
    }
}

impl std::error::Error for Error {}
