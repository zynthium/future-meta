//! Jin10 daily futures-fee snapshot parsing.

use crate::parse::AllowedRow;
use anyhow::{Result, anyhow};
use future_meta::fee::{parse_fee_spec, parse_optional_f64};
use future_meta::model::TradingStatus;
use future_meta::symbol::{derive_underlying_symbol, normalize_futures_symbol};
use serde::Deserialize;
use serde_json::json;
use std::collections::BTreeMap;
use time::Date;
use time::format_description;

/// Jin10 public futures-fee endpoint.
pub const API_URL: &str = "https://mp-api.jin10.com/api/dynamic-data/child";

/// Product-level static data required to retain a historical fee row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContractStaticMetadata {
    /// Number of units per lot.
    pub lot_size: f64,
    /// Minimum price tick.
    pub tick_size: f64,
}

/// A parsed Jin10 daily fee snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    /// Rows completed with validated product static metadata.
    pub rows: Vec<AllowedRow>,
    /// Source rows skipped because their contract codes are unsupported.
    pub skipped_invalid_symbols: usize,
    /// Source rows skipped because product static metadata is unavailable.
    pub skipped_missing_metadata: usize,
}

/// A parsed Jin10 snapshot tied to its source snapshot date.
#[derive(Debug, Clone, PartialEq)]
pub struct DatedSnapshot {
    /// First safe effective day after source snapshot publication in China Standard Time.
    pub observed_at: String,
    /// Completed rows from that source snapshot day.
    pub snapshot: Snapshot,
}

/// Build a Jin10 request URL for an inclusive date range.
///
/// # Errors
///
/// Returns an error when the fixed source endpoint cannot be parsed.
pub fn range_url(from: &str, to: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(API_URL)?;
    let search = json!({
        "range,date": format!("{from},{to}"),
        "status": 1,
    });
    url.query_pairs_mut()
        .append_pair("tb_name", "_vir_26")
        .append_pair("search", &search.to_string());
    Ok(url)
}

/// Parse a Jin10 response into rows safe for fee-history insertion.
///
/// Jin10 does not expose lot size or tick size. The caller supplies verified
/// current product metadata and every completed row is checked against the
/// source's per-tick value before it can be returned.
///
/// # Errors
///
/// Returns an error when response structure, values, exchange identity, or
/// static-metadata verification is invalid.
pub fn parse_snapshot(
    json: &str,
    metadata_by_product: &BTreeMap<String, ContractStaticMetadata>,
) -> Result<Snapshot> {
    let candidates = one_candidate_per_product(metadata_by_product);
    parse_snapshot_with_candidates(json, &candidates)
}

/// Parse a Jin10 response using all known static-metadata candidates.
///
/// The source's per-tick value must select exactly one candidate; otherwise
/// the response is rejected rather than guessing a historical contract spec.
///
/// # Errors
///
/// Returns an error when no unique static-metadata candidate verifies a row.
pub fn parse_snapshot_with_candidates(
    json: &str,
    metadata_by_product: &BTreeMap<String, Vec<ContractStaticMetadata>>,
) -> Result<Snapshot> {
    let mut snapshots = parse_snapshots_with_candidates(json, metadata_by_product)?;
    if snapshots.len() != 1 {
        return Err(anyhow!(
            "expected one Jin10 snapshot date, found {}",
            snapshots.len()
        ));
    }

    snapshots
        .pop()
        .map(|snapshot| snapshot.snapshot)
        .ok_or_else(|| anyhow!("expected one Jin10 snapshot date, found none"))
}

/// Parse a Jin10 response that may contain multiple source snapshot dates.
///
/// # Errors
///
/// Returns an error when response structure, values, exchange identity,
/// snapshot date, or static-metadata verification is invalid.
pub fn parse_snapshots(
    json: &str,
    metadata_by_product: &BTreeMap<String, ContractStaticMetadata>,
) -> Result<Vec<DatedSnapshot>> {
    let candidates = one_candidate_per_product(metadata_by_product);
    parse_snapshots_with_candidates(json, &candidates)
}

