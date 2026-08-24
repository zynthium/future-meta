//! Import retained DCE daily settlement parameters with complete fee tuples.
//!
//! The DCE report application is protected by a browser-issued request token.
//! Acquisition therefore happens outside this importer; this module accepts only
//! hash-verified raw JSON snapshots and their date-bound manifest.

use crate::db;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::{SymbolKind, parse_symbol};
use reqwest::Url;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime};

/// Inputs for one offline, hash-verified DCE settlement-parameter import.
#[derive(Debug, Clone)]
pub struct DceParameterImportOptions {
    pub history_db: PathBuf,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub observed_at: String,
}

/// Counts returned after a successful DCE import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DceParameterImportResult {
    pub snapshots: usize,
    pub contracts: usize,
    pub versions: usize,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    requested_date: String,
    status: String,
    report_date: String,
    sha256: String,
    url: String,
    record_count: Option<usize>,
    content_type: String,
}

#[derive(Debug, Deserialize)]
struct ParameterDocument {
    success: bool,
    code: i64,
    data: Vec<ParameterRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParameterRow {
    contract_id: String,
    open_fee: serde_json::Value,
    offset_fee: serde_json::Value,
    short_offset_fee: serde_json::Value,
    style: String,
}

#[derive(Debug, Clone)]
struct Observation {
    symbol: String,
    valid_from: String,
    fees: [FeeSpec; 3],
    canonical_url: String,
    body_sha256: String,
}

type ExistingMetadata = (Option<String>, Option<String>, f64, f64);

/// Materialize official DCE fee history from retained settlement responses.
///
/// DCE's `openFee`, `offsetFee`, and `shortOffsetFee` map respectively to
/// open, close-yesterday, and close-today fees. The report's `style` declares
/// whether values are yuan-per-lot or turnover rates per ten thousand.
pub fn import_daily_settlement_parameters(
    options: &DceParameterImportOptions,
) -> Result<DceParameterImportResult> {
    let (mut observations, snapshots) = load_observations(options)?;
    if observations.is_empty() {
        bail!("DCE parameter import has no in-range observations");
    }
    observations.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then_with(|| left.valid_from.cmp(&right.valid_from))
    });

    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    let mut last_observed = BTreeMap::<String, String>::new();
    for observation in observations {
        last_observed
            .entry(observation.symbol.clone())
            .and_modify(|latest| {
                if observation.valid_from > *latest {
                    *latest = observation.valid_from.clone();
                }
            })
            .or_insert_with(|| observation.valid_from.clone());
        let entries = by_symbol.entry(observation.symbol.clone()).or_default();
        if entries
            .last()
            .is_none_or(|previous| previous.fees != observation.fees)
        {
            entries.push(observation);
        }
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let history_rows = materialize_rows(&connection, &by_symbol, &last_observed)?;
    if history_rows.is_empty() {
        bail!("DCE parameter observations contain no known futures contracts");
    }
    let contracts = history_rows
        .iter()
        .map(|row| row.row.symbol.as_str())
        .collect::<BTreeSet<_>>()
        .len();
    let versions = db::replace_with_official_parameter_history(
        &mut connection,
        &history_rows,
        &options.observed_at,
    )?;
    Ok(DceParameterImportResult {
        snapshots,
        contracts,
        versions,
    })
}

fn load_observations(options: &DceParameterImportOptions) -> Result<(Vec<Observation>, usize)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut observations = Vec::new();
    let mut snapshots = 0;
    for entry in reader.deserialize::<ManifestRow>() {
        let entry = entry?;
        if entry.status != "ok" {
            continue;
        }
        let date = parse_compact_date(&entry.report_date)?;
        if date < options.from {
            continue;
        }
        validate_manifest_row(&entry)?;
        let path = options.snapshot_dir.join(format!("{}.json", entry.sha256));
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read retained DCE snapshot {}", path.display()))?;
        if hex::encode(Sha256::digest(&bytes)) != entry.sha256 {
            bail!("retained DCE SHA-256 mismatch for {}", entry.url);
        }
        let document: ParameterDocument = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse DCE settlement response {}", entry.report_date))?;
        if !document.success || document.code != 200 {
            bail!("DCE settlement response unsuccessful {}", entry.report_date);
        }
        if entry
            .record_count
            .is_some_and(|expected| expected != document.data.len())
        {
            bail!("DCE settlement record count mismatch {}", entry.report_date);
        }
        snapshots += 1;
        for row in document.data {
            let symbol = format!("DCE.{}", row.contract_id.trim().to_ascii_lowercase());
            if !is_futures_symbol(&symbol) {
                continue;
            }
            observations.push(Observation {
                symbol,
                valid_from: format!("{date}T00:00:00+08:00"),
                fees: [
                    parse_fee(&row.open_fee, &row.style)?,
                    parse_fee(&row.offset_fee, &row.style)?,
                    parse_fee(&row.short_offset_fee, &row.style)?,
                ],
                canonical_url: entry.url.clone(),
                body_sha256: entry.sha256.clone(),
            });
        }
    }
    Ok((observations, snapshots))
}

