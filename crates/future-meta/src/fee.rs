//! Fee rule helpers.

use crate::model::{FeeKind, FeeSpec};

/// Parses a source fee cell into a structured fee specification.
#[must_use]
pub fn parse_fee_spec(text: &str) -> FeeSpec {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return unknown_fee(None);
    }

    if let Some(value) = parse_zero_value(trimmed) {
        return FeeSpec {
            kind: FeeKind::Zero,
            value: Some(value),
            raw_text: Some(trimmed.to_owned()),
        };
    }

    if let Some(value_text) = trimmed.strip_suffix('元') {
        return match parse_optional_f64(value_text) {
            Some(value) => FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(value),
                raw_text: Some(trimmed.to_owned()),
            },
            None => unknown_fee(Some(trimmed.to_owned())),
        };
    }

    if let Some(value_text) = trimmed.strip_suffix("/万分之") {
        return match parse_optional_f64(value_text) {
            Some(value) => FeeSpec {
                kind: FeeKind::TurnoverRatePerTenThousand,
                value: Some(value),
                raw_text: Some(trimmed.to_owned()),
            },
            None => unknown_fee(Some(trimmed.to_owned())),
        };
    }

    unknown_fee(Some(trimmed.to_owned()))
}

/// Parses an optional floating-point field, ignoring grouping commas.
#[must_use]
pub fn parse_optional_f64(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return None;
    }

    trimmed
        .replace(',', "")
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

fn parse_zero_value(text: &str) -> Option<f64> {
    let value_text = text.strip_suffix('元').unwrap_or(text);
    parse_optional_f64(value_text).filter(|value| *value == 0.0)
}

fn unknown_fee(raw_text: Option<String>) -> FeeSpec {
    FeeSpec {
        kind: FeeKind::Unknown,
        value: None,
        raw_text,
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_fee_spec, parse_optional_f64};
    use crate::model::FeeKind;

    #[test]
    fn parses_cny_per_lot_fee() {
        let spec = parse_fee_spec("2元");

        assert_eq!(spec.kind, FeeKind::CnyPerLot);
        assert_eq!(spec.value, Some(2.0));
        assert_eq!(spec.raw_text.as_deref(), Some("2元"));
    }

    #[test]
    fn parses_turnover_rate_per_ten_thousand_fee() {
        let spec = parse_fee_spec("0.51/万分之");

        assert_eq!(spec.kind, FeeKind::TurnoverRatePerTenThousand);
        assert_eq!(spec.value, Some(0.51));
        assert_eq!(spec.raw_text.as_deref(), Some("0.51/万分之"));
    }

    #[test]
    fn parses_zero_fee() {
        let spec = parse_fee_spec("0");

        assert_eq!(spec.kind, FeeKind::Zero);
        assert_eq!(spec.value, Some(0.0));
    }

    #[test]
    fn preserves_unknown_fee_text() {
        let spec = parse_fee_spec("按交易所通知");

        assert_eq!(spec.kind, FeeKind::Unknown);
        assert_eq!(spec.value, None);
        assert_eq!(spec.raw_text.as_deref(), Some("按交易所通知"));
    }

    #[test]
    fn malformed_known_unit_fees_are_unknown() {
        for text in ["abc元", "abc/万分之"] {
            let spec = parse_fee_spec(text);

            assert_eq!(spec.kind, FeeKind::Unknown);
            assert_eq!(spec.value, None);
            assert_eq!(spec.raw_text.as_deref(), Some(text));
        }
    }

    #[test]
    fn ignores_empty_fee_text() {
        let empty = parse_fee_spec("");
        let dash = parse_fee_spec("-");

        assert_eq!(empty.kind, FeeKind::Unknown);
        assert_eq!(empty.value, None);
        assert_eq!(empty.raw_text, None);
        assert_eq!(dash.kind, FeeKind::Unknown);
        assert_eq!(dash.value, None);
        assert_eq!(dash.raw_text, None);
    }

    #[test]
    fn treats_zero_yuan_as_zero_fee() {
        let spec = parse_fee_spec("0元");

        assert_eq!(spec.kind, FeeKind::Zero);
        assert_eq!(spec.value, Some(0.0));
        assert_eq!(spec.raw_text.as_deref(), Some("0元"));
    }

    #[test]
    fn parses_optional_f64_text() {
        assert_eq!(parse_optional_f64(" 12 "), Some(12.0));
        assert_eq!(parse_optional_f64(""), None);
        assert_eq!(parse_optional_f64("-"), None);
        assert_eq!(parse_optional_f64("1,234.5"), Some(1234.5));
    }

    #[test]
    fn parse_optional_f64_rejects_non_finite() {
        assert_eq!(parse_optional_f64("NaN"), None);
        assert_eq!(parse_optional_f64("inf"), None);
        assert_eq!(parse_optional_f64("-inf"), None);
    }
}
