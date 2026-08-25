//! Import CZCE concrete-contract lifecycle evidence from the official calendar API.

use crate::db;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::symbol::{SymbolKind, parse_symbol};
use rusqlite::{Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

/// Inputs for a hash-verified CZCE official calendar import.
#[derive(Debug, Clone)]
pub struct CzceMetadataImportOptions {
    pub history_db: PathBuf,
    pub calendar_manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub observed_at: String,
}

/// Result of importing CZCE lifecycle evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CzceMetadataImportResult {
    pub calendar_snapshots: usize,
    pub contracts: usize,
    pub lifecycle_evidence: usize,
}

#[derive(Debug, Deserialize)]
struct CalendarManifestRow {
    month: String,
    canonical_url: String,
    sha256: String,
    bytes: usize,
    record_count: usize,
    content_type: String,
}

#[derive(Debug, Deserialize)]
struct CalendarResponse {
    success: bool,
    result: Vec<CalendarRow>,
}

#[derive(Debug, Deserialize)]
struct CalendarRow {
    xsrq: String,
    hygp: Option<String>,
    hydq: Option<String>,
}

#[derive(Debug, Clone)]
struct Lifecycle {
    listing_date: Option<Date>,
    listing_url: String,
    listing_sha256: String,
    expiry_date: Option<Date>,
    expiry_url: String,
    expiry_sha256: String,
}

#[derive(Debug, Clone)]
struct Contract {
    id: i64,
    symbol: String,
}

/// Import exact listing and expiry events stated by CZCE's official calendar API.
///
/// The importer intentionally does not infer a date from a product rule, a
/// month code, or a third-party database. Every concrete contract in the
/// database must have both an explicit `合约挂牌` and `合约到期` event.
///
/// # Errors
///
/// Returns an error when a retained snapshot is malformed, unverified, or
/// does not provide both lifecycle events for every CZCE contract.
pub fn import_metadata(options: &CzceMetadataImportOptions) -> Result<CzceMetadataImportResult> {
    OffsetDateTime::parse(&options.observed_at, &Rfc3339)
        .context("invalid CZCE metadata observed_at")?;
    let events = load_calendar(&options.calendar_manifest, &options.snapshot_dir)?;
    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let contracts = load_contracts(&connection)?;
    if contracts.is_empty() {
        bail!("CZCE metadata import found no contracts");
    }

    let transaction = connection.transaction()?;
    let mut evidence_count = 0;
    for contract in &contracts {
        let lifecycle = events
            .events
            .get(
                contract
                    .symbol
                    .strip_prefix("CZCE.")
                    .unwrap_or(&contract.symbol),
            )
            .ok_or_else(|| {
                anyhow!(
                    "CZCE calendar missing lifecycle events: {}",
                    contract.symbol
                )
            })?;
        let (Some(listing_date), Some(expiry_date)) =
            (lifecycle.listing_date, lifecycle.expiry_date)
        else {
            bail!(
                "CZCE calendar missing listing or expiry event: {}",
                contract.symbol
            );
        };
        if expiry_date < listing_date {
            bail!("CZCE expiry precedes listing: {}", contract.symbol);
        }
        persist_contract(&transaction, contract, lifecycle, &options.observed_at)?;
        evidence_count += 1;
    }
    transaction.commit()?;

    Ok(CzceMetadataImportResult {
        calendar_snapshots: events.snapshots,
        contracts: contracts.len(),
        lifecycle_evidence: evidence_count,
    })
}

#[derive(Debug)]
struct CalendarEvents {
    snapshots: usize,
    events: BTreeMap<String, Lifecycle>,
}

