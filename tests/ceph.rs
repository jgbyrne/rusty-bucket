//! Read-only conformance suite that runs against a real S3-compatible endpoint
//! (normally Ceph RGW).
//!
//! The suite is skipped when `CEPH_TEST_ENDPOINT` is unset, so plain
//! `cargo test` is unaffected. See `scripts/ceph-test.sh` for a script that
//! provisions a throwaway Ceph RGW container and runs this.
//!
//! Environment:
//!   CEPH_TEST_ENDPOINT    e.g. http://127.0.0.1:18080   (required to run)
//!   CEPH_TEST_ACCESS_KEY  access key                     (required to run)
//!   CEPH_TEST_SECRET_KEY  secret key                     (required to run)
//!   CEPH_TEST_BUCKET      bucket name (default rusty-bucket-ceph-test)
//!   CEPH_TEST_REGION      region (default us-east-1)
//!   CEPH_TEST_CACERT      extra PEM CA bundle (optional, for HTTPS)

mod common;

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::{Credentials, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use common::{pattern, read_all, MountGuard};

const DEFAULT_BUCKET: &str = "rusty-bucket-ceph-test";

/// Keys with characters that must survive URL encoding.
const SPECIAL: &[(&str, &str)] = &[
    ("with space.txt", "space"),
    ("with+plus.txt", "plus"),
    ("comma,key.txt", "comma"),
    ("percent%.txt", "percent"),
    ("amp&equals=key.txt", "amp"),
    ("quote'and\"dq.txt", "quote"),
    ("tilde~hash#key.txt", "tilde"),
    ("question?mark.txt", "question"),
    ("uni-ü-😀.txt", "unicode"),
];

struct Ceph {
    endpoint: String,
    access_key: String,
    secret_key: String,
    region: String,
    bucket: String,
    cacert: Option<std::path::PathBuf>,
}

impl Ceph {
    fn from_env() -> Option<Self> {
        let endpoint = std::env::var("CEPH_TEST_ENDPOINT").ok()?;
        Some(Self {
            endpoint,
            access_key: std::env::var("CEPH_TEST_ACCESS_KEY").unwrap_or_default(),
            secret_key: std::env::var("CEPH_TEST_SECRET_KEY").unwrap_or_default(),
            region: std::env::var("CEPH_TEST_REGION").unwrap_or_else(|_| "us-east-1".into()),
            bucket: std::env::var("CEPH_TEST_BUCKET").unwrap_or_else(|_| DEFAULT_BUCKET.into()),
            cacert: std::env::var("CEPH_TEST_CACERT").ok().map(Into::into),
        })
    }

    fn client(&self) -> Client {
        let creds = Credentials::new(
            self.access_key.clone(),
            self.secret_key.clone(),
            None,
            None,
            "ceph-test",
        );
        let mut builder = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(self.region.clone()))
            .credentials_provider(creds)
            .endpoint_url(&self.endpoint)
            .force_path_style(true)
            // Older RGW releases reject the SDK's default checksum trailers.
            .request_checksum_calculation(
                aws_sdk_s3::config::RequestChecksumCalculation::WhenRequired,
            )
            .response_checksum_validation(
                aws_sdk_s3::config::ResponseChecksumValidation::WhenRequired,
            );
        if let Some(ca) = &self.cacert {
            builder = builder
                .http_client(rusty_bucket::backend::custom_http_client(ca).expect("TLS client"));
        }
        Client::from_conf(builder.build())
    }

    fn child_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("AWS_ACCESS_KEY_ID", self.access_key.clone()),
            ("AWS_SECRET_ACCESS_KEY", self.secret_key.clone()),
            ("AWS_REGION", self.region.clone()),
            ("AWS_DEFAULT_REGION", self.region.clone()),
            ("AWS_EC2_METADATA_DISABLED", "true".to_string()),
        ]
    }
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let big = pattern(10 * 1024 * 1024, 2);
    let crossing = pattern(6 * 1024 * 1024 + 123, 1);

    let mut files: Vec<(String, Vec<u8>)> = vec![
        ("top.txt".into(), b"hello top\n".to_vec()),
        ("empty.bin".into(), Vec::new()),
        ("onebyte.bin".into(), vec![0x42]),
        ("readahead-crossing.bin".into(), crossing),
        ("big.bin".into(), big),
        ("small/k1.txt".into(), b"k1".to_vec()),
        ("small/k2.txt".into(), b"k2".to_vec()),
        ("small/nested/deep.txt".into(), b"deep".to_vec()),
        // A zero-byte object whose key ends in '/' is a directory marker.
        ("dir-marker/".into(), Vec::new()),
        ("dir-marker/inside.txt".into(), b"inside".to_vec()),
    ];
    for (name, content) in SPECIAL {
        files.push((format!("special/{name}"), content.as_bytes().to_vec()));
    }
    files
}

