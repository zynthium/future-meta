//! Import reviewed product-level contract multiplier and tick-size history.

use crate::db;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::Url;
use rusqlite::{Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

/// Inputs for one offline, hash-verified product specification import.
#[derive(Debug, Clone)]
pub struct ProductSpecImportOptions {
    pub history_db: PathBuf,
    pub exchange: String,
    pub manifest: PathBuf,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub observed_at: String,
}

/// Counts returned after a successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProductSpecImportResult {
    pub products: usize,
    pub contracts: usize,
    pub versions: usize,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    exchange: String,
    product: String,
    valid_from: String,
    valid_to: String,
    lot_size: f64,
    tick_size: f64,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct ProductVersion {
    valid_from: Date,
    valid_to: Option<Date>,
    lot_size: f64,
    tick_size: f64,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug)]
struct Contract {
    id: i64,
    symbol: String,
    product: String,
    listing: Date,
    expiry: Date,
}

/// Replace in-scope contract specification history with reviewed official
/// product intervals.
///
/// # Errors
///
/// Returns an error for incomplete product coverage, interval gaps, invalid
/// official URLs or digests, missing lifecycle dates, or database failures.
pub fn import_product_specs(options: &ProductSpecImportOptions) -> Result<ProductSpecImportResult> {
    OffsetDateTime::parse(&options.observed_at, &Rfc3339).context("invalid observed_at")?;
    validate_exchange(&options.exchange)?;
    let versions_by_product = load_manifest(options)?;

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let contracts = load_contracts(&connection, &options.exchange, options.from)?;
    validate_coverage(&contracts, &versions_by_product, options.from)?;
    let used_products = contracts
        .iter()
        .map(|contract| contract.product.clone())
        .collect::<BTreeSet<_>>();
    let unused = versions_by_product
        .keys()
        .filter(|product| !used_products.contains(*product))
        .cloned()
        .collect::<Vec<_>>();
    if !unused.is_empty() {
        bail!(
            "product specification manifest has unused products: {}",
            unused.join(", ")
        );
    }

    let transaction = connection.transaction()?;
    let versions = persist_contracts(
        &transaction,
        &contracts,
        &versions_by_product,
        &options.observed_at,
    )?;
    transaction.commit()?;
    Ok(ProductSpecImportResult {
        products: versions_by_product.len(),
        contracts: contracts.len(),
        versions,
    })
}

fn load_manifest(
    options: &ProductSpecImportOptions,
) -> Result<BTreeMap<String, Vec<ProductVersion>>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut versions = BTreeMap::<String, Vec<ProductVersion>>::new();
    for row in reader.deserialize::<ManifestRow>() {
        let row = row?;
        if row.exchange != options.exchange {
            bail!(
                "product specification manifest exchange mismatch: {}",
                row.exchange
            );
        }
        validate_product(&row.product)?;
        let valid_from = parse_date(&row.valid_from)?;
        let valid_to = optional_text(&row.valid_to).map(parse_date).transpose()?;
        if valid_to.is_some_and(|date| date <= valid_from) {
            bail!("invalid product specification interval: {}", row.product);
        }
        validate_number(row.lot_size, "lot size", &row.product)?;
        validate_number(row.tick_size, "tick size", &row.product)?;
        validate_specification_url(&options.exchange, &row.canonical_url)?;
        validate_sha256(&row.sha256)?;
        verify_retained_evidence(&options.snapshot_dir, &row.sha256)?;
        versions
            .entry(row.product.clone())
            .or_default()
            .push(ProductVersion {
                valid_from,
                valid_to,
                lot_size: row.lot_size,
                tick_size: row.tick_size,
                canonical_url: row.canonical_url,
                sha256: row.sha256,
            });
    }
    if versions.is_empty() {
        bail!("product specification manifest is empty");
    }
    for (product, rows) in &mut versions {
        rows.sort_by_key(|row| row.valid_from);
        for pair in rows.windows(2) {
            if pair[0].valid_to != Some(pair[1].valid_from) {
                bail!("product specification intervals are not contiguous: {product}");
            }
        }
        if rows.last().is_some_and(|row| row.valid_to.is_some()) {
            bail!("product specification history must end open: {product}");
        }
    }
    Ok(versions)
}