fn load_calendar(manifest: &Path, snapshot_dir: &Path) -> Result<CalendarEvents> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(manifest)
        .with_context(|| format!("open CZCE calendar manifest {}", manifest.display()))?;
    let mut events = BTreeMap::<String, Lifecycle>::new();
    let mut snapshots = 0;

    for row in reader.deserialize::<CalendarManifestRow>() {
        let row = row?;
        validate_manifest_row(&row)?;
        let bytes = read_snapshot(snapshot_dir, &row.sha256)?;
        if bytes.len() != row.bytes {
            bail!("CZCE calendar byte count mismatch: {}", row.canonical_url);
        }
        let response: CalendarResponse = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse CZCE calendar snapshot {}", row.canonical_url))?;
        if !response.success || response.result.len() != row.record_count {
            bail!(
                "CZCE calendar response metadata mismatch: {}",
                row.canonical_url
            );
        }
        if row.record_count == 0 {
            continue;
        }
        snapshots += 1;

        for item in response.result {
            let event_date = parse_date(&item.xsrq)?;
            for event_text in [item.hygp.as_deref(), item.hydq.as_deref()]
                .into_iter()
                .flatten()
            {
                if !event_text.contains("挂盘") {
                    continue;
                }
                for symbol in extract_contracts(event_text) {
                    let entry = events.entry(symbol.clone()).or_insert_with(|| Lifecycle {
                        listing_date: Some(event_date),
                        listing_url: row.canonical_url.clone(),
                        listing_sha256: row.sha256.clone(),
                        expiry_date: None,
                        expiry_url: row.canonical_url.clone(),
                        expiry_sha256: row.sha256.clone(),
                    });
                    if entry
                        .listing_date
                        .is_some_and(|previous| previous != event_date)
                    {
                        bail!(
                            "conflicting CZCE listing events: {symbol} {:?} vs {event_date}",
                            entry.listing_date
                        );
                    }
                    entry.listing_date = Some(event_date);
                    entry.listing_url.clone_from(&row.canonical_url);
                    entry.listing_sha256.clone_from(&row.sha256);
                }
            }
            for event_text in [item.hygp.as_deref(), item.hydq.as_deref()]
                .into_iter()
                .flatten()
            {
                if !event_text.contains("最后交易日") && !event_text.contains("到期") {
                    continue;
                }
                for symbol in extract_contracts(event_text) {
                    let entry = events.entry(symbol.clone()).or_insert_with(|| Lifecycle {
                        listing_date: None,
                        listing_url: row.canonical_url.clone(),
                        listing_sha256: row.sha256.clone(),
                        expiry_date: Some(event_date),
                        expiry_url: row.canonical_url.clone(),
                        expiry_sha256: row.sha256.clone(),
                    });
                    if entry
                        .expiry_date
                        .is_some_and(|previous| previous != event_date)
                    {
                        bail!("conflicting CZCE expiry events: {symbol}");
                    }
                    entry.expiry_date = Some(event_date);
                    entry.expiry_url.clone_from(&row.canonical_url);
                    entry.expiry_sha256.clone_from(&row.sha256);
                }
            }
        }
    }
    if snapshots == 0 {
        bail!("CZCE calendar manifest is empty");
    }
    Ok(CalendarEvents { snapshots, events })
}

