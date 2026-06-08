//! Canonical allowed-field hashing.

use crate::parse::AllowedRow;
use future_meta::model::{FeeSpec, TradingStatus};
use serde::Serialize;

/// Hash the allowed identity and rule fields for one row.
///
/// The source update timestamp is intentionally excluded because it describes
/// observation time, not fee-rule identity.
///
/// # Panics
///
/// Panics if a manually constructed row contains a non-finite float, or if the
/// allowed row cannot be serialized to canonical JSON.
#[must_use]
pub fn row_rule_hash(row: &AllowedRow) -> String {
    assert_finite_row(row);
    let canonical = CanonicalRow {
        symbol: row.symbol.as_str(),
        listing_date: row.listing_date.as_deref(),
        expiry_date: row.expiry_date.as_deref(),
        trading_status: &row.trading_status,
        buy_margin_rate: row.buy_margin_rate,
        sell_margin_rate: row.sell_margin_rate,
        open_fee: &row.open_fee,
        close_yesterday_fee: &row.close_yesterday_fee,
        close_today_fee: &row.close_today_fee,
        lot_size: row.lot_size,
        tick_size: row.tick_size,
        is_main_contract: row.is_main_contract,
    };
    let text =
        serde_json::to_string(&canonical).expect("canonical allowed row should serialize to JSON");

    digest_text(&text)
}

fn assert_finite_row(row: &AllowedRow) {
    assert_optional_finite(row.buy_margin_rate, "buy_margin_rate");
    assert_optional_finite(row.sell_margin_rate, "sell_margin_rate");
    assert_fee_spec_finite(&row.open_fee, "open_fee");
    assert_fee_spec_finite(&row.close_yesterday_fee, "close_yesterday_fee");
    assert_fee_spec_finite(&row.close_today_fee, "close_today_fee");
    assert_finite(row.lot_size, "lot_size");
    assert_finite(row.tick_size, "tick_size");
}

fn assert_fee_spec_finite(fee: &FeeSpec, field: &str) {
    assert_optional_finite(fee.value, field);
}

fn assert_optional_finite(value: Option<f64>, field: &str) {
    if let Some(value) = value {
        assert_finite(value, field);
    }
}

fn assert_finite(value: f64, field: &str) {
    assert!(value.is_finite(), "{field} must be finite");
}

/// Hash an order-independent set of allowed rows.
#[must_use]
pub fn rule_set_hash(rows: &[AllowedRow]) -> String {
    let mut hashes = rows.iter().map(row_rule_hash).collect::<Vec<_>>();
    hashes.sort();
    digest_text(&hashes.join("\n"))
}

/// Hash a source's stable probe identity.
#[must_use]
pub fn source_probe_hash(csv_url: &str, detail_url: &str) -> String {
    digest_text(&format!("{csv_url}\n{detail_url}"))
}

#[derive(Serialize)]
struct CanonicalRow<'a> {
    symbol: &'a str,
    listing_date: Option<&'a str>,
    expiry_date: Option<&'a str>,
    trading_status: &'a TradingStatus,
    buy_margin_rate: Option<f64>,
    sell_margin_rate: Option<f64>,
    open_fee: &'a FeeSpec,
    close_yesterday_fee: &'a FeeSpec,
    close_today_fee: &'a FeeSpec,
    lot_size: f64,
    tick_size: f64,
    is_main_contract: bool,
}

#[must_use]
fn digest_text(text: &str) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(text.as_bytes()))
}