/// Parse a Jin10 response using all known static-metadata candidates.
///
/// # Errors
///
/// Returns an error when response structure, values, exchange identity,
/// snapshot date, or static-metadata verification is invalid.
pub fn parse_snapshots_with_candidates(
    json: &str,
    metadata_by_product: &BTreeMap<String, Vec<ContractStaticMetadata>>,
) -> Result<Vec<DatedSnapshot>> {
    let response: Response = serde_json::from_str(json)?;
    if response.status != 200 {
        return Err(anyhow!("Jin10 response status is {}", response.status));
    }

    let mut source_rows_by_date = BTreeMap::<String, Vec<SourceRow>>::new();
    for source_row in response.data {
        source_rows_by_date
            .entry(source_row.snapshot_date.clone())
            .or_default()
            .push(source_row);
    }

    let mut snapshots = Vec::with_capacity(source_rows_by_date.len());
    for (date, source_rows) in source_rows_by_date {
        let observed_at = observed_at(&date)?;
        let source_updated_at = source_updated_at(&observed_at);
        let mut rows = Vec::new();
        let mut skipped_invalid_symbols = 0usize;
        let mut skipped_missing_metadata = 0usize;
        for source_row in latest_rows_by_contract(source_rows)? {
            let exchange = exchange_code(&source_row.exchange)?;
            let Ok(symbol) = normalize_futures_symbol(exchange, &source_row.contract_code) else {
                skipped_invalid_symbols += 1;
                continue;
            };
            let product = derive_underlying_symbol(&symbol)?;
            let Some(candidates) = metadata_by_product.get(&product) else {
                skipped_missing_metadata += 1;
                continue;
            };
            let metadata = select_static_metadata(&symbol, candidates, &source_row.per_ratio)?;
            validate_static_metadata(&symbol, &metadata, &source_row.per_ratio)?;

            rows.push(AllowedRow {
                symbol,
                listing_date: None,
                expiry_date: None,
                trading_status: trading_status(source_row.status)?,
                buy_margin_rate: parse_margin(&source_row.buy_margin_rate, "buy_ratio")?,
                sell_margin_rate: parse_margin(&source_row.sell_margin_rate, "sell_ratio")?,
                open_fee: parse_jin10_fee(&source_row.open_fee),
                // Jin10's API field names are inverted against its published table:
                // sell_cur is 平昨 and sell_yesterday is 平今.
                close_yesterday_fee: parse_jin10_fee(&source_row.close_current_fee),
                close_today_fee: parse_jin10_fee(&source_row.close_yesterday_fee),
                lot_size: metadata.lot_size,
                tick_size: metadata.tick_size,
                // `pub_date_commission` is stale on some rows where dynamic
                // rules change, so the source snapshot date is the only
                // consistent daily as-of boundary.
                source_updated_at: Some(source_updated_at.clone()),
                is_main_contract: false,
            });
        }

        snapshots.push(DatedSnapshot {
            observed_at,
            snapshot: Snapshot {
                rows,
                skipped_invalid_symbols,
                skipped_missing_metadata,
            },
        });
    }

    Ok(snapshots)
}

fn one_candidate_per_product(
    metadata_by_product: &BTreeMap<String, ContractStaticMetadata>,
) -> BTreeMap<String, Vec<ContractStaticMetadata>> {
    metadata_by_product
        .iter()
        .map(|(product, metadata)| (product.clone(), vec![*metadata]))
        .collect()
}

#[derive(Debug, Deserialize)]
struct Response {
    status: i64,
    data: Vec<SourceRow>,
}

#[derive(Debug, Deserialize)]
struct SourceRow {
    #[serde(rename = "date")]
    snapshot_date: String,
    #[serde(rename = "heyue_code")]
    contract_code: String,
    #[serde(rename = "buy_ratio")]
    buy_margin_rate: String,
    #[serde(rename = "sell_ratio")]
    sell_margin_rate: String,
    #[serde(rename = "buy_commission")]
    open_fee: String,
    #[serde(rename = "sell_cur_commission")]
    close_current_fee: String,
    #[serde(rename = "sell_yesterday_commission")]
    close_yesterday_fee: String,
    #[serde(rename = "per_ratio")]
    per_ratio: String,
    #[serde(rename = "jys")]
    exchange: String,
    status: i64,
    #[serde(default)]
    updated_at: Option<String>,
}

fn latest_rows_by_contract(source_rows: Vec<SourceRow>) -> Result<Vec<SourceRow>> {
    let mut latest_by_contract = BTreeMap::<String, SourceRow>::new();
    for source_row in source_rows {
        let key = format!("{}\u{1f}{}", source_row.exchange, source_row.contract_code);
        let Some(existing) = latest_by_contract.get(&key) else {
            latest_by_contract.insert(key, source_row);
            continue;
        };
        let (Some(existing_updated_at), Some(source_updated_at)) =
            (&existing.updated_at, &source_row.updated_at)
        else {
            return Err(anyhow!(
                "duplicate Jin10 contract row without comparable updated_at: {}",
                source_row.contract_code
            ));
        };
        if source_updated_at == existing_updated_at {
            return Err(anyhow!(
                "duplicate Jin10 contract row with identical updated_at: {}",
                source_row.contract_code
            ));
        }
        if source_updated_at > existing_updated_at {
            latest_by_contract.insert(key, source_row);
        }
    }

    Ok(latest_by_contract.into_values().collect())
}

