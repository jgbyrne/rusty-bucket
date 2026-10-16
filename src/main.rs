//! rusty-bucket: a read-only FUSE filesystem for S3-compatible object stores
//! (such as Ceph RGW). It is a read-only Rust port of
//! [goofys](https://github.com/kahing/goofys).

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use fuser::{Config, MountOption};

mod backend;
mod config;
mod error;
mod fuse;
mod handle;
mod mount;
mod types;

use backend::{build_client, S3Backend};
use config::{parse_duration, parse_mode, parse_size, BackendConfig, FsConfig};
use fuse::RustyBucket;
use mount::Mount;

/// A read-only FUSE filesystem for S3-compatible object stores (Ceph RGW).
#[derive(Parser, Debug)]
#[command(name = "rusty-bucket", version, about, long_about = None)]
struct Cli {
    /// Bucket to mount, optionally `bucket:prefix`.
    bucket: String,

    /// Directory to mount on.
    mountpoint: PathBuf,

    /// S3 endpoint URL (e.g. `https://rgw.example.com:7480`).
    #[arg(long)]
    endpoint: Option<String>,

    /// S3 region. Usually auto-detected; Ceph RGW often uses `us-east-1`.
    #[arg(long)]
    region: Option<String>,

    /// AWS credentials profile in `~/.aws/credentials`.
    #[arg(long)]
    profile: Option<String>,

    /// PEM certificate bundle to trust (private CA / self-signed Ceph RGW).
    #[arg(long)]
    cacert: Option<PathBuf>,

    /// Use virtual-hosted-style requests instead of path-style.
    #[arg(long)]
    subdomain: bool,

    /// Permission bits for directories.
    #[arg(long, default_value = "0755")]
    dir_mode: String,

    /// Permission bits for files.
    #[arg(long, default_value = "0644")]
    file_mode: String,

    /// UID owner of all inodes.
    #[arg(long)]
    uid: Option<u32>,

    /// GID owner of all inodes.
    #[arg(long)]
    gid: Option<u32>,

    /// How long to cache HEAD/GetObject metadata (e.g. `30s`, `1m`).
    #[arg(long, default_value = "1m")]
    stat_cache_ttl: String,

    /// How long to cache name -> file/dir mappings.
    #[arg(long, default_value = "1m")]
    type_cache_ttl: String,

    /// Read-ahead size per ranged GET (e.g. `4M`, `512K`).
    #[arg(long, default_value = "4M")]
    readahead: String,

    /// Assume all directories exist and skip the implicit-directory probe.
    #[arg(long)]
    no_implicit_dir: bool,

    /// Additional FUSE mount option (may be repeated).
    #[arg(short = 'o', long = "opt", value_name = "OPTION")]
    fuse_opts: Vec<String>,

    /// Run in the foreground (accepted for goofys compatibility; always true).
    #[arg(short = 'f', long)]
    foreground: bool,
}

fn run() -> anyhow::Result<()> {
    let cli = Cli::parse();
    // fuser always runs in the foreground; the flag exists for goofys parity.
    let _ = cli.foreground;

    let (bucket, prefix) = FsConfig::parse_bucket_spec(&cli.bucket);

    let fs_config = FsConfig {
        bucket: bucket.clone(),
        prefix,
        dir_mode: parse_mode(&cli.dir_mode)?,
        file_mode: parse_mode(&cli.file_mode)?,
        uid: cli.uid.unwrap_or_else(|| unsafe { libc::getuid() }),
        gid: cli.gid.unwrap_or_else(|| unsafe { libc::getgid() }),
        stat_cache_ttl: parse_duration(&cli.stat_cache_ttl)?,
        type_cache_ttl: parse_duration(&cli.type_cache_ttl)?,
        implicit_dir: !cli.no_implicit_dir,
        readahead: parse_size(&cli.readahead)?,
    };

    let backend_config = BackendConfig {
        endpoint: cli.endpoint.clone(),
        region: cli.region.clone(),
        profile: cli.profile.clone(),
        subdomain: cli.subdomain,
        cacert: cli.cacert.clone(),
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let handle = runtime.handle().clone();

    let client = runtime.block_on(build_client(&backend_config))?;
    let backend = S3Backend::new(client, bucket.clone());

    tracing::info!(
        bucket = %bucket,
        mountpoint = %cli.mountpoint.display(),
        "mounting read-only S3 filesystem"
    );

    let mount = Mount::new(backend, handle, fs_config);
    let filesystem = RustyBucket::new(mount);

    let mut options = vec![
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::FSName(format!("rusty-bucket:{bucket}")),
    ];
    options.extend(cli.fuse_opts.iter().cloned().map(MountOption::CUSTOM));

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);

    let mut config = Config::default();
    config.mount_options = options;
    config.n_threads = Some(threads);
    config.clone_fd = true;

    fuser::mount(filesystem, &cli.mountpoint, &config)?;
    Ok(())
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("rusty-bucket: {err:#}");
            ExitCode::FAILURE
        }
    }
}

#[allow(dead_code)]
fn assert_arc_send_sync() {
    fn check<T: Send + Sync>() {}
    check::<Arc<Mount>>();
}
