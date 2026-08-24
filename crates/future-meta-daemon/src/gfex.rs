//! Import retained GFEX daily settlement parameters as complete fee tuples.

use crate::db;
use crate::jin10::ContractStaticMetadata;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::Url;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime};

/// Inputs for one offline, hash-verified GFEX parameter import.
#[derive(Debug, Clone)]
pub struct GfexParameterImportOptions {
    pub history_db: PathBuf,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub observed_at: String,
}

/// Counts returned after successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfexParameterImportResult {
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
    code: String,
    data: Vec<ParameterRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParameterRow {
    contract_id: String,
    open_fee: f64,
    offset_fee: f64,
    short_offset_fee: f64,
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

/// Validate retained daily settlement bytes and materialize GFEX fee history.
///
/// GFEX fields map as follows: `openFee` is open commission, `offsetFee` is
/// ordinary close commission, and `shortOffsetFee` is close-today commission.
///
/// # Errors
///
/// Returns an error for invalid manifests, URLs, digests, documents, fee units,
/// incomplete tuples, ambiguous static metadata, or database failures.
pub fn import_daily_settlement_parameters(
    options: &GfexParameterImportOptions,
) -> Result<GfexParameterImportResult> {
    let (observations, snapshots) = load_observations(options)?;
    if observations.is_empty() {
        bail!("GFEX parameter import has no in-range observations");
    }
    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    let mut last_observed = BTreeMap::<String, String>::new();
    let mut corpus_last = String::new();
    for observation in observations {
        corpus_last = corpus_last.max(observation.valid_from.clone());
        last_observed.insert(observation.symbol.clone(), observation.valid_from.clone());
        let rows = by_symbol.entry(observation.symbol.clone()).or_default();
        if rows
            .last()
            .is_none_or(|previous| previous.fees != observation.fees)
        {
            rows.push(observation);
        }
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let metadata = db::product_static_metadata_candidates(&connection)?;
    let history_rows = materialize_rows(
        &connection,
        &metadata,
        &by_symbol,
        &last_observed,
        &corpus_last,
    )?;
    let versions = db::replace_with_official_parameter_history(
        &mut connection,
        &history_rows,
        &options.observed_at,
    )?;
    Ok(GfexParameterImportResult {
        snapshots,
        contracts: by_symbol.len(),
        versions,
    })
}

fn load_observations(options: &GfexParameterImportOptions) -> Result<(Vec<Observation>, usize)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut observations = Vec::new();
    let mut snapshots = 0;
    for record in reader.deserialize::<ManifestRow>() {
        let record = record?;
        if record.status != "ok" {
            continue;
        }
        let date = parse_compact_date(&record.report_date)?;
        if date < options.from {
            continue;
        }
        validate_manifest_row(&record)?;
        let path = options.snapshot_dir.join(format!("{}.json", record.sha256));
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read retained GFEX snapshot {}", path.display()))?;
        let actual = hex::encode(Sha256::digest(&bytes));
        if actual != record.sha256 {
            bail!("retained GFEX SHA-256 mismatch for {}", record.url);
        }
        let document: ParameterDocument = serde_json::from_slice(&bytes)?;
        if document.code != "0" {
            bail!(
                "GFEX settlement document is unsuccessful for {}",
                record.report_date
            );
        }
        if record
            .record_count
            .is_some_and(|expected| expected != document.data.len())
        {
            bail!(
                "GFEX settlement record count mismatch for {}",
                record.report_date
            );
        }
        snapshots += 1;
        for row in document.data {
            if !is_contract_id(&row.contract_id) {
                continue;
            }
            observations.push(Observation {
                symbol: format!("GFEX.{}", row.contract_id.to_ascii_lowercase()),
                valid_from: format!("{date}T00:00:00+08:00"),
                fees: [
                    parse_fee(row.open_fee, &row.style)?,
                    parse_fee(row.offset_fee, &row.style)?,
                    parse_fee(row.short_offset_fee, &row.style)?,
                ],
                canonical_url: record.url.clone(),
                body_sha256: record.sha256.clone(),
            });
        }
    }
    Ok((observations, snapshots))
}

fn materialize_rows(
    connection: &rusqlite::Connection,
    product_metadata: &BTreeMap<String, Vec<ContractStaticMetadata>>,
    by_symbol: &BTreeMap<String, Vec<Observation>>,
    last_observed: &BTreeMap<String, String>,
    corpus_last: &str,
) -> Result<Vec<db::OfficialHistoryRow>> {
    let mut result = Vec::new();
    for observations in by_symbol.values() {
        let first = observations
            .first()
            .ok_or_else(|| anyhow!("empty GFEX observation group"))?;
        let symbol_last = last_observed
            .get(&first.symbol)
            .ok_or_else(|| anyhow!("GFEX contract has no last observation"))?;
        let coverage_end =
            (OffsetDateTime::parse(symbol_last, &Rfc3339)? + Duration::days(1)).format(&Rfc3339)?;
        let existing = load_existing_metadata(connection, &first.symbol)?;
        let inferred_listing = first.valid_from[..10].replace('-', "");
        let inferred_expiry =
            (symbol_last.as_str() < corpus_last).then(|| symbol_last[..10].replace('-', ""));
        let (listing, expiry, lot_size, tick_size) = if let Some(mut existing) = existing {
            existing.0.get_or_insert(inferred_listing);
            if existing.1.is_none() {
                existing.1 = inferred_expiry;
            }
            existing
        } else {
            let product = derive_underlying_symbol(&first.symbol)?;
            let candidates = product_metadata
                .get(&product)
                .ok_or_else(|| anyhow!("GFEX contract metadata missing for {}", first.symbol))?;
            let candidate = select_static_metadata(&product, &first.valid_from, candidates)?;
            (
                Some(inferred_listing),
                inferred_expiry,
                candidate.lot_size,
                candidate.tick_size,
            )
        };
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

fn select_static_metadata<'a>(
    product: &str,
    valid_from: &str,
    candidates: &'a [ContractStaticMetadata],
) -> Result<&'a ContractStaticMetadata> {
    if let [candidate] = candidates {
        return Ok(candidate);
    }
    let expected_tick = match product {
        "GFEX.lc" if valid_from < "2024-12-18T00:00:00+08:00" => 50.0_f64,
        "GFEX.lc" => 20.0_f64,
        _ => bail!("GFEX contract metadata is ambiguous for {product}"),
    };
    candidates
        .iter()
        .find(|candidate| candidate.tick_size.to_bits() == expected_tick.to_bits())
        .ok_or_else(|| anyhow!("GFEX expected historical tick is missing for {product}"))
}

type ExistingMetadata = (Option<String>, Option<String>, f64, f64);

fn load_existing_metadata(
    connection: &rusqlite::Connection,
    symbol: &str,
) -> Result<Option<ExistingMetadata>> {
    Ok(connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size
             from contracts where symbol = ?1",
            [symbol],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

fn validate_manifest_row(row: &ManifestRow) -> Result<()> {
    if row.requested_date != row.report_date {
        bail!("GFEX requested/report date mismatch");
    }
    if row.sha256.len() != 64
        || !row
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid GFEX SHA-256 for {}", row.report_date);
    }
    if !row.content_type.to_ascii_lowercase().contains("json") {
        bail!("unexpected GFEX content type for {}", row.report_date);
    }
    let url = Url::parse(&row.url)?;
    if url.scheme() != "http"
        || url.host_str() != Some("www.gfex.com.cn")
        || url.path() != "/u/interfacesWebTiFutAndOptSettle/loadList"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unexpected GFEX settlement URL {}", row.url);
    }
    Ok(())
}

fn parse_fee(value: f64, style: &str) -> Result<FeeSpec> {
    if !value.is_finite() || value < 0.0 {
        bail!("invalid GFEX fee value");
    }
    if value == 0.0 {
        return Ok(FeeSpec {
            kind: FeeKind::Zero,
            value: Some(0.0),
            raw_text: Some(format!("0 ({style})")),
        });
    }
    let kind = match style.trim() {
        "比例值" => FeeKind::TurnoverRatePerTenThousand,
        "绝对值" => FeeKind::CnyPerLot,
        other => bail!("unknown GFEX fee style {other}"),
    };
    Ok(FeeSpec {
        kind,
        value: Some(value),
        raw_text: Some(format!("{value} ({style})")),
    })
}

fn is_contract_id(value: &str) -> bool {
    let value = value.as_bytes();
    let letters = value
        .iter()
        .take_while(|byte| byte.is_ascii_alphabetic())
        .count();
    (1..=3).contains(&letters)
        && value.len() == letters + 4
        && value[letters..].iter().all(u8::is_ascii_digit)
}

fn parse_compact_date(value: &str) -> Result<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid GFEX compact date {value}");
    }
    let format = time::format_description::parse("[year][month][day]")?;
    Ok(Date::parse(value, &format)?)
}