fn exchange_code(name: &str) -> Result<&'static str> {
    match name.trim() {
        "上海期货交易所" => Ok("SHFE"),
        "大连商品交易所" => Ok("DCE"),
        "郑州商品交易所" => Ok("CZCE"),
        "中国金融期货交易所" => Ok("CFFEX"),
        "上海国际能源交易中心" => Ok("INE"),
        "广州期货交易所" => Ok("GFEX"),
        other => Err(anyhow!("unknown Jin10 exchange: {other}")),
    }
}

fn parse_margin(value: &str, field: &str) -> Result<Option<f64>> {
    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed == "-" {
        return Ok(None);
    }

    parse_optional_f64(trimmed.strip_suffix('%').unwrap_or(trimmed))
        .map(Some)
        .ok_or_else(|| anyhow!("invalid Jin10 {field}: {value}"))
}

fn parse_jin10_fee(value: &str) -> future_meta::model::FeeSpec {
    parse_fee_spec(
        value
            .split_once('(')
            .map_or(value, |(primary, _)| primary)
            .trim(),
    )
}

fn trading_status(value: i64) -> Result<TradingStatus> {
    match value {
        1 => Ok(TradingStatus::Trading),
        0 => Ok(TradingStatus::NotTrading),
        other => Err(anyhow!("unknown Jin10 row status: {other}")),
    }
}

fn select_static_metadata(
    symbol: &str,
    candidates: &[ContractStaticMetadata],
    per_ratio: &str,
) -> Result<ContractStaticMetadata> {
    let actual = parse_optional_f64(per_ratio)
        .ok_or_else(|| anyhow!("invalid Jin10 per_ratio for {symbol}: {per_ratio}"))?;
    let matches = candidates
        .iter()
        .copied()
        .filter(|candidate| per_ratio_matches(candidate.lot_size * candidate.tick_size, actual))
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [metadata] => Ok(*metadata),
        [] => Err(anyhow!(
            "Jin10 per_ratio has no static metadata match for {symbol}: {actual}"
        )),
        _ => Err(anyhow!(
            "Jin10 per_ratio has ambiguous static metadata matches for {symbol}: {actual}"
        )),
    }
}

fn validate_static_metadata(
    symbol: &str,
    metadata: &ContractStaticMetadata,
    per_ratio: &str,
) -> Result<()> {
    if !metadata.lot_size.is_finite() || metadata.lot_size <= 0.0 {
        return Err(anyhow!(
            "invalid lot_size for {symbol}: {}",
            metadata.lot_size
        ));
    }
    if !metadata.tick_size.is_finite() || metadata.tick_size <= 0.0 {
        return Err(anyhow!(
            "invalid tick_size for {symbol}: {}",
            metadata.tick_size
        ));
    }
    let actual = parse_optional_f64(per_ratio)
        .ok_or_else(|| anyhow!("invalid Jin10 per_ratio for {symbol}: {per_ratio}"))?;
    let expected = metadata.lot_size * metadata.tick_size;
    if !per_ratio_matches(expected, actual) {
        return Err(anyhow!(
            "Jin10 per_ratio does not match static metadata for {symbol}: expected {expected}, got {actual}"
        ));
    }

    Ok(())
}

fn per_ratio_matches(expected: f64, actual: f64) -> bool {
    let tolerance = expected.abs().max(actual.abs()).max(1.0) * 1e-9;
    (expected - actual).abs() <= tolerance
}

fn observed_at(date: &str) -> Result<String> {
    let format = format_description::parse("[year]-[month]-[day]")?;
    let snapshot_date = Date::parse(date, &format)
        .map_err(|err| anyhow!("invalid Jin10 snapshot date {date}: {err}"))?;
    let effective_date = snapshot_date
        .next_day()
        .ok_or_else(|| anyhow!("Jin10 snapshot date has no following day: {date}"))?;
    Ok(format!(
        "{}T00:00:00+08:00",
        effective_date.format(&format)?
    ))
}

fn source_updated_at(observed_at: &str) -> String {
    observed_at.replace("T00:00:00+08:00", " 00:00:00")
}
