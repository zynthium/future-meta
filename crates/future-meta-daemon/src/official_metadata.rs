//! Import reviewed official lifecycle and contract-specification history.

use crate::db;
use crate::official::validate_official_canonical_url;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::symbol::{SymbolKind, parse_symbol};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

/// Inputs for one offline, hash-verified metadata import.
#[derive(Debug, Clone)]
pub struct OfficialMetadataImportOptions {
    pub history_db: PathBuf,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub observed_at: String,
}

/// Counts returned after successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialMetadataImportResult {
    pub contracts: usize,
    pub specification_versions: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct ManifestRow {
    symbol: String,
    listing_date: String,
    expiry_date: String,
    valid_from: String,
    valid_to: String,
    lot_size: f64,
    tick_size: f64,
    lifecycle_url: String,
    lifecycle_sha256: String,
    specification_url: String,
    specification_sha256: String,
}

#[derive(Debug)]
struct ValidatedRow {
    source: ManifestRow,
    listing: Date,
    expiry: Date,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
}

/// Replace lifecycle and specification intervals from reviewed official files.
///
/// The manifest deliberately carries separate lifecycle and specification
/// evidence references. A document may fill both only when it states both.
///
/// # Errors
///
/// Returns an error for invalid symbols, dates, intervals, official URLs,
/// retained-byte digests, incomplete lifetime coverage, or database failures.
pub fn import_contract_metadata(
    options: &OfficialMetadataImportOptions,
) -> Result<OfficialMetadataImportResult> {
    OffsetDateTime::parse(&options.observed_at, &Rfc3339).context("invalid observed_at")?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut by_symbol = BTreeMap::<String, Vec<ValidatedRow>>::new();
    for record in reader.deserialize::<ManifestRow>() {
        let row = validate_row(record?, &options.snapshot_dir)?;
        by_symbol
            .entry(row.source.symbol.clone())
            .or_default()
            .push(row);
    }
    if by_symbol.is_empty() {
        bail!("official metadata manifest is empty");
    }
    for rows in by_symbol.values_mut() {
        validate_contract_intervals(rows)?;
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let transaction = connection.transaction()?;
    let mut version_count = 0;
    for rows in by_symbol.values() {
        version_count += persist_contract(&transaction, rows, &options.observed_at)?;
    }
    transaction.commit()?;
    Ok(OfficialMetadataImportResult {
        contracts: by_symbol.len(),
        specification_versions: version_count,
    })
}

fn persist_contract(
    transaction: &Transaction<'_>,
    rows: &[ValidatedRow],
    observed_at: &str,
) -> Result<usize> {
    let first = rows
        .first()
        .ok_or_else(|| anyhow!("official metadata contract has no intervals"))?;
    let last = rows
        .last()
        .ok_or_else(|| anyhow!("official metadata contract has no intervals"))?;
    let contract_id = transaction
        .query_row(
            "select id from contracts where symbol = ?1",
            [&first.source.symbol],
            |record| record.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            anyhow!(
                "official metadata contract missing: {}",
                first.source.symbol
            )
        })?;
    transaction.execute(
        "update contracts set listing_date = ?1, expiry_date = ?2,
             lot_size = ?3, tick_size = ?4, last_seen_at = ?5 where id = ?6",
        params![
            compact_date(first.listing),
            compact_date(first.expiry),
            last.source.lot_size,
            last.source.tick_size,
            observed_at,
            contract_id
        ],
    )?;
    for table in [
        "contract_spec_evidence",
        "contract_spec_versions",
        "contract_lifecycle_evidence",
    ] {
        transaction.execute(
            &format!("delete from {table} where contract_id = ?1"),
            [contract_id],
        )?;
    }
    for row in rows {
        insert_specification_interval(transaction, contract_id, row, observed_at)?;
    }
    transaction.execute(
        "insert into contract_lifecycle_evidence(
             contract_id, listing_date, expiry_date, canonical_url,
             body_sha256, recorded_at
         ) values(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            contract_id,
            compact_date(first.listing),
            compact_date(first.expiry),
            first.source.lifecycle_url,
            first.source.lifecycle_sha256,
            observed_at
        ],
    )?;
    Ok(rows.len())
}

