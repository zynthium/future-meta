//! Import SHFE and INE retained contract-base lifecycle snapshots.

use crate::db;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::symbol::{SymbolKind, derive_underlying_symbol, parse_symbol};
use rusqlite::{Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

/// Inputs for one offline, hash-verified SHFE or INE lifecycle import.
#[derive(Debug, Clone)]
pub struct ContractBaseInfoImportOptions {
    pub history_db: PathBuf,
    pub exchange: String,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub observed_at: String,
}

/// Counts returned after a successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContractBaseInfoImportResult {
    pub snapshots: usize,
    pub contracts: usize,
    pub evidence_links: usize,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    exchange: String,
    report_date: String,
    canonical_url: String,
    sha256: String,
    record_count: usize,
}

#[derive(Debug, Deserialize)]
struct Document {
    #[serde(rename = "ContractBaseInfo")]
    rows: Vec<DocumentRow>,
}

#[derive(Debug, Deserialize)]
struct DocumentRow {
    #[serde(rename = "INSTRUMENTID")]
    instrument_id: String,
    #[serde(rename = "OPENDATE")]
    open_date: String,
    #[serde(rename = "EXPIREDATE")]
    expiry_date: String,
    #[serde(rename = "TRADINGDAY")]
    trading_day: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Evidence {
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct Lifecycle {
    listing_date: String,
    expiry_date: String,
    evidence: Vec<Evidence>,
}

/// Import exact listing and expiry dates from retained exchange snapshots.
///
/// Every concrete contract already present for the selected exchange must occur
/// in at least one snapshot. Listing conflicts fail; later official expiry
/// revisions replace earlier provisional boundaries.
///
/// # Errors
///
/// Returns an error for unsupported exchanges, invalid manifests, unretained
/// bytes, malformed official data, incomplete contract coverage, or database
/// failures.
pub fn import_contract_base_info(
    options: &ContractBaseInfoImportOptions,
) -> Result<ContractBaseInfoImportResult> {
    OffsetDateTime::parse(&options.observed_at, &Rfc3339).context("invalid observed_at")?;
    validate_exchange(&options.exchange)?;
    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let transaction = connection.transaction()?;
    let mut targets = load_targets(&transaction, &options.exchange)?;
    let (lifecycles, snapshots) = load_lifecycles(options)?;
    admit_discovered_contracts(
        &transaction,
        &options.exchange,
        &mut targets,
        &lifecycles,
        &options.observed_at,
    )?;
    let missing = targets
        .keys()
        .filter(|symbol| !lifecycles.contains_key(*symbol))
        .cloned()
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        bail!(
            "{} contract-base snapshots miss {} database contracts: {}",
            options.exchange,
            missing.len(),
            missing.join(", ")
        );
    }

    let evidence_links =
        persist_lifecycles(&transaction, &targets, &lifecycles, &options.observed_at)?;
    transaction.commit()?;
    Ok(ContractBaseInfoImportResult {
        snapshots,
        contracts: targets.len(),
        evidence_links,
    })
}

fn load_lifecycles(
    options: &ContractBaseInfoImportOptions,
) -> Result<(BTreeMap<String, Lifecycle>, usize)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut lifecycles = BTreeMap::<String, Lifecycle>::new();
    let mut identities = BTreeSet::new();
    let mut report_dates = BTreeSet::new();
    let mut previous_report_date = None::<String>;
    let mut snapshots = 0;
    for row in reader.deserialize::<ManifestRow>() {
        let row = row?;
        validate_manifest_row(&row, &options.exchange)?;
        if !report_dates.insert(row.report_date.clone()) {
            bail!(
                "duplicate report date in contract-base manifest: {}",
                row.report_date
            );
        }
        if previous_report_date
            .as_ref()
            .is_some_and(|previous| previous >= &row.report_date)
        {
            bail!("contract-base manifest report dates are not ascending");
        }
        previous_report_date = Some(row.report_date.clone());
        if !identities.insert((row.canonical_url.clone(), row.sha256.clone())) {
            bail!(
                "duplicate contract-base manifest snapshot: {}",
                row.canonical_url
            );
        }
        let bytes = read_retained_evidence(&options.snapshot_dir, &row.sha256)?;
        let document: Document = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse contract-base snapshot {}", row.canonical_url))?;
        if document.rows.len() != row.record_count {
            bail!(
                "contract-base record count mismatch for {}",
                row.report_date
            );
        }
        for item in document.rows {
            if item.trading_day.trim() != row.report_date {
                bail!("contract-base trading day mismatch for {}", row.report_date);
            }
            let symbol = format!("{}.{}", options.exchange, item.instrument_id.trim());
            let Ok(parsed) = parse_symbol(&symbol) else {
                continue;
            };
            if parsed.kind != SymbolKind::Futures {
                continue;
            }
            let listing_date = item.open_date.trim();
            let expiry_date = item.expiry_date.trim();
            validate_lifecycle_dates(&symbol, listing_date, expiry_date)?;
            let evidence = Evidence {
                canonical_url: row.canonical_url.clone(),
                sha256: row.sha256.clone(),
            };
            match lifecycles.get_mut(&symbol) {
                Some(existing) if existing.listing_date != listing_date => {
                    bail!("contract-base listing date conflict for {symbol}");
                }
                Some(existing) if existing.expiry_date != expiry_date => {
                    expiry_date.clone_into(&mut existing.expiry_date);
                    existing.evidence = vec![evidence];
                }
                Some(existing) => existing.evidence.push(evidence),
                None => {
                    lifecycles.insert(
                        symbol,
                        Lifecycle {
                            listing_date: listing_date.to_owned(),
                            expiry_date: expiry_date.to_owned(),
                            evidence: vec![evidence],
                        },
                    );
                }
            }
        }
        snapshots += 1;
    }
    if snapshots == 0 {
        bail!("contract-base manifest is empty");
    }
    Ok((lifecycles, snapshots))
}

