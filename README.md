# rusty-bucket

A read-only [FUSE](https://github.com/libfuse/libfuse) filesystem for
S3-compatible object stores, written in Rust. It is a read-only port of
[goofys](https://github.com/kahing/goofys), designed to mount buckets served by
**Ceph RADOS Gateway (RGW)** and other S3-compatible endpoints. All write
operations are rejected with `EROFS`.

```
rusty-bucket <bucket[:prefix]> <mountpoint> [OPTIONS]
```

## Features

- High-performance, read-only access to S3 buckets as a local filesystem.
- Path-style addressing by default (what Ceph RGW expects), with optional
  virtual-hosted-style (`--subdomain`).
- Signature V4 credentials via environment variables, `~/.aws/credentials`, or
  `--profile`.
- `--cacert` to trust a private CA / self-signed certificate (common for
  internal Ceph RGW deployments).
- Metadata caching (`--stat-cache-ttl`), name/kind caching
  (`--type-cache-ttl`), and read-ahead (`--readahead`) for sequential reads.
- Implicit directory support: directories that exist only as key prefixes are
  discovered automatically (`--no-implicit-dir` to disable the probe).
- Reclaims inodes created by `readdir` when they are forgotten — the inode-leak
  fix from goofys [PR #786](https://github.com/kahing/goofys/pull/786), as
  carried by the [StatCan fork](https://github.com/StatCan/goofys).

## Read-only semantics

The filesystem is mounted `ro` and refuses every mutating operation:

| Operation | Result |
| --- | --- |
| `open(O_WRONLY/O_RDWR)`, `create`, `write`, `fallocate` | `EROFS` |
| `mkdir`, `rmdir`, `mknod`, `symlink`, `link`, `unlink`, `rename` | `EROFS` |
| `setattr`, `setxattr`, `removexattr` | `EROFS` |

`flush`, `fsync`, `release`, `fsyncdir` and `releasedir` succeed as no-ops
because nothing needs to be written back.

## Building and running with Nix

The flake provides the toolchain and native dependencies. All packaging uses
[`crane`](https://github.com/ipetkov/crane).

```console
# enter a development shell (rustc, cargo, fmt, clippy, fuse3, awscli)
$ nix develop

# build the release binary
$ nix build
$ ./result/bin/rusty-bucket --help

# run directly
$ nix run . -- --endpoint https://rgw.example.com:7480 mybucket /mnt/data
```

Using plain `cargo` also works once a FUSE development/runtime is available:
`fusermount3` must be on `PATH` at runtime.

## Credentials

Credentials are resolved by the AWS SDK, in order:

1. `AWS_ACCESS_KEY_ID` / `AWS_SECRET_ACCESS_KEY` / `AWS_SESSION_TOKEN`
2. `~/.aws/credentials` (optionally via `--profile NAME`)
3. Instance/role metadata (if available)

Example for Ceph RGW:

```console
$ export AWS_ACCESS_KEY_ID=my-access-key
$ export AWS_SECRET_ACCESS_KEY=my-secret-key
$ rusty-bucket \
    --endpoint https://rgw.example.com:7480 \
    --region us-east-1 \
    --cacert /etc/ssl/certs/rgw-ca.pem \
    mybucket /mnt/data
```

To mount only part of a bucket, use `bucket:prefix`:

```console
$ rusty-bucket --endpoint https://rgw.example.com:7480 mybucket:datasets/2024 /mnt/data
```

## Options

| Option | Description |
| --- | --- |
| `--endpoint URL` | S3 endpoint (e.g. `https://rgw.example.com:7480`). |
| `--region REGION` | S3 region (Ceph RGW commonly uses `us-east-1`). |
| `--profile NAME` | Named profile in `~/.aws/credentials`. |
| `--cacert PATH` | PEM bundle to trust for TLS (private CA / self-signed). |
| `--subdomain` | Use virtual-hosted-style requests instead of path-style. |
| `--dir-mode MODE` | Directory permission bits (default `0755`). |
| `--file-mode MODE` | File permission bits (default `0644`). |
| `--uid UID`, `--gid GID` | Owner of all inodes (default: current user). |
| `--stat-cache-ttl DUR` | TTL for object metadata (default `1m`). |
| `--type-cache-ttl DUR` | TTL for name -> file/dir mappings (default `1m`). |
| `--readahead SIZE` | Read-ahead per ranged GET (default `4M`). |
| `--no-implicit-dir` | Assume all directories exist; skip the probe. |
| `-o, --opt OPTION` | Extra FUSE mount option (repeatable). |
| `-f, --foreground` | Accepted for goofys parity (always runs foreground). |

Durations accept `s`, `m`, `h` suffixes (or a bare number of seconds). Sizes
accept `K`, `M`, `G` suffixes.

## Differences from goofys

- Written in Rust; FUSE binding is [`fuser`](https://github.com/cberner/fuser),
  S3 client is the official [AWS SDK for Rust](https://github.com/awslabs/aws-sdk-rust).
- **Read-only**: there is no write, multipart upload, copy, rename, or delete
  support.
- No on-disk data cache (`catfs`) and no Azure/GCS/ADL backends; S3-compatible
  endpoints only.
- Symlinks, hard links, and extended attributes are not supported.

## Testing

Unit tests run with `cargo test` (and as the `test` flake check). The
`tests/ceph.rs` conformance suite mounts a real bucket and **skips
automatically** unless `CEPH_TEST_ENDPOINT` is set, so plain `cargo test` stays
green offline.

### Against a throwaway Ceph RGW

The provisioning script pulls `quay.io/ceph/demo`, starts a single-node Ceph
cluster with RGW, runs the suite, and tears everything down. It needs Docker or
Podman and `/dev/fuse`:

```console
$ nix run .#ceph-test          # or: nix develop && ./scripts/ceph-test.sh
```

Useful environment variables: `KEEP=1` keeps the container, `CEPH_TEST_PORT`
changes the host port, and `CEPH_TEST_IMAGE` changes the image.

### Against an existing cluster

Set the connection details and run the suite directly:

```console
$ CEPH_TEST_ENDPOINT=https://rgw.example.com:7480 \
  CEPH_TEST_ACCESS_KEY=... \
  CEPH_TEST_SECRET_KEY=... \
  CEPH_TEST_BUCKET=mybucket \
  CEPH_TEST_REGION=us-east-1 \
  CEPH_TEST_CACERT=/etc/ssl/certs/rgw-ca.pem \
  cargo test --test ceph -- --nocapture --test-threads=1
```

The suite exercises listing (including implicit directories and directory
markers), stat sizes, full and ranged reads crossing the read-ahead boundary,
10 MiB objects, special/unicode key names, sub-prefix mounts, and that every
mutating operation fails with `EROFS`.

## License

Licensed under the Apache License, Version 2.0. See `LICENSE` and `NOTICE`.
