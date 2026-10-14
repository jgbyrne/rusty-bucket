//! Shared helpers for integration tests that mount a real bucket over FUSE.

#![allow(dead_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

/// Path to the binary under test, provided by Cargo.
pub const BIN: &str = env!("CARGO_BIN_EXE_rusty-bucket");

/// A mounted filesystem, unmounted and killed on drop.
pub struct MountGuard {
    child: Child,
    base: tempfile::TempDir,
    mountpoint: PathBuf,
}

impl MountGuard {
    /// Spawn `rusty-bucket` with `args` and `env`, then wait until the
    /// mountpoint appears in `/proc/mounts`.
    pub fn spawn(args: &[String], env: &[(&str, String)]) -> MountGuard {
        let base = tempfile::tempdir().expect("create temp dir");
        let mountpoint = base.path().join("mnt");
        fs::create_dir(&mountpoint).expect("create mountpoint");

        let log_path = base.path().join("rusty-bucket.log");
        let out = File::create(&log_path).expect("create log");
        let err = out.try_clone().expect("clone log handle");

        let mut cmd = Command::new(BIN);
        cmd.args(args)
            .arg(&mountpoint)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err));
        for (key, value) in env {
            cmd.env(key, value);
        }

        let child = cmd.spawn().expect("spawn rusty-bucket");

        let guard = MountGuard {
            child,
            base,
            mountpoint,
        };
        guard.wait_until_mounted(&log_path);
        guard
    }

    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }

    fn wait_until_mounted(&self, log_path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if is_mounted(&self.mountpoint) {
                // Give fuse a moment to settle after mount(2).
                sleep(Duration::from_millis(200));
                return;
            }
            sleep(Duration::from_millis(100));
        }

        let log = fs::read_to_string(log_path).unwrap_or_default();
        panic!("filesystem did not mount within 30s; logs:\n{log}\n--- end log ---");
    }

    fn unmount(&mut self) {
        for tool in ["fusermount3", "fusermount"] {
            let ok = Command::new(tool)
                .arg("-u")
                .arg(&self.mountpoint)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                break;
            }
        }

        // Never block forever waiting for the process to exit.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = self.child.kill();
                        let _ = self.child.wait();
                        break;
                    }
                    sleep(Duration::from_millis(50));
                }
            }
        }
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        self.unmount();
        let _ = self.child.kill();
    }
}

pub fn is_mounted(mountpoint: &Path) -> bool {
    let needle = mountpoint.to_string_lossy();
    fs::read_to_string("/proc/mounts")
        .map(|mounts| mounts.lines().any(|line| line.contains(needle.as_ref())))
        .unwrap_or(false)
}

/// Read a whole file through the mount.
pub fn read_all(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// Deterministic pseudo-random bytes, reproducible in the test process.
pub fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state = seed;
    for _ in 0..len {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        out.push((state >> 33) as u8);
    }
    out
}