fn top_level_names() -> BTreeSet<String> {
    let mut set: BTreeSet<String> = fixtures()
        .into_iter()
        .map(|(key, _)| key.split('/').next().unwrap().to_string())
        .collect();
    // `dir-marker` comes from the marker key `dir-marker/`.
    set.insert("dir-marker".to_string());
    set
}

async fn setup(client: &Client, bucket: &str, files: &[(String, Vec<u8>)]) {
    // Create the bucket; ignore "already owned/exists".
    let _ = client.create_bucket().bucket(bucket).send().await;

    for (key, data) in files {
        client
            .put_object()
            .bucket(bucket)
            .key(key)
            .body(ByteStream::from(data.clone()))
            .send()
            .await
            .unwrap_or_else(|e| panic!("PUT {key}: {e:?}"));
    }
}

async fn teardown(client: &Client, bucket: &str) {
    let mut token: Option<String> = None;
    for _ in 0..10_000 {
        let out = match client
            .list_objects_v2()
            .bucket(bucket)
            .set_continuation_token(token.clone())
            .send()
            .await
        {
            Ok(out) => out,
            Err(_) => return,
        };

        for obj in out.contents() {
            if let Some(key) = obj.key() {
                let _ = client.delete_object().bucket(bucket).key(key).send().await;
            }
        }

        let next = out.next_continuation_token().map(str::to_string);
        if !out.is_truncated().unwrap_or(false) || next == token || next.is_none() {
            break;
        }
        token = next;
    }
    let _ = client.delete_bucket().bucket(bucket).send().await;
}

struct Cleanup {
    handle: tokio::runtime::Handle,
    client: Client,
    bucket: String,
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        let client = self.client.clone();
        let bucket = self.bucket.clone();
        self.handle
            .block_on(async move { teardown(&client, &bucket).await });
    }
}

