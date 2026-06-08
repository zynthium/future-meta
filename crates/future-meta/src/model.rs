//! Shared archive and contract data models.

use serde::{Deserialize, Serialize};

/// Current archive schema version.
pub const SCHEMA_VERSION: u32 = 1;

/// Published artifact manifest metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Schema version used by the artifact.
    pub schema_version: u32,
    /// Logical data version for cache invalidation.
    pub data_version: String,
    /// Generation timestamp.
    pub generated_at: String,
    /// Earliest date covered by the artifact.
    pub history_start: String,
    /// Latest date covered by the artifact.
    pub history_end: String,
    /// Artifact path or URL.
    pub artifact: String,
    /// SHA-256 checksum of the artifact bytes.
    pub sha256: String,
    /// Artifact size in bytes.
    pub size: u64,
    /// Alternate artifact URLs.
    pub mirrors: Vec<String>,
}

/// Version 1 fee archive payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeArchiveV1 {
    /// Schema version used by this archive.
    pub schema_version: u32,
    /// Generation timestamp.
    pub generated_at: String,
    /// Earliest date covered by this archive.
    pub history_start: String,
    /// Latest date covered by this archive.
    pub history_end: String,
    /// Contract metadata table.
    pub contracts: Vec<Contract>,
    /// Fee records over time.
    pub fee_versions: Vec<ContractFee>,
}

/// Futures contract metadata.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Contract {
    /// Stable numeric id inside the archive.
    pub id: u32,
    /// TqSdk-style contract symbol.
    pub symbol: String,
    /// Listing date when known.
    pub listing_date: Option<String>,
    /// Expiry date when known.
    pub expiry_date: Option<String>,
    /// Number of units per lot.
    pub lot_size: f64,
    /// Minimum price tick.
    pub tick_size: f64,
    /// Whether the contract is currently active.
    pub active: bool,
}

/// Fee rule for a contract over a validity interval.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContractFee {
    /// Contract id from the contract table.
    pub contract_id: u32,
    /// Hash identifying the normalized fee rule.
    pub rule_hash: String,
    /// Long-side margin rate when known.
    pub buy_margin_rate: Option<f64>,
    /// Short-side margin rate when known.
    pub sell_margin_rate: Option<f64>,
    /// Open position fee.
    pub open_fee: FeeSpec,
    /// Close yesterday position fee.
    pub close_yesterday_fee: FeeSpec,
    /// Close today position fee.
    pub close_today_fee: FeeSpec,
    /// Trading status reported by the source.
    pub trading_status: TradingStatus,
    /// Whether this record refers to the main contract.
    pub is_main_contract: bool,
    /// Source update timestamp when provided.
    pub source_updated_at: Option<String>,
    /// Inclusive first date this rule is valid.
    pub valid_from: String,
    /// Exclusive end date this rule is valid; `None` means open-ended/current.
    pub valid_to: Option<String>,
}

/// Trading availability status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TradingStatus {
    /// The contract is trading.
    Trading,
    /// The contract is not trading.
    NotTrading,
    /// The source did not provide a known status.
    Unknown,
}

/// Parsed fee specification with original text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FeeSpec {
    /// Parsed fee unit.
    pub kind: FeeKind,
    /// Parsed numeric value when available.
    pub value: Option<f64>,
    /// Trimmed source text when meaningful.
    pub raw_text: Option<String>,
}

/// Supported fee units.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FeeKind {
    /// Fixed CNY amount per lot.
    CnyPerLot,
    /// Turnover rate expressed per ten thousand.
    TurnoverRatePerTenThousand,
    /// Explicitly zero fee.
    Zero,
    /// Unknown or unsupported fee text.
    Unknown,
}
