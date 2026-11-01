//! Optional archive download and cache support.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{ETAG, HeaderValue, IF_NONE_MATCH};

use crate::archive::sha256_hex;
use crate::error::FutureMetaError;
use crate::model::Manifest;
use crate::query::FutureMeta;

const DEFAULT_MANIFEST_URL: &str =
    "https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json";
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(20);

/// Download and cache configuration for loading published future-meta archives.
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    /// URL of the manifest JSON file.
    pub manifest_url: String,
    /// Directory used to cache downloaded artifacts.
    pub cache_dir: PathBuf,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        let cache_dir = std::env::var_os("FUTURE_META_CACHE_DIR").map_or_else(
            || std::env::temp_dir().join("future-meta-cache"),
            PathBuf::from,
        );

        Self {
            manifest_url: DEFAULT_MANIFEST_URL.to_owned(),
            cache_dir,
        }
    }
}

/// A loaded archive with a reusable HTTP client and conditional refresh support.
///
/// Keep this value for periodic checks. Unchanged content preserves the query
/// instance and its handles; changed content replaces it only after validation.
#[derive(Debug)]
pub struct DownloadClient {
    client: reqwest::Client,
    config: DownloadConfig,
    sha256: String,
    etag: Option<HeaderValue>,
    meta: FutureMeta,
}

impl DownloadClient {
    /// Fetch the manifest and load its verified archive, using the disk cache.
    ///
    /// # Errors
    /// Returns an error on download, manifest, checksum, archive or cache failure.
    pub async fn load(config: DownloadConfig) -> Result<Self, FutureMetaError> {
        let client = reqwest::Client::builder()
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?;
        let (manifest, etag) = fetch_manifest(&client, &config.manifest_url, None).await?;
        let manifest = manifest.ok_or_else(|| {
            FutureMetaError::DownloadFailed("initial manifest returned no content".to_owned())
        })?;
        let meta = load_manifest_archive(&client, &config, &manifest).await?;
        Ok(Self {
            client,
            config,
            sha256: manifest.sha256,
            etag,
            meta,
        })
    }

    /// Access the current immutable query snapshot without network or disk I/O.
    #[must_use]
    pub const fn meta(&self) -> &FutureMeta {
        &self.meta
    }

    /// Check for changes, returning whether the query snapshot was replaced.
    ///
    /// HTTP 304 and an unchanged archive SHA avoid disk reads and reindexing.
    /// A changed snapshot invalidates handles for the new instance; clones of
    /// the previous snapshot remain usable with their own handles.
    ///
    /// # Errors
    /// Returns an error on download, manifest, checksum, archive or cache failure.
    /// An error preserves the current query snapshot and its manifest validator.
    pub async fn refresh(&mut self) -> Result<bool, FutureMetaError> {
        let (manifest, etag) =
            fetch_manifest(&self.client, &self.config.manifest_url, self.etag.as_ref()).await?;
        let Some(manifest) = manifest else {
            if etag.is_some() {
                self.etag = etag;
            }
            return Ok(false);
        };
        validate_manifest(&self.config, &manifest)?;
        if manifest.sha256 == self.sha256 {
            self.etag = etag;
            return Ok(false);
        }
        let meta = load_manifest_archive(&self.client, &self.config, &manifest).await?;
        self.meta = meta;
        self.sha256 = manifest.sha256;
        self.etag = etag;
        Ok(true)
    }
}

/// Load a cached archive or fetch the current manifest and artifact.
///
/// Each call creates a query instance. For periodic checks that retain existing
/// indexes and handles when data is unchanged, keep a [`DownloadClient`] instead.
///
/// # Errors
///
/// Returns an error when the manifest/artifact cannot be downloaded, the
/// manifest JSON cannot be parsed, checksum validation fails, or the archive
/// cannot be decoded and indexed.
pub async fn load_or_fetch(config: DownloadConfig) -> Result<FutureMeta, FutureMetaError> {
    Ok(DownloadClient::load(config).await?.meta)
}

fn validate_manifest(
    config: &DownloadConfig,
    manifest: &Manifest,
) -> Result<PathBuf, FutureMetaError> {
    if !(crate::model::LEGACY_SCHEMA_VERSION..=crate::model::SCHEMA_VERSION)
        .contains(&manifest.schema_version)
    {
        return Err(FutureMetaError::UnsupportedSchemaVersion {
            found: manifest.schema_version,
            supported: crate::model::SCHEMA_VERSION,
        });
    }
    artifact_cache_path(&config.cache_dir, &manifest.artifact)
}