fn listing(dir: &Path) -> BTreeSet<String> {
    fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn assert_erofs(err: std::io::Error, what: &str) {
    assert_eq!(
        err.raw_os_error(),
        Some(libc::EROFS),
        "{what} should fail with EROFS, got: {err}"
    );
}

fn assert_reads(mp: &Path, files: &[(String, Vec<u8>)]) {
    for (key, expected) in files {
        if key.ends_with('/') {
            continue;
        }
        let path = mp.join(key);
        let got = read_all(&path);
        assert_eq!(
            &got,
            expected,
            "content mismatch for {key} (len {} vs {})",
            got.len(),
            expected.len()
        );
    }
}

#[test]
fn ceph_readonly_suite() {
    let Some(cfg) = Ceph::from_env() else {
        eprintln!("skipping ceph_readonly_suite: CEPH_TEST_ENDPOINT is not set");
        return;
    };

    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let client = cfg.client();
    let files = fixtures();

    runtime.block_on(setup(&client, &cfg.bucket, &files));

    let _cleanup = Cleanup {
        handle: runtime.handle().clone(),
        client: client.clone(),
        bucket: cfg.bucket.clone(),
    };

    let env = cfg.child_env();

    // -------- Whole-bucket mount --------
    let args = vec![
        "--endpoint".to_string(),
        cfg.endpoint.clone(),
        "--region".to_string(),
        cfg.region.clone(),
        cfg.bucket.clone(),
    ];
    let mnt = MountGuard::spawn(&args, &env);
    let mp = mnt.mountpoint();

    // Directory listing, including implicit directories and a dir marker.
    let names = listing(mp);
    for expected in top_level_names() {
        assert!(
            names.contains(&expected),
            "root listing missing {expected}: {names:?}"
        );
    }

    let small = listing(&mp.join("small"));
    assert!(small.contains("k1.txt") && small.contains("k2.txt") && small.contains("nested"));
    assert!(
        fs::metadata(mp.join("small/nested")).unwrap().is_dir(),
        "small/nested should be an implicit directory"
    );
    assert_eq!(
        read_all(&mp.join("small/nested/deep.txt")),
        b"deep".to_vec()
    );

    let marker = listing(&mp.join("dir-marker"));
    assert!(marker.contains("inside.txt"));
    assert_eq!(
        read_all(&mp.join("dir-marker/inside.txt")),
        b"inside".to_vec()
    );

    // Sizes and full contents.
    assert_eq!(fs::metadata(mp.join("top.txt")).unwrap().len(), 10);
    assert_eq!(fs::metadata(mp.join("empty.bin")).unwrap().len(), 0);
    assert!(read_all(&mp.join("empty.bin")).is_empty());
    assert_eq!(read_all(&mp.join("onebyte.bin")), vec![0x42]);
    assert_eq!(
        fs::metadata(mp.join("big.bin")).unwrap().len(),
        10 * 1024 * 1024
    );

    assert_reads(mp, &files);

    // Ranged reads crossing the read-ahead boundary.
    let (crossing_key, crossing_data) = files
        .iter()
        .find(|(k, _)| k == "readahead-crossing.bin")
        .unwrap();
    assert_eq!(crossing_key, "readahead-crossing.bin");
    let mut f = fs::File::open(mp.join("readahead-crossing.bin")).unwrap();
    for &offset in &[
        0u64,
        1,
        4095,
        4096,
        1024 * 1024 - 3,
        4 * 1024 * 1024,
        5 * 1024 * 1024 + 100,
    ] {
        let remaining = crossing_data.len() as u64 - offset;
        let n = (200_000u64).min(remaining) as usize;
        f.seek(SeekFrom::Start(offset)).unwrap();
        let mut buf = vec![0u8; n];
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, &crossing_data[offset as usize..offset as usize + n]);
    }
    drop(f);

    // Special characters / unicode names round-trip correctly.
    for (name, content) in SPECIAL {
        let path = mp.join("special").join(name);
        assert_eq!(
            read_all(&path),
            content.as_bytes().to_vec(),
            "content for {name}"
        );
    }

    // Mutations must be refused.
    assert_erofs(fs::File::create(mp.join("newfile")).unwrap_err(), "create");
    assert_erofs(fs::write(mp.join("top.txt"), b"x").unwrap_err(), "write");
    assert_erofs(fs::create_dir(mp.join("newdir")).unwrap_err(), "mkdir");
    assert_erofs(fs::remove_file(mp.join("top.txt")).unwrap_err(), "unlink");
    assert_erofs(fs::remove_dir(mp.join("small")).unwrap_err(), "rmdir");
    assert_erofs(
        fs::rename(mp.join("top.txt"), mp.join("renamed.txt")).unwrap_err(),
        "rename",
    );
    // Opening for writing must fail at open() time.
    assert_erofs(
        fs::OpenOptions::new()
            .write(true)
            .open(mp.join("top.txt"))
            .unwrap_err(),
        "open(O_WRONLY)",
    );

    drop(mnt);

    // -------- Sub-prefix mount --------
    let args = vec![
        "--endpoint".to_string(),
        cfg.endpoint.clone(),
        "--region".to_string(),
        cfg.region.clone(),
        format!("{}:small", cfg.bucket),
    ];
    let mnt = MountGuard::spawn(&args, &env);
    let mp = mnt.mountpoint();

    let names = listing(mp);
    assert!(names.contains("k1.txt") && names.contains("k2.txt") && names.contains("nested"));
    assert!(!names.contains("top.txt"), "prefix mount leaked top.txt");
    assert_eq!(read_all(&mp.join("k1.txt")), b"k1".to_vec());
    assert_eq!(read_all(&mp.join("nested/deep.txt")), b"deep".to_vec());
}