fn load_contracts(
    connection: &rusqlite::Connection,
    exchange: &str,
    from: Date,
) -> Result<Vec<Contract>> {
    let prefix = format!("{exchange}.%");
    let mut statement = connection.prepare(
        "select id, symbol, listing_date, expiry_date
         from contracts where symbol like ?1 order by symbol",
    )?;
    let rows = statement
        .query_map([prefix], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut contracts = Vec::new();
    for (id, symbol, listing, expiry) in rows {
        let listing = listing
            .as_deref()
            .ok_or_else(|| anyhow!("product specification contract lacks listing date: {symbol}"))
            .and_then(parse_compact_date)?;
        let expiry = expiry
            .as_deref()
            .ok_or_else(|| anyhow!("product specification contract lacks expiry date: {symbol}"))
            .and_then(parse_compact_date)?;
        if expiry < from {
            continue;
        }
        let underlying = derive_underlying_symbol(&symbol)?;
        let product = underlying
            .split_once('.')
            .map(|(_, product)| product.to_owned())
            .ok_or_else(|| anyhow!("invalid underlying symbol {underlying}"))?;
        contracts.push(Contract {
            id,
            symbol,
            product,
            listing,
            expiry,
        });
    }
    if contracts.is_empty() {
        bail!("database has no in-scope {exchange} contracts");
    }
    Ok(contracts)
}

fn validate_coverage(
    contracts: &[Contract],
    versions_by_product: &BTreeMap<String, Vec<ProductVersion>>,
    from: Date,
) -> Result<()> {
    for contract in contracts {
        let versions = versions_by_product.get(&contract.product).ok_or_else(|| {
            anyhow!(
                "product specification missing for contract {}",
                contract.symbol
            )
        })?;
        let scope_start = contract.listing.max(from);
        let scope_end = contract
            .expiry
            .next_day()
            .ok_or_else(|| anyhow!("contract expiry cannot advance: {}", contract.symbol))?;
        let relevant = relevant_versions(versions, scope_start, scope_end);
        let first = relevant
            .first()
            .ok_or_else(|| anyhow!("product specification does not cover {}", contract.symbol))?;
        if first.valid_from > scope_start {
            bail!(
                "product specification starts after scope for {}",
                contract.symbol
            );
        }
        let last = relevant
            .last()
            .expect("relevant product versions are nonempty");
        if last.valid_to.is_some_and(|end| end < scope_end) {
            bail!(
                "product specification ends before expiry for {}",
                contract.symbol
            );
        }
    }
    Ok(())
}

fn relevant_versions(
    versions: &[ProductVersion],
    scope_start: Date,
    scope_end: Date,
) -> Vec<&ProductVersion> {
    versions
        .iter()
        .filter(|version| {
            version.valid_from < scope_end && version.valid_to.is_none_or(|end| end > scope_start)
        })
        .collect()
}

fn persist_contracts(
    transaction: &Transaction<'_>,
    contracts: &[Contract],
    versions_by_product: &BTreeMap<String, Vec<ProductVersion>>,
    observed_at: &str,
) -> Result<usize> {
    let mut inserted = 0;
    for contract in contracts {
        let scope_end = contract
            .expiry
            .next_day()
            .ok_or_else(|| anyhow!("contract expiry cannot advance: {}", contract.symbol))?;
        let versions = versions_by_product
            .get(&contract.product)
            .ok_or_else(|| anyhow!("missing validated product {}", contract.product))?;
        transaction.execute(
            "delete from contract_spec_evidence where contract_id = ?1",
            [contract.id],
        )?;
        transaction.execute(
            "delete from contract_spec_versions where contract_id = ?1",
            [contract.id],
        )?;
        let mut last_values = None;
        for version in versions {
            let valid_from = contract.listing.max(version.valid_from);
            let valid_to = version.valid_to.map_or(scope_end, |end| end.min(scope_end));
            if valid_from >= valid_to {
                continue;
            }
            let valid_from = exchange_start(valid_from)?;
            let valid_to = exchange_start(valid_to)?;
            transaction.execute(
                "insert into contract_spec_versions(
                     contract_id, lot_size, tick_size, valid_from, valid_to,
                     source_kind, source_url, first_seen_at, last_seen_at
                 ) values(?1, ?2, ?3, ?4, ?5, 'official', ?6, ?7, ?7)",
                params![
                    contract.id,
                    version.lot_size,
                    version.tick_size,
                    valid_from,
                    valid_to,
                    version.canonical_url,
                    observed_at
                ],
            )?;
            transaction.execute(
                "insert into contract_spec_evidence(
                     contract_id, valid_from, canonical_url, body_sha256, recorded_at
                 ) values(?1, ?2, ?3, ?4, ?5)",
                params![
                    contract.id,
                    valid_from,
                    version.canonical_url,
                    version.sha256,
                    observed_at
                ],
            )?;
            last_values = Some((version.lot_size, version.tick_size));
            inserted += 1;
        }
        let (lot_size, tick_size) = last_values
            .ok_or_else(|| anyhow!("no product specification written for {}", contract.symbol))?;
        transaction.execute(
            "update contracts set lot_size = ?1, tick_size = ?2 where id = ?3",
            params![lot_size, tick_size, contract.id],
        )?;
    }
    Ok(inserted)
}

