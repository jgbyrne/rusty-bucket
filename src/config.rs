use std::path::PathBuf;
use std::time::Duration;

use crate::error::{Error, Result};

/// Connection settings for the S3-compatible backend.
#[derive(Clone, Debug, Default)]
pub struct BackendConfig {
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub profile: Option<String>,
    /// Use virtual-hosted-style requests (Ceph RGW defaults to path-style).
    pub subdomain: bool,
    /// Extra PEM certificate to trust (private CA / self-signed Ceph RGW).
    pub cacert: Option<PathBuf>,
}

/// Filesystem-level settings (ownership, permissions, caching).
#[derive(Clone, Debug)]
pub struct FsConfig {
    pub bucket: String,
    /// Normalized key prefix: either empty or ending in `/`.
    pub prefix: String,
    pub dir_mode: u32,
    pub file_mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub stat_cache_ttl: Duration,
    pub type_cache_ttl: Duration,
    pub implicit_dir: bool,
    pub readahead: u64,
}

impl FsConfig {
    /// Split a `bucket[:prefix]` argument, normalizing the prefix.
    pub fn parse_bucket_spec(spec: &str) -> (String, String) {
        match spec.split_once(':') {
            Some((bucket, prefix)) if !prefix.is_empty() => {
                let mut prefix = prefix.to_string();
                if !prefix.ends_with('/') {
                    prefix.push('/');
                }
                (bucket.to_string(), prefix)
            }
            _ => (spec.to_string(), String::new()),
        }
    }
}

/// Parse a duration such as `30s`, `5m`, `2h`, or a bare number of seconds.
pub fn parse_duration(input: &str) -> Result<Duration> {
    let input = input.trim();
    if input.is_empty() {
        return Err(Error::Invalid("empty duration".into()));
    }

    let (value, unit) = match input.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => {
            let (v, _unit) = input.split_at(input.len() - c.len_utf8());
            (v, Some(c))
        }
        _ => (input, None),
    };

    let value: u64 = value
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid duration: {input}")))?;

    let secs = match unit {
        None | Some('s') => value,
        Some('m') => value * 60,
        Some('h') => value * 3600,
        Some('d') => value * 86400,
        Some(other) => {
            return Err(Error::Invalid(format!(
                "invalid duration unit '{other}' in {input}"
            )))
        }
    };

    Ok(Duration::from_secs(secs))
}

/// Parse an octal permission such as `0755` or `0o755`.
pub fn parse_mode(input: &str) -> Result<u32> {
    let trimmed = input.trim().trim_start_matches("0o");
    u32::from_str_radix(trimmed, 8)
        .map_err(|_| Error::Invalid(format!("invalid octal mode: {input}")))
}

/// Parse a size such as `4M`, `512K`, or a bare number of bytes.
pub fn parse_size(input: &str) -> Result<u64> {
    let input = input.trim();
    let (value, mult) = match input.chars().last() {
        Some('k') | Some('K') => (&input[..input.len() - 1], 1024),
        Some('m') | Some('M') => (&input[..input.len() - 1], 1024 * 1024),
        Some('g') | Some('G') => (&input[..input.len() - 1], 1024 * 1024 * 1024),
        _ => (input, 1),
    };
    let value: u64 = value
        .parse()
        .map_err(|_| Error::Invalid(format!("invalid size: {input}")))?;
    Ok(value * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_spec() {
        assert_eq!(
            FsConfig::parse_bucket_spec("mybucket"),
            ("mybucket".into(), "".into())
        );
        assert_eq!(
            FsConfig::parse_bucket_spec("mybucket:foo"),
            ("mybucket".into(), "foo/".into())
        );
        assert_eq!(
            FsConfig::parse_bucket_spec("mybucket:foo/bar/"),
            ("mybucket".into(), "foo/bar/".into())
        );
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("30").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("30s").unwrap(), Duration::from_secs(30));
        assert_eq!(parse_duration("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_duration("nope").is_err());
    }

    #[test]
    fn modes() {
        assert_eq!(parse_mode("0755").unwrap(), 0o755);
        assert_eq!(parse_mode("0o644").unwrap(), 0o644);
        assert_eq!(parse_mode("644").unwrap(), 0o644);
    }

    #[test]
    fn sizes() {
        assert_eq!(parse_size("4096").unwrap(), 4096);
        assert_eq!(parse_size("4M").unwrap(), 4 * 1024 * 1024);
        assert_eq!(parse_size("512K").unwrap(), 512 * 1024);
    }
}
