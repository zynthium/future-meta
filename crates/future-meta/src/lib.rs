#![doc = "Shared data types and high-performance client APIs for future-meta."]

pub mod archive;
#[cfg(feature = "download")]
pub mod download;
pub mod error;
pub mod fee;
pub mod model;
pub mod query;
pub mod symbol;

#[cfg(feature = "download")]
pub use crate::download::{DownloadConfig, load_or_fetch};
pub use crate::error::{AsOfError, FutureMetaError};
pub use crate::model::{Contract, ContractFee, FeeArchiveV1, FeeSpec, Manifest};
pub use crate::query::FutureMeta;

/// Crate version exported for clients and compatibility checks.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
