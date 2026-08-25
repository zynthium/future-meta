//! Import retained GFEX daily settlement parameters as complete fee tuples.

use crate::db;
use crate::jin10::ContractStaticMetadata;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::Url;
use rusqlite::{OptionalExtension, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime, Time, UtcOffset};

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
    let (mut observations, snapshots) = load_observations(options)?;
    if observations.is_empty() {
        bail!("GFEX parameter import has no in-range observations");
    }
    observations.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then_with(|| left.valid_from.cmp(&right.valid_from))
    });
    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    let mut last_observed = BTreeMap::<String, String>::new();
    let mut corpus_last = String::new();
    for observation in observations {
        corpus_last = corpus_last.max(observation.valid_from.clone());
        last_observed
            .entry(observation.symbol.clone())
            .and_modify(|latest| {
                if observation.valid_from > *latest {
                    latest.clone_from(&observation.valid_from);
                }
            })
            .or_insert_with(|| observation.valid_from.clone());
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
        let coverage_end = (OffsetDateTime::parse(symbol_last, &Rfc3339)? + Duration::days(1))
            .max(
                compact_day_start(listing.as_deref().ok_or_else(|| {
                    anyhow!("GFEX contract listing date missing {}", first.symbol)
                })?)?
                    + Duration::days(1),
            )
            .format(&Rfc3339)?;
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

fn compact_day_start(value: &str) -> Result<OffsetDateTime> {
    Ok(parse_compact_date(value)?
        .with_time(Time::MIDNIGHT)
        .assume_offset(UtcOffset::from_hms(8, 0, 0)?))
}

/// Inputs one offline, hash-verified GFEX trading-calendar lifecycle import.
#[derive(Debug, Clone)]
pub struct GfexCalendarImportOptions {
    pub history_db: PathBuf,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub observed_at: String,
}

/// Counts returned after a GFEX trading-calendar lifecycle import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GfexCalendarImportResult {
    pub snapshots: usize,
    pub contracts: usize,
    pub evidence_links: usize,
}

#[derive(Debug, Deserialize)]
struct CalendarManifestRow {
    report_date: String,
    status: String,
    sha256: String,
    url: String,
}

#[derive(Debug, Deserialize)]
struct CalendarDocument {
    code: String,
    data: Vec<CalendarEvent>,
}

#[derive(Debug, Deserialize)]
struct CalendarEvent {
    #[serde(rename = "calendarDate")]
    calendar_date: String,
    #[serde(rename = "contractId")]
    contract_id: String,
    #[serde(rename = "eventType")]
    event_type: String,
}

#[derive(Debug, Clone)]
struct CalendarLifecycle {
    listing_date: String,
    listing_evidence: (String, String),
    expiry_date: String,
    expiry_evidence: (String, String),
}