fn load_targets(transaction: &Transaction<'_>, exchange: &str) -> Result<BTreeMap<String, i64>> {
    let prefix = format!("{exchange}.%");
    let mut statement = transaction
        .prepare("select id, symbol from contracts where symbol like ?1 order by symbol")?;
    let targets = statement
        .query_map([prefix], |row| {
            Ok((row.get::<_, String>(1)?, row.get::<_, i64>(0)?))
        })?
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
    if targets.is_empty() {
        bail!("database has no {exchange} contracts");
    }
    Ok(targets)
}

/// Admit concrete contracts present in an official snapshot but absent from
/// the provisional seed.  Static multiplier/tick metadata comes from an
/// already-imported official product specification for the same product.
fn admit_discovered_contracts(
    transaction: &Transaction<'_>,
    exchange: &str,
    targets: &mut BTreeMap<String, i64>,
    lifecycles: &BTreeMap<String, Lifecycle>,
    observed_at: &str,
) -> Result<()> {
    for (symbol, lifecycle) in lifecycles {
        if targets.contains_key(symbol) {
            continue;
        }

        let parsed = parse_symbol(symbol)?;
        if parsed.kind != SymbolKind::Futures || parsed.exchange != exchange {
            continue;
        }
        let product = derive_underlying_symbol(symbol)?;
        // SHFE's retained ContractBaseInfo feed also carries the INE BC
        // product. Match the product component across exchange namespaces so
        // that its official INE specification can seed the SHFE lifecycle
        // row without duplicating static evidence.
        let product_code = product
            .split_once('.')
            .map_or(product.as_str(), |(_, local)| local);
        let pattern = format!("*.{product_code}[0-9][0-9][0-9][0-9]");
        let mut statement = transaction.prepare(
            "select distinct s.lot_size, s.tick_size
             from contract_spec_versions s
             join contracts c on c.id = s.contract_id
             where c.symbol glob ?1
               and s.source_kind = 'official'
               and julianday(s.valid_from) <= julianday(?2)
               and (s.valid_to is null or julianday(?2) < julianday(s.valid_to))
             order by s.lot_size, s.tick_size",
        )?;
        let mut candidates = statement
            .query_map(params![pattern, lifecycle.listing_date], |row| {
                Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        // A newly listed month can be later than the retained product
        // interval's right edge.  In that case carry forward the latest
        // official product tuple; this is safe only when the latest tuple is
        // unambiguous (the check below enforces that invariant).
        if candidates.is_empty() {
            let mut statement = transaction.prepare(
                "select distinct s.lot_size, s.tick_size
                 from contract_spec_versions s
                 join contracts c on c.id = s.contract_id
                 where c.symbol glob ?1 and s.source_kind = 'official'
                 order by s.valid_from desc
                 limit 1",
            )?;
            candidates = statement
                .query_map([&pattern], |row| {
                    Ok((row.get::<_, f64>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
        }
        let Some(&(lot_size, tick_size)) = candidates.first() else {
            bail!("official product specification missing for discovered contract {symbol}");
        };
        if candidates.iter().any(|candidate| {
            candidate.0.to_bits() != lot_size.to_bits()
                || candidate.1.to_bits() != tick_size.to_bits()
        }) {
            bail!("ambiguous official product specification for discovered contract {symbol}");
        }

        transaction.execute(
            "insert into contracts(
                 symbol, listing_date, expiry_date, lot_size, tick_size,
                 first_seen_at, last_seen_at, active
             ) values(?1, ?2, ?3, ?4, ?5, ?6, ?6, 0)",
            params![
                symbol,
                lifecycle.listing_date,
                lifecycle.expiry_date,
                lot_size,
                tick_size,
                observed_at,
            ],
        )?;
        targets.insert(symbol.clone(), transaction.last_insert_rowid());
    }
    Ok(())
}

fn persist_lifecycles(
    transaction: &Transaction<'_>,
    targets: &BTreeMap<String, i64>,
    lifecycles: &BTreeMap<String, Lifecycle>,
    observed_at: &str,
) -> Result<usize> {
    let mut evidence_links = 0;
    for (symbol, contract_id) in targets {
        let lifecycle = lifecycles
            .get(symbol)
            .ok_or_else(|| anyhow!("missing validated lifecycle for {symbol}"))?;
        transaction.execute(
            "update contracts set listing_date = ?1, expiry_date = ?2 where id = ?3",
            params![lifecycle.listing_date, lifecycle.expiry_date, contract_id],
        )?;
        transaction.execute(
            "delete from contract_lifecycle_evidence where contract_id = ?1",
            [contract_id],
        )?;
        for evidence in &lifecycle.evidence {
            transaction.execute(
                "insert into contract_lifecycle_evidence(
                     contract_id, listing_date, expiry_date, canonical_url,
                     body_sha256, recorded_at
                 ) values(?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    contract_id,
                    lifecycle.listing_date,
                    lifecycle.expiry_date,
                    evidence.canonical_url,
                    evidence.sha256,
                    observed_at
                ],
            )?;
            evidence_links += 1;
        }
    }
    Ok(evidence_links)
}

fn validate_exchange(exchange: &str) -> Result<()> {
    if !matches!(exchange, "SHFE" | "INE") {
        bail!("contract-base importer supports only SHFE or INE");
    }
    Ok(())
}

fn validate_manifest_row(row: &ManifestRow, exchange: &str) -> Result<()> {
    if row.exchange != exchange {
        bail!("contract-base manifest exchange mismatch: {}", row.exchange);
    }
    parse_compact_date(&row.report_date)?;
    validate_sha256(&row.sha256)?;
    let expected = match exchange {
        "SHFE" => format!(
            "https://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo{}.dat",
            row.report_date
        ),
        "INE" => format!(
            "https://www.ine.cn/data/busiparamdata/future/ContractBaseInfo{}.dat",
            row.report_date
        ),
        _ => unreachable!("exchange validated before manifest"),
    };
    if row.canonical_url != expected {
        bail!("invalid contract-base canonical URL: {}", row.canonical_url);
    }
    Ok(())
}

fn validate_lifecycle_dates(symbol: &str, listing: &str, expiry: &str) -> Result<()> {
    let listing_date = parse_compact_date(listing)
        .with_context(|| format!("invalid contract-base listing date for {symbol}"))?;
    let expiry_date = parse_compact_date(expiry)
        .with_context(|| format!("invalid contract-base expiry date for {symbol}"))?;
    if listing_date > expiry_date {
        bail!("contract-base listing after expiry for {symbol}");
    }
    Ok(())
}

fn parse_compact_date(value: &str) -> Result<Date> {
    let format = time::format_description::parse("[year][month][day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid compact date {value}"))
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid contract-base SHA-256: {value}");
    }
    Ok(())
}

fn read_retained_evidence(snapshot_dir: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
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
        bail!("retained contract-base evidence must resolve uniquely: {expected_sha256}");
    }
    let bytes = std::fs::read(&matches[0])?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected_sha256 {
        bail!("retained contract-base evidence SHA-256 mismatch: {expected_sha256}");
    }
    Ok(bytes)
}
