//! Import retained CZCE daily clearing parameters as complete fee tuples.

use crate::db;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::Url;
use rusqlite::OptionalExtension;
use scraper::{Html, Selector};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, Month, OffsetDateTime};

/// Inputs for one offline, hash-verified CZCE parameter import.
#[derive(Debug, Clone)]
pub struct CzceParameterImportOptions {
    pub history_db: PathBuf,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub observed_at: String,
}

/// Counts returned after a successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CzceParameterImportResult {
    pub snapshots: usize,
    pub contracts: usize,
    pub versions: usize,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    requested_date: String,
    status: String,
    sha256: String,
    url: String,
    byte_count: Option<usize>,
    content_type: String,
}

#[derive(Debug, Clone)]
struct Observation {
    symbol: String,
    valid_from: String,
    fees: [FeeSpec; 3],
    canonical_url: String,
    body_sha256: String,
}

/// Validate retained exchange bytes and materialize CZCE fee history.
///
/// CZCE defines `交易手续费` as both open and ordinary close commission.
/// `日内平今仓交易手续费` supplies close-today commission. Absolute values
/// are yuan per lot; proportional values are per ten-thousand of turnover.
///
/// # Errors
///
/// Returns an error for non-official URLs, hash mismatches, malformed tables,
/// incomplete fee tuples, unknown units, or database write failures.
#[allow(clippy::too_many_lines)]
pub fn import_daily_parameters(
    options: &CzceParameterImportOptions,
) -> Result<CzceParameterImportResult> {
    let loaded = load_observations(options)?;
    if loaded.observations.is_empty() {
        bail!(
            "CZCE parameter import has no observations on or after {}",
            options.from
        );
    }

    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    let mut last_observed = BTreeMap::<String, String>::new();
    let mut corpus_last = String::new();
    for observation in loaded.observations {
        corpus_last = corpus_last.max(observation.valid_from.clone());
        last_observed.insert(observation.symbol.clone(), observation.valid_from.clone());
        let entries = by_symbol.entry(observation.symbol.clone()).or_default();
        if entries
            .last()
            .is_some_and(|previous| previous.fees == observation.fees)
        {
            continue;
        }
        entries.push(observation);
    }

    let mut conn = db::connect(&options.history_db)?;
    db::ensure_schema(&conn)?;
    let product_metadata = db::product_static_metadata_candidates(&conn)?;
    let mut rows = Vec::new();
    for observations in by_symbol.values() {
        let first = observations
            .first()
            .ok_or_else(|| anyhow!("empty CZCE contract observation group"))?;
        let symbol_last = last_observed
            .get(&first.symbol)
            .ok_or_else(|| anyhow!("CZCE contract has no last observation"))?;
        let coverage_end_exclusive =
            (OffsetDateTime::parse(symbol_last, &Rfc3339)? + Duration::days(1)).format(&Rfc3339)?;
        let metadata = conn
            .query_row(
                "select listing_date, expiry_date, lot_size, tick_size
                 from contracts where symbol = ?1",
                [&first.symbol],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                },
            )
            .optional()?;
        let inferred_listing = first.valid_from[..10].replace('-', "");
        let inferred_expiry =
            (symbol_last < &corpus_last).then(|| symbol_last[..10].replace('-', ""));
        let metadata = if let Some(mut metadata) = metadata {
            if metadata.0.is_none() {
                metadata.0 = Some(inferred_listing);
            }
            if metadata.1.is_none() {
                metadata.1 = inferred_expiry;
            }
            metadata
        } else {
            let product = derive_underlying_symbol(&first.symbol)?;
            let candidates = product_metadata
                .get(&product)
                .ok_or_else(|| anyhow!("CZCE contract metadata missing for {}", first.symbol))?;
            if candidates.len() != 1 {
                bail!(
                    "CZCE contract metadata is ambiguous for {}: {} candidates",
                    first.symbol,
                    candidates.len()
                );
            }
            (
                Some(inferred_listing),
                inferred_expiry,
                candidates[0].lot_size,
                candidates[0].tick_size,
            )
        };
        for observation in observations {
            rows.push(db::OfficialParameterRow {
                row: AllowedRow {
                    symbol: observation.symbol.clone(),
                    listing_date: metadata.0.clone(),
                    expiry_date: metadata.1.clone(),
                    trading_status: TradingStatus::Unknown,
                    buy_margin_rate: None,
                    sell_margin_rate: None,
                    open_fee: observation.fees[0].clone(),
                    close_yesterday_fee: observation.fees[1].clone(),
                    close_today_fee: observation.fees[2].clone(),
                    lot_size: metadata.2,
                    tick_size: metadata.3,
                    source_updated_at: Some(observation.valid_from.clone()),
                    is_main_contract: false,
                },
                coverage_end_exclusive: coverage_end_exclusive.clone(),
                canonical_url: observation.canonical_url.clone(),
                body_sha256: observation.body_sha256.clone(),
            });
        }
    }

    let versions =
        db::replace_with_official_parameter_history(&mut conn, &rows, &options.observed_at)?;

    Ok(CzceParameterImportResult {
        snapshots: loaded.snapshots,
        contracts: by_symbol.len(),
        versions,
    })
}

struct LoadedObservations {
    observations: Vec<Observation>,
    snapshots: usize,
}