/// Import exact GFEX listing and last-trading dates from retained calendar data.
///
/// Each contract must have both official event types. The importer refuses to
/// infer an expiry from a contract disappearing from another feed.
///
/// # Errors
///
/// Returns an error for malformed retained snapshots, incomplete event pairs,
/// lifecycle conflicts, or database failures.
#[allow(clippy::too_many_lines)]
pub fn import_trading_calendar_lifecycles(
    options: &GfexCalendarImportOptions,
) -> Result<GfexCalendarImportResult> {
    time::OffsetDateTime::parse(&options.observed_at, &Rfc3339)?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut events = BTreeMap::<
        String,
        (
            Option<(String, String, String)>,
            Option<(String, String, String)>,
        ),
    >::new();
    let mut snapshots = 0usize;

    for row in reader.deserialize::<CalendarManifestRow>() {
        let row = row?;
        if row.status != "ok" {
            continue;
        }
        parse_compact_date(&row.report_date)?;
        validate_calendar_url(&row.url)?;
        let bytes = read_retained_evidence(&options.snapshot_dir, &row.sha256)?;
        let document: CalendarDocument = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse GFEX calendar snapshot {}", row.report_date))?;
        if document.code != "0" {
            bail!("GFEX calendar document unsuccessful {}", row.report_date);
        }
        for event in document.data {
            if !matches!(
                event.event_type.as_str(),
                "期货合约开始交易日" | "合约最后交易日"
            ) {
                continue;
            }
            parse_compact_date(&event.calendar_date)?;
            for contract_id in calendar_contract_ids(&event.contract_id) {
                let entry = events.entry(format!("GFEX.{contract_id}")).or_default();
                let evidence = (
                    event.calendar_date.clone(),
                    row.url.clone(),
                    row.sha256.clone(),
                );
                let slot = if event.event_type == "期货合约开始交易日" {
                    &mut entry.0
                } else {
                    &mut entry.1
                };
                if let Some(existing) = slot {
                    // The endpoint republishes the same lifecycle event in later
                    // snapshots.  The date is the contract fact; retain the first
                    // official snapshot as provenance when the date agrees.
                    if existing.0 != evidence.0 {
                        bail!(
                            "conflicting GFEX calendar {} event for {}",
                            event.event_type,
                            contract_id
                        );
                    }
                } else {
                    *slot = Some(evidence);
                }
            }
        }
        snapshots += 1;
    }

    if snapshots == 0 {
        bail!("GFEX calendar manifest has no successful snapshots");
    }

    let mut lifecycles = BTreeMap::new();
    for (symbol, (listing, expiry)) in events {
        let (Some(listing), Some(expiry)) = (listing, expiry) else {
            continue;
        };
        if listing.0 > expiry.0 {
            bail!("GFEX calendar listing after expiry for {symbol}");
        }
        lifecycles.insert(
            symbol,
            CalendarLifecycle {
                listing_date: listing.0,
                listing_evidence: (listing.1, listing.2),
                expiry_date: expiry.0,
                expiry_evidence: (expiry.1, expiry.2),
            },
        );
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let transaction = connection.transaction()?;
    let mut evidence_links = 0usize;
    let mut contracts = 0usize;
    for (symbol, lifecycle) in lifecycles {
        let Some(contract_id) = transaction
            .query_row(
                "select id from contracts where symbol = ?1",
                [&symbol],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
        else {
            continue;
        };
        transaction.execute(
            "update contracts set listing_date = ?1, expiry_date = ?2 where id = ?3",
            params![lifecycle.listing_date, lifecycle.expiry_date, contract_id],
        )?;
        transaction.execute(
            "delete from contract_lifecycle_evidence where contract_id = ?1",
            [contract_id],
        )?;
        let mut evidence = BTreeSet::new();
        evidence.insert(lifecycle.listing_evidence);
        evidence.insert(lifecycle.expiry_evidence);
        for (url, sha256) in evidence {
            transaction.execute(
                "insert into contract_lifecycle_evidence(
                     contract_id, listing_date, expiry_date, canonical_url, body_sha256, recorded_at
                 ) values(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    contract_id,
                    lifecycle.listing_date,
                    lifecycle.expiry_date,
                    url,
                    sha256,
                    options.observed_at
                ],
            )?;
            evidence_links += 1;
        }
        contracts += 1;
    }
    transaction.commit()?;
    Ok(GfexCalendarImportResult {
        snapshots,
        contracts,
        evidence_links,
    })
}

fn calendar_contract_ids(value: &str) -> Vec<String> {
    value
        .split(['、', '，', ',', ' '])
        .filter_map(|item| item.strip_suffix("合约").or(Some(item)))
        .map(str::trim)
        .filter(|item| is_contract_id(item))
        .map(str::to_ascii_lowercase)
        .collect()
}

fn validate_calendar_url(value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    if url.scheme() != "http"
        || url.host_str() != Some("www.gfex.com.cn")
        || url.path() != "/u/interfacesWebTpTradingCalendar/loadList"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unexpected GFEX calendar URL {value}");
    }
    Ok(())
}

fn read_retained_evidence(snapshot_dir: &std::path::Path, sha256: &str) -> Result<Vec<u8>> {
    if sha256.len() != 64
        || !sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid GFEX calendar SHA-256 {sha256}");
    }
    let path = snapshot_dir.join(format!("{sha256}.json"));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read retained GFEX calendar snapshot {}", path.display()))?;
    if hex::encode(Sha256::digest(&bytes)) != sha256 {
        bail!("retained GFEX calendar SHA-256 mismatch {sha256}");
    }
    Ok(bytes)
}
