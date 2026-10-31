//! Canonical allowed-field hashing.

use crate::model::AllowedRow;
use future_meta::model::{FeeKind, FeeSpec};
use serde::Serialize;

/// Hash the published fee legs for one row.
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
        intraday_open: row.intraday_open_fee.as_ref().map(canonical_fee),
    };
    let text =
        serde_json::to_string(&canonical).expect("canonical allowed row should serialize to JSON");

    digest_text(&text)
}

/// Identity of a complete parameter state, separate from fee-evidence identity.
///
/// # Panics
///
/// Panics if the row contains non-finite numeric values or cannot be serialized.
#[must_use]
pub fn row_state_hash(row: &AllowedRow) -> String {
    let tuple = (
        row_rule_hash(row),
        row.buy_margin_rate.map(f64::to_bits),
        row.sell_margin_rate.map(f64::to_bits),
    );
    digest_text(&serde_json::to_string(&tuple).expect("finite state should serialize"))
}

fn assert_finite_row(row: &AllowedRow) {
    assert_optional_finite(row.buy_margin_rate, "buy_margin_rate");
    assert_optional_finite(row.sell_margin_rate, "sell_margin_rate");
    assert_fee_spec_finite(&row.open_fee, "open_fee");
    assert_fee_spec_finite(&row.close_yesterday_fee, "close_yesterday_fee");
    assert_fee_spec_finite(&row.close_today_fee, "close_today_fee");
    if let Some(fee) = &row.intraday_open_fee {
        assert_fee_spec_finite(fee, "intraday_open_fee");
    }
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

#[derive(Serialize)]
struct CanonicalRow {
    open: CanonicalFee,
    close_yesterday: CanonicalFee,
    close_today: CanonicalFee,
    #[serde(skip_serializing_if = "Option::is_none")]
    intraday_open: Option<CanonicalFee>,
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

#[cfg(test)]
mod tests {
    use super::{row_rule_hash, row_state_hash};
    use crate::model::AllowedRow;
    use future_meta::model::{FeeKind, FeeSpec, TradingStatus};

    fn fee(kind: FeeKind, value: Option<f64>) -> FeeSpec {
        FeeSpec {
            kind,
            value,
            raw_text: None,
        }
    }

    fn parameter_row() -> AllowedRow {
        AllowedRow {
            symbol: "SHFE.cu2607".to_owned(),
            listing_date: Some("2025-07-16".to_owned()),
            expiry_date: Some("2026-07-15".to_owned()),
            trading_status: TradingStatus::Trading,
            buy_margin_rate: Some(12.0),
            sell_margin_rate: Some(13.0),
            open_fee: fee(FeeKind::CnyPerLot, Some(0.1)),
            close_yesterday_fee: fee(FeeKind::CnyPerLot, Some(0.2)),
            close_today_fee: fee(FeeKind::TurnoverRatePerTenThousand, Some(0.51)),
            intraday_open_fee: None,
            lot_size: 5.0,
            tick_size: 10.0,
            source_updated_at: None,
            is_main_contract: false,
        }
    }

    #[test]
    fn canonical_hashes_remain_compatible() {
        let row = parameter_row();
        assert_eq!(
            row_rule_hash(&row),
            "7fc9f043074c2db149aed4f498df5534b9e9ebf6a30d20d410c6e6086d1739f9"
        );
        assert_eq!(
            row_state_hash(&row),
            "43bfbb613fcc34310aaa1e038a2ddbc23ff2213d0d8add79318da42233257405"
        );
    }

    #[test]
    fn observation_and_metadata_do_not_rekey_parameter_states() {
        let row = parameter_row();
        let mut changed = row.clone();
        changed.source_updated_at = Some("2026-07-01T08:00:00Z".to_owned());
        changed.listing_date = Some("2025-07-17".to_owned());
        changed.expiry_date = Some("2026-07-16".to_owned());
        changed.trading_status = TradingStatus::NotTrading;
        changed.is_main_contract = true;
        changed.lot_size = 10.0;
        changed.tick_size = 1.0;
        changed.open_fee.raw_text = Some("0.10元".to_owned());
        assert_eq!(row_rule_hash(&row), row_rule_hash(&changed));
        assert_eq!(row_state_hash(&row), row_state_hash(&changed));
    }

    #[test]
    fn margins_change_state_without_rekeying_fee_evidence() {
        let row = parameter_row();
        let mut changed = row.clone();
        changed.buy_margin_rate = Some(14.0);
        assert_eq!(row_rule_hash(&row), row_rule_hash(&changed));
        assert_ne!(row_state_hash(&row), row_state_hash(&changed));
        changed = row.clone();
        changed.sell_margin_rate = None;
        assert_eq!(row_rule_hash(&row), row_rule_hash(&changed));
        assert_ne!(row_state_hash(&row), row_state_hash(&changed));
    }

    #[test]
    fn intraday_open_fee_changes_rule_and_state() {
        let row = parameter_row();
        let mut changed = row.clone();
        changed.intraday_open_fee = Some(fee(FeeKind::Zero, Some(0.0)));
        assert_ne!(row_rule_hash(&row), row_rule_hash(&changed));
        assert_ne!(row_state_hash(&row), row_state_hash(&changed));
    }

    #[test]
    fn equivalent_zero_units_have_the_same_rule_hash() {
        let mut row = parameter_row();
        row.open_fee = fee(FeeKind::Zero, Some(0.0));
        let mut changed = row.clone();
        changed.open_fee = fee(FeeKind::CnyPerLot, Some(-0.0));
        assert_eq!(row_rule_hash(&row), row_rule_hash(&changed));
        changed.open_fee = fee(FeeKind::TurnoverRatePerTenThousand, Some(0.0));
        assert_eq!(row_rule_hash(&row), row_rule_hash(&changed));
    }

    #[test]
    #[should_panic(expected = "open_fee must be finite")]
    fn rejects_non_finite_rule_values() {
        let mut row = parameter_row();
        row.open_fee.value = Some(f64::NAN);
        let _ = row_rule_hash(&row);
    }
}
