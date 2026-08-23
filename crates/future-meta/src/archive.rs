//! Archive encoding, decoding, and checksum helpers.

use crate::error::FutureMetaError;
use crate::model::{FeeArchiveV1, FeeArchiveV2, LEGACY_SCHEMA_VERSION, SCHEMA_VERSION};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// Encodes an archive payload into compressed artifact bytes.
///
/// # Errors
///
/// Returns [`FutureMetaError::CorruptArchive`] if bincode serialization or zstd
/// compression fails.
pub fn encode_archive_bytes(archive: &impl Serialize) -> Result<Vec<u8>, FutureMetaError> {
    let encoded = bincode::serde::encode_to_vec(archive, bincode::config::standard())
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;

    zstd::stream::encode_all(encoded.as_slice(), 19)
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))
}

/// Decodes compressed artifact bytes into an archive payload.
///
/// # Errors
///
/// Returns [`FutureMetaError::CorruptArchive`] if zstd decompression or bincode
/// deserialization fails. Returns [`FutureMetaError::UnsupportedSchemaVersion`]
/// when the archive schema is not supported by this client.
pub fn decode_archive_bytes(bytes: &[u8]) -> Result<FeeArchiveV2, FutureMetaError> {
    let decoded = zstd::stream::decode_all(bytes)
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    let (schema_version, _): (u32, usize) =
        bincode::serde::decode_from_slice(&decoded, bincode::config::standard())
            .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    match schema_version {
        LEGACY_SCHEMA_VERSION => decode_exact::<FeeArchiveV1>(&decoded).map(Into::into),
        SCHEMA_VERSION => decode_exact::<FeeArchiveV2>(&decoded),
        found => Err(FutureMetaError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        }),
    }
}

fn decode_exact<T>(decoded: &[u8]) -> Result<T, FutureMetaError>
where
    T: serde::de::DeserializeOwned,
{
    let (archive, consumed): (T, usize) =
        bincode::serde::decode_from_slice(decoded, bincode::config::standard())
            .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    if consumed != decoded.len() {
        return Err(FutureMetaError::CorruptArchive(
            "archive contains trailing bytes".to_owned(),
        ));
    }
    Ok(archive)
}

/// Computes the SHA-256 checksum as lowercase hexadecimal text.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
