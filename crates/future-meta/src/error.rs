//! Error types for future-meta client APIs.

use thiserror::Error;

/// Error type used by future-meta client APIs.
#[derive(Debug, Error)]
pub enum FutureMetaError {
    /// The supplied symbol could not be parsed.
    #[error("invalid symbol: {0}")]
    InvalidSymbol(String),
    /// The symbol kind is recognized but not supported by this operation.
    #[error("unsupported symbol kind: {0}")]
    UnsupportedSymbolKind(String),
    /// The requested contract is unknown.
    #[error("unknown contract: {0}")]
    UnknownContract(String),
    /// The supplied pre-resolved contract handle does not belong to this client.
    #[error("invalid contract handle")]
    InvalidContractHandle,
    /// The requested underlying product symbol is unknown.
    #[error("unknown underlying symbol: {0}")]
    UnknownUnderlyingSymbol(String),
    /// No metadata version is available at the requested time.
    #[error("no metadata version available at: {0}")]
    NoVersionAt(String),
    /// The supplied as-of timestamp could not be parsed.
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),
    /// The requested date predates the retained history.
    #[error("not available before history start")]
    NotAvailableBeforeHistoryStart,
    /// Archive schema version is newer than this client supports.
    #[error("unsupported schema version: found {found}, supported {supported}")]
    UnsupportedSchemaVersion {
        /// Schema version found in the archive.
        found: u32,
        /// Highest schema version supported by this client.
        supported: u32,
    },
    /// Archive checksum validation failed.
    #[error("checksum mismatch for {path}: expected {expected}, actual {actual}")]
    ChecksumMismatch {
        /// Path whose contents failed checksum validation.
        path: String,
        /// Expected checksum value.
        expected: String,
        /// Actual checksum value.
        actual: String,
    },
    /// Archive data is malformed or incomplete.
    #[error("corrupt archive: {0}")]
    CorruptArchive(String),
    /// Download failed.
    #[error("download failed: {0}")]
    DownloadFailed(String),
    /// I/O failure.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON serialization or deserialization failure.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

/// Error type returned by as-of queries.
pub type AsOfError = FutureMetaError;