fn load_contracts(connection: &rusqlite::Connection) -> Result<Vec<Contract>> {
    let mut statement = connection
        .prepare("select id, symbol from contracts where symbol like 'CZCE.%' order by symbol")?;
    let rows = statement
        .query_map([], |row| {
            Ok(Contract {
                id: row.get(0)?,
                symbol: row.get(1)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn persist_contract(
    transaction: &Transaction<'_>,
    contract: &Contract,
    lifecycle: &Lifecycle,
    observed_at: &str,
) -> Result<()> {
    let listing = compact_date(lifecycle.listing_date.expect("validated listing date"));
    let expiry = compact_date(lifecycle.expiry_date.expect("validated expiry date"));
    transaction.execute(
        "update contracts set listing_date = ?1, expiry_date = ?2, last_seen_at = ?3 where id = ?4",
        params![listing, expiry, observed_at, contract.id],
    )?;
    transaction.execute(
        "delete from contract_lifecycle_evidence where contract_id = ?1",
        [contract.id],
    )?;
    for (url, sha256) in [
        (&lifecycle.listing_url, &lifecycle.listing_sha256),
        (&lifecycle.expiry_url, &lifecycle.expiry_sha256),
    ] {
        transaction.execute(
            "insert or ignore into contract_lifecycle_evidence(contract_id, listing_date, expiry_date, canonical_url, body_sha256, recorded_at) values(?1, ?2, ?3, ?4, ?5, ?6)",
            params![contract.id, listing, expiry, url, sha256, observed_at],
        )?;
    }
    Ok(())
}

fn validate_manifest_row(row: &CalendarManifestRow) -> Result<()> {
    let mut month_parts = row.month.split('-');
    let year = month_parts
        .next()
        .and_then(|value| value.parse::<i32>().ok());
    let month = month_parts
        .next()
        .and_then(|value| value.parse::<u8>().ok());
    if year.is_none()
        || month.is_none()
        || month_parts.next().is_some_and(|value| !value.is_empty())
    {
        bail!("invalid CZCE calendar month: {}", row.month);
    }
    if !(1..=12).contains(&month.unwrap()) {
        bail!("invalid CZCE calendar month: {}", row.month);
    }
    let url = reqwest::Url::parse(&row.canonical_url)
        .with_context(|| format!("invalid CZCE calendar URL: {}", row.canonical_url))?;
    if url.scheme() != "https"
        || !matches!(
            url.host_str(),
            Some("app.czce.com.cn" | "www.czce.com.cn" | "czce.com.cn")
        )
        || url.username() != ""
        || url.password().is_some()
        || url.path() != "/cmsapi/cmsapp/content/selectJyyl"
    {
        bail!("unexpected CZCE calendar URL: {}", row.canonical_url);
    }
    let query = url.query_pairs().collect::<Vec<_>>();
    if query.len() != 1 || query[0].0 != "Jv5sQwFC" || query[0].1.is_empty() {
        bail!("CZCE calendar URL must contain one Jv5sQwFC token");
    }
    if row.content_type != "application/json" {
        bail!("CZCE calendar content type must be application/json");
    }
    if row.bytes == 0 {
        bail!("CZCE calendar snapshot byte count must be non-zero");
    }
    validate_sha256(&row.sha256)
}

fn read_snapshot(snapshot_dir: &Path, sha256: &str) -> Result<Vec<u8>> {
    let path = snapshot_dir.join(format!("{sha256}.json"));
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read retained CZCE calendar snapshot {}", path.display()))?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != sha256 {
        bail!("retained CZCE calendar SHA-256 mismatch: {sha256}");
    }
    Ok(bytes)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid CZCE calendar SHA-256: {value}");
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<Date> {
    Date::parse(
        value.trim(),
        &time::format_description::well_known::Iso8601::DEFAULT,
    )
    .map_err(Into::into)
}

fn compact_date(date: Date) -> String {
    date.format(
        &time::format_description::parse(
            "[year][month repr:numerical padding:zero][day padding:zero]",
        )
        .expect("valid date format"),
    )
    .expect("valid date")
}

fn extract_contracts(text: &str) -> Vec<String> {
    let mut result = Vec::new();
    for token in text.split(|character: char| !character.is_ascii_alphanumeric()) {
        let Some(digit_start) = token.find(|character: char| character.is_ascii_digit()) else {
            continue;
        };
        let (letters, suffix) = token.split_at(digit_start);
        if letters.is_empty()
            || !letters
                .chars()
                .all(|character| character.is_ascii_uppercase())
        {
            continue;
        }
        let digits: String = suffix.chars().take(4).collect();
        if digits.len() != 4 || !digits.chars().all(|character| character.is_ascii_digit()) {
            continue;
        }
        let trailing = suffix.chars().nth(4);
        if trailing.is_some_and(|character| character.is_ascii_alphabetic()) {
            continue;
        }
        let local = format!("{letters}{}", &digits[1..]);
        let symbol = format!("CZCE.{local}");
        if parse_symbol(&symbol).is_ok_and(|parsed| parsed.kind == SymbolKind::Futures)
            && !result.contains(&local)
        {
            result.push(local);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::extract_contracts;

    #[test]
    fn extracts_futures_and_skips_options() {
        assert_eq!(
            extract_contracts("今日AP2701、CF2703C/P、ZC2701合约挂盘"),
            vec!["AP701", "ZC701"]
        );
    }
}
