use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_config::BehaviorVersion;
use aws_sdk_s3::config::Region;
use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::primitives::DateTime;
use aws_sdk_s3::Client;
use aws_smithy_runtime_api::http::Response;

use crate::config::BackendConfig;
use crate::error::{Error, Result};
use crate::types::FileMeta;

/// One page of a `/`-delimited `ListObjectsV2` call.
#[derive(Clone, Debug, Default)]
pub struct ListResult {
    /// Common prefixes (directories) as full keys ending in `/`.
    pub prefixes: Vec<String>,
    /// Objects under the prefix.
    pub items: Vec<(String, FileMeta)>,
    /// Continuation token for the next page; `None` on the last page.
    pub next_token: Option<String>,
}

/// Read-only S3 storage backend, shared across all FUSE requests.
#[derive(Clone)]
pub struct S3Backend {
    client: Client,
    bucket: String,
}

impl S3Backend {
    pub fn new(client: Client, bucket: String) -> Self {
        Self { client, bucket }
    }

    /// `HeadObject` for a single key.
    pub async fn head(&self, key: &str) -> Result<FileMeta> {
        let out = self
            .client
            .head_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .map_err(map_sdk_error)?;

        Ok(FileMeta {
            size: out.content_length().unwrap_or(0).max(0) as u64,
            mtime: out
                .last_modified()
                .map(to_system_time)
                .unwrap_or(SystemTime::UNIX_EPOCH),
        })
    }

    /// `GetObject` for a byte range `[start, start + len)`.
    pub async fn get_range(&self, key: &str, start: u64, len: u64) -> Result<Vec<u8>> {
        if len == 0 {
            return Ok(Vec::new());
        }
        let end = start + len - 1;

        let out = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .range(format!("bytes={start}-{end}"))
            .send()
            .await
            .map_err(map_sdk_error)?;

        let body = out
            .body
            .collect()
            .await
            .map_err(|e| Error::Backend(e.to_string()))?;

        Ok(body.into_bytes().to_vec())
    }

    /// One page of the directory listing under `prefix`, resuming from `token`.
    pub async fn list_page(&self, prefix: &str, token: Option<&str>) -> Result<ListResult> {
        self.list(prefix, token, None).await
    }

    /// Whether anything exists under `prefix` (detects implicit directories).
    pub async fn dir_has_entries(&self, prefix: &str) -> Result<bool> {
        let page = self.list(prefix, None, Some(1)).await?;
        Ok(!page.items.is_empty() || !page.prefixes.is_empty())
    }

    /// `ListObjectsV2` with the `/` delimiter.
    async fn list(
        &self,
        prefix: &str,
        token: Option<&str>,
        max_keys: Option<i32>,
    ) -> Result<ListResult> {
        let out = self
            .client
            .list_objects_v2()
            .bucket(&self.bucket)
            .prefix(prefix)
            .delimiter("/")
            .set_max_keys(max_keys)
            .set_continuation_token(token.map(str::to_string))
            .send()
            .await
            .map_err(map_sdk_error)?;

        let prefixes = out
            .common_prefixes()
            .iter()
            .filter_map(|p| p.prefix().map(str::to_string))
            .collect();

        let items = out
            .contents()
            .iter()
            .filter_map(|obj| {
                let key = obj.key()?.to_string();
                let meta = FileMeta {
                    size: obj.size().unwrap_or(0).max(0) as u64,
                    mtime: obj
                        .last_modified()
                        .map(to_system_time)
                        .unwrap_or_else(SystemTime::now),
                };
                Some((key, meta))
            })
            .collect();

        Ok(ListResult {
            prefixes,
            items,
            next_token: out.next_continuation_token().map(str::to_string),
        })
    }
}

fn to_system_time(dt: &DateTime) -> SystemTime {
    let secs = dt.secs();
    if secs < 0 {
        return UNIX_EPOCH;
    }
    UNIX_EPOCH + Duration::new(secs as u64, dt.subsec_nanos())
}

fn map_sdk_error<E>(err: SdkError<E, Response>) -> Error
where
    E: ProvideErrorMetadata + std::error::Error + Send + Sync + 'static,
{
    if let Some(resp) = err.raw_response() {
        match resp.status().as_u16() {
            404 => return Error::NotFound,
            403 => return Error::PermissionDenied,
            _ => {}
        }
    }

    let code = err
        .as_service_error()
        .and_then(|e| e.code().map(str::to_string));
    match code.as_deref() {
        Some("NoSuchKey") | Some("NotFound") | Some("NoSuchBucket") => Error::NotFound,
        Some("AccessDenied") | Some("AllAccessDisabled") => Error::PermissionDenied,
        _ => Error::Backend(err.to_string()),
    }
}

/// Build an S3 client configured for a Ceph RGW / S3-compatible endpoint.
pub async fn build_client(cfg: &BackendConfig) -> anyhow::Result<Client> {
    let mut loader = aws_config::defaults(BehaviorVersion::latest());

    if let Some(region) = &cfg.region {
        loader = loader.region(Region::new(region.clone()));
    }
    if let Some(profile) = &cfg.profile {
        loader = loader.profile_name(profile);
    }

    let shared = loader.load().await;

    let mut builder = aws_sdk_s3::config::Builder::from(&shared)
        // Ceph RGW normally serves buckets path-style rather than via DNS.
        .force_path_style(!cfg.subdomain);

    if let Some(endpoint) = &cfg.endpoint {
        builder = builder.endpoint_url(endpoint);
    }

    if let Some(ca_path) = &cfg.cacert {
        builder = builder.http_client(custom_http_client(ca_path)?);
    }

    Ok(Client::from_conf(builder.build()))
}

/// Build an HTTPS client that trusts an extra PEM certificate (e.g. a private
/// Ceph RGW CA or a self-signed certificate).
pub fn custom_http_client(
    ca_path: &Path,
) -> anyhow::Result<aws_smithy_runtime_api::client::http::SharedHttpClient> {
    use aws_smithy_http_client::tls::rustls_provider::CryptoMode;
    use aws_smithy_http_client::tls::{Provider, TlsContext, TrustStore};
    use aws_smithy_http_client::Builder;

    let pem = std::fs::read(ca_path)
        .map_err(|e| anyhow::anyhow!("failed to read {}: {e}", ca_path.display()))?;

    let trust_store = TrustStore::default().with_pem_certificate(pem);
    let tls_context = TlsContext::builder()
        .with_trust_store(trust_store)
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build TLS context: {e}"))?;

    let client = Builder::new()
        .tls_provider(Provider::Rustls(CryptoMode::AwsLc))
        .tls_context(tls_context)
        .build_https();

    Ok(client)
}