fn insert_specification_interval(
    transaction: &Transaction<'_>,
    contract_id: i64,
    row: &ValidatedRow,
    observed_at: &str,
) -> Result<()> {
    transaction.execute(
        "insert into contract_spec_versions(
             contract_id, lot_size, tick_size, valid_from, valid_to,
             source_kind, source_url, first_seen_at, last_seen_at
         ) values(?1, ?2, ?3, ?4, ?5, 'official', ?6, ?7, ?7)",
        params![
            contract_id,
            row.source.lot_size,
            row.source.tick_size,
            row.source.valid_from,
            empty_to_none(&row.source.valid_to),
            row.source.specification_url,
            observed_at
        ],
    )?;
    transaction.execute(
        "insert into contract_spec_evidence(
             contract_id, valid_from, canonical_url, body_sha256, recorded_at
         ) values(?1, ?2, ?3, ?4, ?5)",
        params![
            contract_id,
            row.source.valid_from,
            row.source.specification_url,
            row.source.specification_sha256,
            observed_at
        ],
    )?;
    Ok(())
}

fn validate_row(row: ManifestRow, snapshot_dir: &Path) -> Result<ValidatedRow> {
    let parsed = parse_symbol(&row.symbol)?;
    if parsed.kind != SymbolKind::Futures {
        bail!("official metadata requires concrete futures symbol");
    }
    let listing = parse_date(&row.listing_date)?;
    let expiry = parse_date(&row.expiry_date)?;
    if listing > expiry {
        bail!("official metadata listing after expiry: {}", row.symbol);
    }
    let valid_from = OffsetDateTime::parse(&row.valid_from, &Rfc3339)?;
    let valid_to = empty_to_none(&row.valid_to)
        .map(|value| OffsetDateTime::parse(value, &Rfc3339))
        .transpose()?;
    if valid_to.is_some_and(|value| value <= valid_from) {
        bail!(
            "official metadata invalid specification interval: {}",
            row.symbol
        );
    }
    if !row.lot_size.is_finite() || row.lot_size <= 0.0 {
        bail!("official metadata invalid lot size: {}", row.symbol);
    }
    if !row.tick_size.is_finite() || row.tick_size <= 0.0 {
        bail!("official metadata invalid tick size: {}", row.symbol);
    }
    validate_official_canonical_url(&parsed.exchange, &row.lifecycle_url)?;
    validate_official_canonical_url(&parsed.exchange, &row.specification_url)?;
    verify_retained_evidence(snapshot_dir, &row.lifecycle_sha256)?;
    verify_retained_evidence(snapshot_dir, &row.specification_sha256)?;
    Ok(ValidatedRow {
        source: row,
        listing,
        expiry,
        valid_from,
        valid_to,
    })
}

fn validate_contract_intervals(rows: &mut [ValidatedRow]) -> Result<()> {
    rows.sort_by_key(|row| row.valid_from);
    let first = &rows[0];
    let listing_start = first
        .listing
        .midnight()
        .assume_offset(first.valid_from.offset());
    if first.valid_from != listing_start {
        bail!(
            "official specification history must start on listing date: {}",
            first.source.symbol
        );
    }
    for row in rows.iter() {
        if row.listing != first.listing || row.expiry != first.expiry {
            bail!(
                "official lifecycle conflicts inside manifest: {}",
                first.source.symbol
            );
        }
    }
    for pair in rows.windows(2) {
        if pair[0].valid_to != Some(pair[1].valid_from) {
            bail!(
                "official specification intervals are not contiguous: {}",
                first.source.symbol
            );
        }
    }
    let last = rows.last().expect("validated rows are nonempty");
    let expiry_end = first
        .expiry
        .next_day()
        .ok_or_else(|| anyhow!("official metadata expiry cannot advance"))?
        .midnight()
        .assume_offset(first.valid_from.offset());
    if last.valid_to.is_some_and(|value| value < expiry_end) {
        bail!(
            "official specification history ends before expiry: {}",
            first.source.symbol
        );
    }
    Ok(())
}

fn parse_date(value: &str) -> Result<Date> {
    let format = time::format_description::parse("[year]-[month]-[day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid official metadata date {value}"))
}

fn compact_date(value: Date) -> String {
    value.to_string().replace('-', "")
}

fn empty_to_none(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn verify_retained_evidence(snapshot_dir: &Path, expected_sha256: &str) -> Result<()> {
    if expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid official metadata SHA-256: {expected_sha256}");
    }
    let prefix = format!("{expected_sha256}.");
    let matches = std::fs::read_dir(snapshot_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name == expected_sha256 || name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!("retained official metadata evidence must resolve uniquely: {expected_sha256}");
    }
    let actual = hex::encode(Sha256::digest(std::fs::read(&matches[0])?));
    if actual != expected_sha256 {
        bail!("retained official metadata evidence SHA-256 mismatch: {expected_sha256}");
    }
    Ok(())
}
