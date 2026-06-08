//! Archive encoding, decoding, and checksum helpers.

use crate::error::FutureMetaError;
use crate::model::{FeeArchiveV1, SCHEMA_VERSION};
use sha2::{Digest, Sha256};

/// Encodes an archive payload into compressed artifact bytes.
///
/// # Errors
///
/// Returns [`FutureMetaError::CorruptArchive`] if bincode serialization or zstd
/// compression fails.
pub fn encode_archive_bytes(archive: &FeeArchiveV1) -> Result<Vec<u8>, FutureMetaError> {
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
pub fn decode_archive_bytes(bytes: &[u8]) -> Result<FeeArchiveV1, FutureMetaError> {
    let decoded = zstd::stream::decode_all(bytes)
        .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    let (archive, consumed): (FeeArchiveV1, usize) =
        bincode::serde::decode_from_slice(&decoded, bincode::config::standard())
            .map_err(|err| FutureMetaError::CorruptArchive(err.to_string()))?;
    if consumed != decoded.len() {
        return Err(FutureMetaError::CorruptArchive(
            "archive contains trailing bytes".to_owned(),
        ));
    }

    if archive.schema_version != SCHEMA_VERSION {
        return Err(FutureMetaError::UnsupportedSchemaVersion {
            found: archive.schema_version,
            supported: SCHEMA_VERSION,
        });
    }

    Ok(archive)
}

/// Computes the SHA-256 checksum as lowercase hexadecimal text.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}