fn load_observations(options: &CzceParameterImportOptions) -> Result<LoadedObservations> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut observations = Vec::new();
    let mut snapshots = 0usize;
    for record in reader.deserialize::<ManifestRow>() {
        let record = record?;
        if record.status != "ok" {
            continue;
        }
        let date = parse_compact_date(&record.requested_date)?;
        if date < options.from {
            continue;
        }
        validate_manifest_row(&record)?;
        let path = options.snapshot_dir.join(format!("{}.htm", record.sha256));
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read retained CZCE snapshot {}", path.display()))?;
        if record
            .byte_count
            .is_some_and(|expected| expected != bytes.len())
        {
            bail!("retained CZCE byte count mismatch for {}", record.url);
        }
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != record.sha256 {
            bail!("retained CZCE SHA-256 mismatch for {}", record.url);
        }
        let html = std::str::from_utf8(&bytes).context("CZCE snapshot is not UTF-8")?;
        snapshots += 1;
        observations.extend(parse_parameter_html(
            html,
            date,
            &record.url,
            &record.sha256,
        )?);
    }
    Ok(LoadedObservations {
        observations,
        snapshots,
    })
}

fn validate_manifest_row(row: &ManifestRow) -> Result<()> {
    if row.sha256.len() != 64
        || !row
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid CZCE SHA-256 for {}", row.requested_date);
    }
    if !row.content_type.to_ascii_lowercase().contains("text/html") {
        bail!("unexpected CZCE content type for {}", row.url);
    }
    let url = Url::parse(&row.url)?;
    if url.scheme() != "https" || url.host_str() != Some("www.czce.com.cn") {
        bail!("CZCE parameter URL must use official HTTPS domain");
    }
    let expected = format!(
        "/cn/DFSStaticFiles/Future/{}/{}/FutureDataClearParams.htm",
        &row.requested_date[..4],
        row.requested_date
    );
    if url.path() != expected || url.query().is_some() || url.fragment().is_some() {
        bail!("unexpected CZCE parameter URL {}", row.url);
    }
    Ok(())
}

fn parse_parameter_html(
    html: &str,
    date: Date,
    canonical_url: &str,
    body_sha256: &str,
) -> Result<Vec<Observation>> {
    let document = Html::parse_document(html);
    let table_selector = Selector::parse("table").expect("valid selector");
    let row_selector = Selector::parse("tr").expect("valid selector");
    let cell_selector = Selector::parse("th, td").expect("valid selector");
    for table in document.select(&table_selector) {
        let rows = table
            .select(&row_selector)
            .map(|row| {
                row.select(&cell_selector)
                    .map(|cell| cell.text().collect::<String>().split_whitespace().collect())
                    .collect::<Vec<String>>()
            })
            .collect::<Vec<_>>();
        let Some((header_index, headers)) = rows.iter().enumerate().find(|(_, cells)| {
            cells.iter().any(|cell| cell == "合约代码")
                && cells.iter().any(|cell| cell == "交易手续费")
        }) else {
            continue;
        };
        let symbol_index = column(headers, &["合约代码"])?;
        let base_index = column(headers, &["交易手续费"])?;
        let close_today_index = column(headers, &["平今仓手续费", "日内平今仓交易手续费"])?;
        let mode_index = headers.iter().position(|cell| cell == "手续费收取方式");
        let mut result = Vec::new();
        for cells in rows.iter().skip(header_index + 1) {
            let Some(instrument) = cells.get(symbol_index) else {
                continue;
            };
            if !is_czce_instrument(instrument) {
                continue;
            }
            let mode = mode_index
                .and_then(|index| cells.get(index))
                .map_or("", String::as_str);
            let open = parse_fee(cells.get(base_index), mode)?;
            let close_today = parse_fee(cells.get(close_today_index), mode)?;
            result.push(Observation {
                symbol: format!("CZCE.{instrument}"),
                valid_from: format!("{date}T00:00:00+08:00"),
                fees: [open.clone(), open, close_today],
                canonical_url: canonical_url.to_owned(),
                body_sha256: body_sha256.to_owned(),
            });
        }
        if !result.is_empty() {
            return Ok(result);
        }
    }
    bail!("no CZCE fee parameter table in {canonical_url}")
}

fn column(headers: &[String], names: &[&str]) -> Result<usize> {
    headers
        .iter()
        .position(|header| names.contains(&header.as_str()))
        .ok_or_else(|| anyhow!("CZCE parameter table missing column {}", names.join("/")))
}

fn parse_fee(value: Option<&String>, mode: &str) -> Result<FeeSpec> {
    let raw = value
        .ok_or_else(|| anyhow!("CZCE parameter fee is missing"))?
        .trim();
    let number = raw.replace(',', "").parse::<f64>()?;
    if !number.is_finite() || number < 0.0 {
        bail!("CZCE parameter fee must be finite and non-negative");
    }
    if number == 0.0 {
        return Ok(FeeSpec {
            kind: FeeKind::Zero,
            value: Some(0.0),
            raw_text: Some(raw.to_owned()),
        });
    }
    let kind = match mode.trim() {
        "" | "绝对值" => FeeKind::CnyPerLot,
        "比例值" => FeeKind::TurnoverRatePerTenThousand,
        other => bail!("unknown CZCE fee collection mode {other}"),
    };
    Ok(FeeSpec {
        kind,
        value: Some(number),
        raw_text: Some(raw.to_owned()),
    })
}

fn is_czce_instrument(value: &str) -> bool {
    let letters = value.bytes().take_while(u8::is_ascii_uppercase).count();
    (1..=3).contains(&letters)
        && (value.len() - letters == 3 || value.len() - letters == 4)
        && value.as_bytes()[letters..].iter().all(u8::is_ascii_digit)
}

fn parse_compact_date(value: &str) -> Result<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid CZCE requested date {value}");
    }
    let year = value[..4].parse::<i32>()?;
    let month = Month::try_from(value[4..6].parse::<u8>()?)?;
    let day = value[6..].parse::<u8>()?;
    Date::from_calendar_date(year, month, day).map_err(Into::into)
}