async fn load_manifest_archive(
    client: &reqwest::Client,
    config: &DownloadConfig,
    manifest: &Manifest,
) -> Result<FutureMeta, FutureMetaError> {
    let artifact_path = validate_manifest(config, manifest)?;
    tokio::fs::create_dir_all(&config.cache_dir).await?;
    if tokio::fs::try_exists(&artifact_path).await? {
        let bytes = tokio::fs::read(&artifact_path).await?;
        if checksum_matches(&bytes, &manifest.sha256) {
            return FutureMeta::from_compressed_bytes(bytes).await;
        }
    }
    let bytes = fetch_artifact(client, manifest).await?;
    let meta = FutureMeta::from_compressed_bytes(bytes.clone()).await?;
    write_cache_file(&artifact_path, &bytes).await?;
    Ok(meta)
}

fn artifact_cache_path(cache_dir: &Path, artifact: &str) -> Result<PathBuf, FutureMetaError> {
    let mut components = Path::new(artifact).components();
    let file_name = match (components.next(), components.next()) {
        (Some(Component::Normal(file_name)), None) => file_name
            .to_str()
            .filter(|value| !value.is_empty() && !value.contains('/') && !value.contains('\\')),
        _ => None,
    }
    .ok_or_else(|| {
        FutureMetaError::DownloadFailed("manifest artifact is not a safe file name".to_owned())
    })?;

    Ok(cache_dir.join(file_name))
}

async fn fetch_artifact(
    client: &reqwest::Client,
    manifest: &Manifest,
) -> Result<Vec<u8>, FutureMetaError> {
    if manifest.mirrors.is_empty() {
        return Err(FutureMetaError::DownloadFailed(
            "manifest has no mirrors".to_owned(),
        ));
    }

    let mut last_error = None;
    for artifact_url in &manifest.mirrors {
        match fetch_bytes(client, artifact_url).await {
            Ok(bytes) if checksum_matches(&bytes, &manifest.sha256) => return Ok(bytes),
            Ok(bytes) => {
                last_error = Some(FutureMetaError::ChecksumMismatch {
                    path: artifact_url.clone(),
                    expected: manifest.sha256.clone(),
                    actual: sha256_hex(&bytes),
                });
            }
            Err(err) => {
                last_error = Some(err);
            }
        }
    }

    Err(last_error
        .unwrap_or_else(|| FutureMetaError::DownloadFailed("manifest has no mirrors".to_owned())))
}

async fn fetch_manifest(
    client: &reqwest::Client,
    url: &str,
    etag: Option<&HeaderValue>,
) -> Result<(Option<Manifest>, Option<HeaderValue>), FutureMetaError> {
    let mut request = client.get(url);
    if let Some(etag) = etag {
        request = request.header(IF_NONE_MATCH, etag);
    }
    let response = request
        .send()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .error_for_status()
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?;
    let next_etag = response.headers().get(ETAG).cloned();
    if response.status() == StatusCode::NOT_MODIFIED {
        if etag.is_none() {
            return Err(FutureMetaError::DownloadFailed(
                "manifest returned 304 without a validator".to_owned(),
            ));
        }
        return Ok((None, next_etag));
    }
    let text = response
        .text()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?;
    Ok((Some(serde_json::from_str(&text)?), next_etag))
}

async fn fetch_bytes(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, FutureMetaError> {
    client
        .get(url)
        .send()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .error_for_status()
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .bytes()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))
        .map(|bytes| bytes.to_vec())
}

fn checksum_matches(bytes: &[u8], expected: &str) -> bool {
    sha256_hex(bytes) == expected
}

async fn write_cache_file(path: &Path, bytes: &[u8]) -> Result<(), FutureMetaError> {
    let tmp_path = path.with_extension("tmp");
    tokio::fs::write(&tmp_path, bytes).await?;
    tokio::fs::rename(&tmp_path, path).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::artifact_cache_path;

    #[test]
    fn default_manifest_uses_data_branch() {
        assert_eq!(
            super::DownloadConfig::default().manifest_url,
            "https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json"
        );
    }

    #[test]
    fn artifact_cache_path_accepts_plain_file_name() {
        let path = artifact_cache_path(std::path::Path::new("cache"), "latest.fmeta.zst")
            .expect("plain file name should be accepted");

        assert_eq!(path, std::path::Path::new("cache").join("latest.fmeta.zst"));
    }

    #[test]
    fn artifact_cache_path_rejects_unsafe_names() {
        for artifact in [
            "",
            ".",
            "..",
            "../latest.fmeta.zst",
            "/tmp/latest.fmeta.zst",
            "artifacts/latest.fmeta.zst",
            r"artifacts\latest.fmeta.zst",
        ] {
            assert!(
                artifact_cache_path(std::path::Path::new("cache"), artifact).is_err(),
                "{artifact} should be rejected"
            );
        }
    }
}
