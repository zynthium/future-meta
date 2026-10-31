//! Source-neutral normalized parameters shared by official import paths.

use future_meta::model::{FeeSpec, TradingStatus};
use serde::Serialize;

/// Source row normalized down to fields allowed for history and publishing.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AllowedRow {
    /// TqSdk-style futures contract symbol.
    pub symbol: String,
    /// Listing date from the source when present.
    pub listing_date: Option<String>,
    /// Expiry date from the source when present.
    pub expiry_date: Option<String>,
    /// Trading status reported by the source.
    pub trading_status: TradingStatus,
    /// Long-side margin percentage points when known, e.g. `12.0` means 12%.
    pub buy_margin_rate: Option<f64>,
    /// Short-side margin percentage points when known, e.g. `12.0` means 12%.
    pub sell_margin_rate: Option<f64>,
    /// Open position fee rule.
    pub open_fee: FeeSpec,
    /// Close-yesterday position fee rule.
    pub close_yesterday_fee: FeeSpec,
    /// Close-today position fee rule.
    pub close_today_fee: FeeSpec,
    /// Published intraday opening fee, distinct from the ordinary opening fee.
    pub intraday_open_fee: Option<FeeSpec>,
    /// Number of units per lot.
    pub lot_size: f64,
    /// Minimum price tick.
    pub tick_size: f64,
    /// Source fee update timestamp when present.
    pub source_updated_at: Option<String>,
    /// Whether the source remark marks this as a main contract.
    pub is_main_contract: bool,
}

/// Validated contract multiplier and tick size used by official imports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContractStaticMetadata {
    /// Number of units per lot.
    pub lot_size: f64,
    /// Minimum price tick.
    pub tick_size: f64,
}
