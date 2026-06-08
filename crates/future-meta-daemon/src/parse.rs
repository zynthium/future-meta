//! CSV parsing into allowed-field rows.

use anyhow::{Result, anyhow};
use csv::StringRecord;
use future_meta::fee::{parse_fee_spec, parse_optional_f64};
use future_meta::model::{FeeSpec, TradingStatus};
use future_meta::symbol::normalize_futures_symbol;

/// Source row normalized down to fields allowed for history and publishing.
#[derive(Debug, Clone, PartialEq)]
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
    /// Number of units per lot.
    pub lot_size: f64,
    /// Minimum price tick.
    pub tick_size: f64,
    /// Source fee update timestamp when present.
    pub source_updated_at: Option<String>,
    /// Whether the source remark marks this as a main contract.
    pub is_main_contract: bool,
}

/// Parse a 9qihuo CSV snapshot into allowed rows.
///
/// # Errors
///
/// Returns an error when required headers are missing, CSV parsing fails, or a
/// non-empty row has an invalid futures symbol.
pub fn parse_csv(csv_text: &str) -> Result<Vec<AllowedRow>> {
    let mut reader = csv::Reader::from_reader(csv_text.as_bytes());
    let headers = reader.headers()?.clone();
    let column = |name: &str| -> Result<usize> {
        headers
            .iter()
            .position(|header| header.trim() == name)
            .ok_or_else(|| anyhow!("missing CSV header {name}"))
    };

    let contract_col = column("合约代码")?;
    let exchange_col = column("交易所编码")?;
    let listed_col = column("上市日期")?;
    let expires_col = column("到期日期")?;
    let status_col = column("是否正在交易")?;
    let buy_margin_col = column("买开保证金%")?;
    let sell_margin_col = column("卖开保证金%")?;
    let open_fee_col = column("开仓手续费")?;
    let close_yesterday_col = column("平昨手续费")?;
    let close_today_col = column("平今手续费")?;
    let lot_size_col = column("每手数量")?;
    let tick_size_col = column("每跳价差")?;
    let source_updated_col = column("手续费更新时间")?;
    let remark_col = column("备注")?;

    let mut rows = Vec::new();
    for (line, record) in reader.records().enumerate() {
        let record = record?;
        let line_number = line + 2;
        if record.iter().all(|value| value.trim().is_empty()) {
            continue;
        }

        let exchange = field(&record, exchange_col);
        let local = field(&record, contract_col);
        if exchange.is_empty() || local.is_empty() {
            return Err(anyhow!(
                "missing required identity field at CSV line {line_number}"
            ));
        }

        rows.push(AllowedRow {
            symbol: normalize_futures_symbol(exchange, local)?,
            listing_date: non_empty(record.get(listed_col)),
            expiry_date: non_empty(record.get(expires_col)),
            trading_status: parse_trading_status(field(&record, status_col)),
            buy_margin_rate: parse_optional_f64(field(&record, buy_margin_col)),
            sell_margin_rate: parse_optional_f64(field(&record, sell_margin_col)),
            open_fee: parse_fee_spec(field(&record, open_fee_col)),
            close_yesterday_fee: parse_fee_spec(field(&record, close_yesterday_col)),
            close_today_fee: parse_fee_spec(field(&record, close_today_col)),
            lot_size: parse_required_positive_f64(
                "每手数量",
                field(&record, lot_size_col),
                line_number,
            )?,
            tick_size: parse_required_positive_f64(
                "每跳价差",
                field(&record, tick_size_col),
                line_number,
            )?,
            source_updated_at: non_empty(record.get(source_updated_col)),
            is_main_contract: parse_main_contract(field(&record, remark_col)),
        });
    }

    Ok(rows)
}

fn field(record: &StringRecord, column: usize) -> &str {
    record.get(column).unwrap_or("").trim()
}

fn non_empty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn parse_required_positive_f64(field_name: &str, text: &str, line_number: usize) -> Result<f64> {
    let value = parse_optional_f64(text).ok_or_else(|| {
        anyhow!("invalid required numeric field {field_name} at CSV line {line_number}: {text}")
    })?;
    if value <= 0.0 {
        return Err(anyhow!(
            "required numeric field {field_name} must be positive at CSV line {line_number}: {text}"
        ));
    }

    Ok(value)
}

fn parse_main_contract(text: &str) -> bool {
    matches!(text.trim(), "主力" | "主力合约")
}

