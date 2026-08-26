//! Canonical allowed-field hashing.

use crate::parse::AllowedRow;
use future_meta::model::{FeeKind, FeeSpec};
use serde::Serialize;

/// Hash the three fee-rule legs for one row.
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
        open: canonical_fee(&row.open_fee),
        close_yesterday: canonical_fee(&row.close_yesterday_fee),
        close_today: canonical_fee(&row.close_today_fee),
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
struct CanonicalRow {
    open: CanonicalFee,
    close_yesterday: CanonicalFee,
    close_today: CanonicalFee,
}

#[derive(Serialize)]
struct CanonicalFee {
    kind: &'static str,
    value_bits: Option<u64>,
}

fn canonical_fee(fee: &FeeSpec) -> CanonicalFee {
    if is_semantically_zero(fee) {
        return CanonicalFee {
            kind: "Zero",
            value_bits: Some(0.0_f64.to_bits()),
        };
    }

    CanonicalFee {
        kind: match fee.kind {
            FeeKind::CnyPerLot => "CnyPerLot",
            FeeKind::TurnoverRatePerTenThousand => "TurnoverRatePerTenThousand",
            FeeKind::Zero => "Zero",
            FeeKind::Unknown => "Unknown",
        },
        value_bits: fee.value.map(f64::to_bits),
    }
}

fn is_semantically_zero(fee: &FeeSpec) -> bool {
    fee.value == Some(0.0)
        && matches!(
            fee.kind,
            FeeKind::CnyPerLot | FeeKind::TurnoverRatePerTenThousand | FeeKind::Zero
        )
}

#[must_use]
fn digest_text(text: &str) -> String {
    use sha2::{Digest, Sha256};

    hex::encode(Sha256::digest(text.as_bytes()))
}
