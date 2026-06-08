//! Optional archive download and cache support.

use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use crate::archive::{decode_archive_bytes, sha256_hex};
use crate::error::FutureMetaError;
use crate::model::Manifest;
use crate::query::FutureMeta;

const DEFAULT_MANIFEST_URL: &str = "https://future-meta.pages.dev/manifest.json";
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

/// Load a cached archive or fetch the current manifest and artifact.
///
/// # Errors
///
/// Returns an error when the manifest/artifact cannot be downloaded, the
/// manifest JSON cannot be parsed, checksum validation fails, or the archive
/// cannot be decoded and indexed.
pub async fn load_or_fetch(config: DownloadConfig) -> Result<FutureMeta, FutureMetaError> {
    tokio::fs::create_dir_all(&config.cache_dir).await?;
    let client = reqwest::Client::builder()
        .timeout(DOWNLOAD_TIMEOUT)
        .build()
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?;

    let manifest_text = fetch_text(&client, &config.manifest_url).await?;
    let manifest: Manifest = serde_json::from_str(&manifest_text)?;
    let artifact_path = artifact_cache_path(&config.cache_dir, &manifest.artifact)?;

    let bytes = load_cached_or_fetch_artifact(&client, &artifact_path, &manifest).await?;

    let archive = decode_archive_bytes(&bytes)?;
    FutureMeta::from_archive(archive)
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

async fn load_cached_or_fetch_artifact(
    client: &reqwest::Client,
    artifact_path: &Path,
    manifest: &Manifest,
) -> Result<Vec<u8>, FutureMetaError> {
    if tokio::fs::try_exists(artifact_path).await? {
        let bytes = tokio::fs::read(artifact_path).await?;
        if checksum_matches(&bytes, &manifest.sha256) {
            return Ok(bytes);
        }

        tokio::fs::remove_file(artifact_path).await?;
    }

    fetch_and_cache_artifact(client, artifact_path, manifest).await
}

async fn fetch_and_cache_artifact(
    client: &reqwest::Client,
    artifact_path: &Path,
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
            Ok(bytes) if checksum_matches(&bytes, &manifest.sha256) => {
                write_cache_file(artifact_path, &bytes).await?;
                return Ok(bytes);
            }
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

async fn fetch_text(client: &reqwest::Client, url: &str) -> Result<String, FutureMetaError> {
    client
        .get(url)
        .send()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .error_for_status()
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))?
        .text()
        .await
        .map_err(|err| FutureMetaError::DownloadFailed(err.to_string()))
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
