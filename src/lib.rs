//! rusty-bucket: a read-only FUSE filesystem for S3-compatible object stores
//! (such as Ceph RGW). It is a read-only Rust port of
//! [goofys](https://github.com/kahing/goofys).

pub mod backend;
pub mod config;
pub mod error;
pub mod fuse;
pub mod handle;
pub mod mount;
pub mod types;