fn materialize_rows(
    connection: &rusqlite::Connection,
    by_symbol: &BTreeMap<String, Vec<Observation>>,
    last_observed: &BTreeMap<String, String>,
) -> Result<Vec<db::OfficialHistoryRow>> {
    let mut result = Vec::new();
    for observations in by_symbol.values() {
        let first = observations
            .first()
            .ok_or_else(|| anyhow!("DCE observation group unexpectedly empty"))?;
        let Some((listing, expiry, lot_size, tick_size)) =
            load_existing_metadata(connection, &first.symbol)?
        else {
            continue;
        };
        let symbol_last = last_observed
            .get(&first.symbol)
            .ok_or_else(|| anyhow!("DCE observation last date missing {}", first.symbol))?;
        let coverage_end =
            (OffsetDateTime::parse(symbol_last, &Rfc3339)? + Duration::days(1)).format(&Rfc3339)?;
        for observation in observations {
            result.push(db::OfficialHistoryRow {
                row: AllowedRow {
                    symbol: observation.symbol.clone(),
                    listing_date: listing.clone(),
                    expiry_date: expiry.clone(),
                    trading_status: TradingStatus::Unknown,
                    buy_margin_rate: None,
                    sell_margin_rate: None,
                    open_fee: observation.fees[0].clone(),
                    close_yesterday_fee: observation.fees[1].clone(),
                    close_today_fee: observation.fees[2].clone(),
                    lot_size,
                    tick_size,
                    source_updated_at: Some(observation.valid_from.clone()),
                    is_main_contract: false,
                },
                coverage_end_exclusive: coverage_end.clone(),
                evidence_level: db::OfficialEvidenceLevel::OfficialParameter,
                evidence: vec![db::OfficialEvidenceReference {
                    canonical_url: observation.canonical_url.clone(),
                    body_sha256: observation.body_sha256.clone(),
                }],
            });
        }
    }
    Ok(result)
}

fn load_existing_metadata(
    connection: &rusqlite::Connection,
    symbol: &str,
) -> Result<Option<ExistingMetadata>> {
    Ok(connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size from contracts where symbol = ?1",
            [symbol],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

fn validate_manifest_row(row: &ManifestRow) -> Result<()> {
    if row.requested_date != row.report_date {
        bail!("DCE requested/report date mismatch");
    }
    parse_compact_date(&row.requested_date)?;
    if row.sha256.len() != 64
        || !row
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid DCE SHA-256 {}", row.report_date);
    }
    if !row.content_type.to_ascii_lowercase().contains("json") {
        bail!("DCE parameter response not JSON {}", row.report_date);
    }
    let url = Url::parse(&row.url)?;
    if url.scheme() != "http"
        || url.host_str() != Some("www.dce.com.cn")
        || url.path() != "/dcereport/publicweb/tradepara/futAndOptSettle"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unexpected DCE settlement URL {}", row.url);
    }
    Ok(())
}

fn parse_compact_date(value: &str) -> Result<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid DCE compact date {value}");
    }
    let format = time::format_description::parse("[year][month][day]")?;
    Ok(Date::parse(value, &format)?)
}

fn is_futures_symbol(symbol: &str) -> bool {
    parse_symbol(symbol).is_ok_and(|parsed| parsed.kind == SymbolKind::Futures)
}

fn parse_fee(value: &serde_json::Value, style: &str) -> Result<FeeSpec> {
    let value = match value {
        serde_json::Value::Number(value) => value
            .as_f64()
            .ok_or_else(|| anyhow!("DCE fee number cannot be represented"))?,
        serde_json::Value::String(value) => value
            .trim()
            .parse::<f64>()
            .with_context(|| format!("invalid DCE fee value {value}"))?,
        _ => bail!("invalid DCE fee value type"),
    };
    if !value.is_finite() || value < 0.0 {
        bail!("invalid DCE fee value {value}");
    }
    if value == 0.0 {
        return Ok(FeeSpec {
            kind: FeeKind::Zero,
            value: Some(0.0),
            raw_text: Some("0".to_owned()),
        });
    }
    let kind = match style.trim() {
        "绝对值" => FeeKind::CnyPerLot,
        "比例值" => FeeKind::TurnoverRatePerTenThousand,
        other => bail!("unknown DCE fee style {other}"),
    };
    Ok(FeeSpec {
        kind,
        value: Some(value),
        raw_text: Some(value.to_string()),
    })
}