fn validate_exchange(exchange: &str) -> Result<()> {
    if !matches!(exchange, "SHFE" | "INE") {
        bail!("product specification importer supports only SHFE or INE");
    }
    Ok(())
}

fn validate_product(product: &str) -> Result<()> {
    if product.is_empty() || !product.bytes().all(|byte| byte.is_ascii_lowercase()) {
        bail!("invalid product specification product: {product}");
    }
    Ok(())
}

fn validate_number(value: f64, field: &str, product: &str) -> Result<()> {
    if !value.is_finite() || value <= 0.0 {
        bail!("invalid product specification {field} for {product}");
    }
    Ok(())
}

fn validate_specification_url(exchange: &str, value: &str) -> Result<()> {
    let url = Url::parse(value)?;
    let allowed_path = url.path().starts_with("/products/futures/")
        || (exchange == "SHFE" && url.path().starts_with("/upload/"))
        || (exchange == "SHFE"
            && url.path().starts_with("/publicnotice/notice/")
            && matches!(
                url.path().rsplit_once('.').map(|(_, extension)| extension),
                Some("doc" | "docx" | "pdf")
            ));
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || !allowed_path
    {
        bail!("product specification URL must be an official HTTPS product page or attachment");
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("product specification URL has no host"))?
        .to_ascii_lowercase();
    let allowed = match exchange {
        "SHFE" => matches!(
            host.as_str(),
            "shfe.com.cn" | "www.shfe.com.cn" | "shfe.cn" | "www.shfe.cn"
        ),
        "INE" => matches!(
            host.as_str(),
            "ine.cn" | "www.ine.cn" | "ine.com.cn" | "www.ine.com.cn"
        ),
        _ => false,
    };
    if !allowed {
        bail!("product specification URL does not use an official {exchange} host");
    }
    Ok(())
}

fn optional_text(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn parse_date(value: &str) -> Result<Date> {
    let format = time::format_description::parse("[year]-[month]-[day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid specification date {value}"))
}

fn parse_compact_date(value: &str) -> Result<Date> {
    let format = time::format_description::parse("[year][month][day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid lifecycle date {value}"))
}

fn exchange_start(date: Date) -> Result<String> {
    let offset = UtcOffset::from_hms(8, 0, 0)?;
    Ok(date.midnight().assume_offset(offset).format(&Rfc3339)?)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid product specification SHA-256: {value}");
    }
    Ok(())
}

fn verify_retained_evidence(snapshot_dir: &Path, expected_sha256: &str) -> Result<()> {
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
        bail!("retained product specification evidence must resolve uniquely: {expected_sha256}");
    }
    let actual = hex::encode(Sha256::digest(std::fs::read(&matches[0])?));
    if actual != expected_sha256 {
        bail!("retained product specification evidence SHA-256 mismatch: {expected_sha256}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_specification_url;

    #[test]
    fn specification_url_accepts_exchange_linked_shfe_upload() {
        validate_specification_url(
            "SHFE",
            "https://www.shfe.cn/upload/20211109/1636450109020.doc",
        )
        .unwrap();
    }

    #[test]
    fn specification_url_accepts_shfe_notice_attachment() {
        validate_specification_url(
            "SHFE",
            "https://www.shfe.cn/publicnotice/notice/201912/P020240320685331759846.docx",
        )
        .unwrap();
    }

    #[test]
    fn specification_url_rejects_foreign_upload_host() {
        assert!(
            validate_specification_url(
                "SHFE",
                "https://example.com/upload/20211109/1636450109020.doc",
            )
            .is_err()
        );
    }
}