fn parse_trading_status(text: &str) -> TradingStatus {
    let trimmed = text.trim();
    if trimmed == "交易中" || trimmed == "是" {
        return TradingStatus::Trading;
    }

    if trimmed == "否"
        || trimmed.contains("暂停")
        || trimmed.contains("未交易")
        || trimmed.contains("停牌")
        || trimmed.contains("非交易")
        || trimmed.contains("不交易")
        || trimmed.contains("停止交易")
    {
        return TradingStatus::NotTrading;
    }

    TradingStatus::Unknown
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::hash::{row_rule_hash, rule_set_hash};
    use future_meta::model::{FeeKind, TradingStatus};

    const BASE: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,13,64122,0.1元,0.2元,0.51/万分之,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n";
    const TWO_ROWS: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,13,64122,0.1元,0.2元,0.51/万分之,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n沪铝2607,al2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,暂停交易,10000,11000/9000,10,11,50000,3元,3元,0元,5,5,25,6,19,2026-03-27 22:56:54,\n";
    const TWO_ROWS_REVERSED: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铝2607,al2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,暂停交易,10000,11000/9000,10,11,50000,3元,3元,0元,5,5,25,6,19,2026-03-27 22:56:54,\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,13,64122,0.1元,0.2元,0.51/万分之,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n";

    #[test]
    fn parses_allowed_fields_and_drops_derived_fields() {
        let rows = parse_csv(BASE).unwrap();

        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.symbol, "SHFE.cu2607");
        assert_eq!(row.listing_date.as_deref(), Some("20250716"));
        assert_eq!(row.expiry_date.as_deref(), Some("20260715"));
        assert_eq!(row.trading_status, TradingStatus::Trading);
        assert_eq!(row.buy_margin_rate, Some(12.0));
        assert_eq!(row.sell_margin_rate, Some(13.0));
        assert_eq!(row.open_fee.kind, FeeKind::CnyPerLot);
        assert_eq!(row.open_fee.value, Some(0.1));
        assert_eq!(row.close_yesterday_fee.kind, FeeKind::CnyPerLot);
        assert_eq!(row.close_yesterday_fee.value, Some(0.2));
        assert_eq!(
            row.close_today_fee.kind,
            FeeKind::TurnoverRatePerTenThousand
        );
        assert_eq!(row.close_today_fee.value, Some(0.51));
        assert_f64_eq(row.lot_size, 5.0);
        assert_f64_eq(row.tick_size, 10.0);
        assert_eq!(
            row.source_updated_at.as_deref(),
            Some("2026-03-27 22:56:54")
        );
        assert!(row.is_main_contract);
    }

    #[test]
    fn derived_field_changes_do_not_change_rule_hash() {
        let changed = update_cells(
            BASE,
            &[
                ("现价", "999999"),
                ("涨/跌停板", "1/2"),
                ("保证金/每手(元)", "1"),
                ("每跳毛利/元", "999"),
                ("手续费(开+平)/元", "99"),
                ("每跳净利/元", "-999"),
                ("手续费更新时间", "2030-01-01 00:00:00"),
            ],
        );
        let base_hash = row_rule_hash(&parse_csv(BASE).unwrap()[0]);
        let changed_hash = row_rule_hash(&parse_csv(&changed).unwrap()[0]);

        assert_eq!(base_hash, changed_hash);
    }

    #[test]
    fn rule_field_changes_change_rule_hash() {
        let changed = update_cells(BASE, &[("开仓手续费", "0.2元")]);
        let base_hash = row_rule_hash(&parse_csv(BASE).unwrap()[0]);
        let changed_hash = row_rule_hash(&parse_csv(&changed).unwrap()[0]);

        assert_ne!(base_hash, changed_hash);
    }

    #[test]
    fn rule_set_hash_is_independent_of_input_row_order() {
        let first = parse_csv(TWO_ROWS).unwrap();
        let second = parse_csv(TWO_ROWS_REVERSED).unwrap();

        assert_eq!(rule_set_hash(&first), rule_set_hash(&second));
    }

    #[test]
    fn rule_hash_rejects_non_finite_numbers() {
        let mut row = parse_csv(BASE).unwrap().remove(0);
        row.open_fee.value = Some(f64::NAN);

        let panic = std::panic::catch_unwind(|| row_rule_hash(&row));

        assert!(panic.is_err());
    }

    #[test]
    fn rejects_missing_identity_in_non_blank_row() {
        let missing_exchange = update_cells(BASE, &[("交易所编码", "")]);

        let err = parse_csv(&missing_exchange).unwrap_err();

        assert!(err.to_string().contains("missing required identity"));
    }

    #[test]
    fn rejects_invalid_required_contract_numbers() {
        let missing_lot_size = update_cells(BASE, &[("每手数量", "")]);
        let bad_tick_size = update_cells(BASE, &[("每跳价差", "NaN")]);

        assert!(parse_csv(&missing_lot_size).is_err());
        assert!(parse_csv(&bad_tick_size).is_err());
    }

    #[test]
    fn parses_main_contract_and_trading_status_precisely() {
        let non_main = update_cells(BASE, &[("备注", "非主力")]);
        let trading_yes = update_cells(BASE, &[("是否正在交易", "是")]);

        assert!(!parse_csv(&non_main).unwrap()[0].is_main_contract);
        assert_eq!(
            parse_csv(&trading_yes).unwrap()[0].trading_status,
            TradingStatus::Trading
        );
    }

    fn update_cells(csv_text: &str, replacements: &[(&str, &str)]) -> String {
        let mut reader = csv::Reader::from_reader(csv_text.as_bytes());
        let headers = reader.headers().unwrap().clone();
        let record = reader.records().next().unwrap().unwrap();
        let mut fields = record.iter().map(str::to_owned).collect::<Vec<_>>();

        for (header, replacement) in replacements {
            let index = headers
                .iter()
                .position(|candidate| candidate == *header)
                .unwrap_or_else(|| panic!("missing test header {header}"));
            fields[index] = (*replacement).to_owned();
        }

        let mut writer = csv::Writer::from_writer(Vec::new());
        writer.write_record(&headers).unwrap();
        writer.write_record(&fields).unwrap();
        String::from_utf8(writer.into_inner().unwrap()).unwrap()
    }

    fn assert_f64_eq(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {expected}, got {actual}"
        );
    }
}
