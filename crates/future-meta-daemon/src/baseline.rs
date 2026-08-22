//! Import and validate the reviewed V11 fee-history baseline.

use crate::db::{self, connect, ensure_schema};
use crate::jin10::ContractStaticMetadata;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use csv::ReaderBuilder;
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use rusqlite::{Connection, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Immutable V11 baseline identifier required by the live updater.
pub const V11_BASELINE_VERSION: &str = "v11";

/// Result of importing a reviewed V11 TSV file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaselineImport {
    pub rows: usize,
    pub contracts: usize,
    pub source_sha256: String,
}

/// Import a reviewed V11 TSV into an empty `SQLite` database.
///
/// Contract static metadata is read from a separate, existing database. Exact
/// contract metadata is preferred; otherwise the product must have one
/// unambiguous verified specification. The sole known historical GFEX lithium
/// tick change is resolved by its documented 2024-12 effective boundary.
///
/// # Errors
///
/// Returns an error when the TSV is malformed, intervals or fee JSON are
/// invalid, static metadata cannot be determined, or the target is non-empty.
pub fn import_v11_baseline(
    db_path: &Path,
    input_path: &Path,
    metadata_db_path: &Path,
) -> Result<BaselineImport> {
    let bytes = std::fs::read(input_path)
        .with_context(|| format!("read V11 baseline {}", input_path.display()))?;
    let source_sha256 = hex::encode(Sha256::digest(&bytes));
    let rows = parse_v11_rows(&bytes)?;

    let metadata_db = connect(metadata_db_path)?;
    ensure_schema(&metadata_db)?;
    let resolver = StaticMetadataResolver::load(&metadata_db)?;
    let allowed_rows = rows
        .rows
        .iter()
        .map(|row| row.to_allowed_row(&resolver))
        .collect::<Result<Vec<_>>>()?;

    let mut conn = connect(db_path)?;
    ensure_schema(&conn)?;
    let existing = db::history_counts(&conn)?;
    if existing.contracts != 0 || existing.fee_versions != 0 {
        bail!(
            "V11 baseline import requires an empty target database; contracts={} fee_versions={}",
            existing.contracts,
            existing.fee_versions
        );
    }

    let imported_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    db::upsert_v11_baseline_rows(&mut conn, &allowed_rows, &imported_at)?;
    conn.execute(
        "insert into baseline_state(
           baseline_version, source_sha256, row_count, contract_count, imported_at
         ) values (?1, ?2, ?3, ?4, ?5)",
        params![
            V11_BASELINE_VERSION,
            source_sha256,
            i64::try_from(allowed_rows.len())?,
            i64::try_from(rows.contracts.len())?,
            imported_at,
        ],
    )?;

    Ok(BaselineImport {
        rows: allowed_rows.len(),
        contracts: rows.contracts.len(),
        source_sha256,
    })
}

/// Require that a database was constructed from an explicitly recorded V11
/// baseline before it is allowed to consume live latest-table data.
///
/// # Errors
///
/// Returns an error when the database is unavailable or has no V11 manifest.
pub fn ensure_v11_baseline(conn: &Connection) -> Result<()> {
    ensure_schema(conn)?;
    let exists: bool = conn.query_row(
        "select exists(
           select 1 from baseline_state where baseline_version = ?1
         )",
        [V11_BASELINE_VERSION],
        |row| row.get(0),
    )?;
    if !exists {
        bail!("V11 baseline is required before latest updates")
    }
    Ok(())
}

#[derive(Debug)]
struct ParsedV11Rows {
    rows: Vec<V11Row>,
    contracts: BTreeSet<String>,
}

fn parse_v11_rows(bytes: &[u8]) -> Result<ParsedV11Rows> {
    let mut reader = ReaderBuilder::new().delimiter(b'\t').from_reader(bytes);
    let mut rows = Vec::new();
    let mut contracts = BTreeSet::new();
    for record in reader.deserialize::<V11TsvRow>() {
        let record = record?;
        let row = V11Row::parse(&record)?;
        if !contracts.insert(row.symbol.clone())
            && rows.iter().any(|prior: &V11Row| {
                prior.symbol == row.symbol && prior.valid_from == row.valid_from
            })
        {
            bail!(
                "duplicate V11 interval start for {} at {}",
                row.symbol,
                row.valid_from
            );
        }
        rows.push(row);
    }
    if rows.is_empty() {
        bail!("V11 baseline TSV contains no data rows");
    }
    validate_intervals(&rows)?;
    Ok(ParsedV11Rows { rows, contracts })
}

fn validate_intervals(rows: &[V11Row]) -> Result<()> {
    let mut by_symbol = BTreeMap::<&str, Vec<&V11Row>>::new();
    for row in rows {
        by_symbol.entry(&row.symbol).or_default().push(row);
    }
    for (symbol, rows) in &mut by_symbol {
        rows.sort_by(|left, right| left.valid_from.cmp(&right.valid_from));
        for (index, row) in rows.iter().enumerate() {
            let next = rows.get(index + 1);
            match (row.valid_to.as_deref(), next) {
                (Some(valid_to), Some(next)) if valid_to == next.valid_from => {}
                (None, None) => {}
                (Some(_), None) => bail!("V11 terminal interval unexpectedly closes for {symbol}"),
                (None, Some(_)) => {
                    bail!("V11 interval has no valid_to before successor for {symbol}")
                }
                (Some(_), Some(_)) => bail!("V11 interval boundary mismatch for {symbol}"),
            }
        }
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct V11TsvRow {
    symbol: String,
    valid_from: String,
    valid_to: String,
    open_fee: String,
    close_yesterday_fee: String,
    close_today_fee: String,
}

#[derive(Debug)]
struct V11Row {
    symbol: String,
    valid_from: String,
    valid_to: Option<String>,
    open_fee: FeeSpec,
    close_yesterday_fee: FeeSpec,
    close_today_fee: FeeSpec,
}

impl V11Row {
    fn parse(source: &V11TsvRow) -> Result<Self> {
        let symbol = source.symbol.trim().to_owned();
        derive_underlying_symbol(&symbol)?;
        OffsetDateTime::parse(source.valid_from.trim(), &Rfc3339)
            .with_context(|| format!("invalid V11 valid_from for {symbol}"))?;
        let valid_to =
            (!source.valid_to.trim().is_empty()).then(|| source.valid_to.trim().to_owned());
        if let Some(valid_to) = &valid_to {
            let valid_to_at = OffsetDateTime::parse(valid_to, &Rfc3339)
                .with_context(|| format!("invalid V11 valid_to for {symbol}"))?;
            if valid_to_at <= OffsetDateTime::parse(source.valid_from.trim(), &Rfc3339)? {
                bail!("V11 valid_to is not after valid_from for {symbol}");
            }
        }
        let open_fee = parse_fee(&source.open_fee, &symbol, "open_fee")?;
        let close_yesterday_fee =
            parse_fee(&source.close_yesterday_fee, &symbol, "close_yesterday_fee")?;
        let close_today_fee = parse_fee(&source.close_today_fee, &symbol, "close_today_fee")?;
        Ok(Self {
            symbol,
            valid_from: source.valid_from.trim().to_owned(),
            valid_to,
            open_fee,
            close_yesterday_fee,
            close_today_fee,
        })
    }

    fn to_allowed_row(&self, resolver: &StaticMetadataResolver) -> Result<AllowedRow> {
        let metadata = resolver.resolve(&self.symbol)?;
        Ok(AllowedRow {
            symbol: self.symbol.clone(),
            listing_date: None,
            expiry_date: None,
            trading_status: TradingStatus::Unknown,
            buy_margin_rate: None,
            sell_margin_rate: None,
            open_fee: self.open_fee.clone(),
            close_yesterday_fee: self.close_yesterday_fee.clone(),
            close_today_fee: self.close_today_fee.clone(),
            lot_size: metadata.lot_size,
            tick_size: metadata.tick_size,
            source_updated_at: Some(self.valid_from.clone()),
            is_main_contract: false,
        })
    }
}

fn parse_fee(text: &str, symbol: &str, field: &str) -> Result<FeeSpec> {
    let fee: FeeSpec = serde_json::from_str(text)
        .with_context(|| format!("invalid V11 {field} JSON for {symbol}"))?;
    if fee.kind == FeeKind::Unknown || fee.value.is_some_and(|value| !value.is_finite()) {
        bail!("invalid V11 {field} for {symbol}");
    }
    Ok(fee)
}

struct StaticMetadataResolver {
    exact: BTreeMap<String, ContractStaticMetadata>,
    products: BTreeMap<String, Vec<ContractStaticMetadata>>,
}

impl StaticMetadataResolver {
    fn load(conn: &Connection) -> Result<Self> {
        let mut statement = conn.prepare("select symbol, lot_size, tick_size from contracts")?;
        let exact = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    ContractStaticMetadata {
                        lot_size: row.get(1)?,
                        tick_size: row.get(2)?,
                    },
                ))
            })?
            .collect::<rusqlite::Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            exact,
            products: db::product_static_metadata_candidates(conn)?,
        })
    }

    fn resolve(&self, symbol: &str) -> Result<ContractStaticMetadata> {
        if let Some(metadata) = self.exact.get(symbol) {
            return Ok(*metadata);
        }
        let product = derive_underlying_symbol(symbol)?;
        let candidates = self
            .products
            .get(&product)
            .ok_or_else(|| anyhow!("missing static metadata for V11 product {product}"))?;
        if let [metadata] = candidates.as_slice() {
            return Ok(*metadata);
        }
        if product == "GFEX.lc" {
            let expected_tick: f64 = if symbol.starts_with("GFEX.lc24") {
                50.0
            } else {
                20.0
            };
            return candidates
                .iter()
                .copied()
                .find(|metadata| metadata.tick_size.to_bits() == expected_tick.to_bits())
                .ok_or_else(|| anyhow!("missing GFEX.lc tick {expected_tick} for {symbol}"));
        }
        bail!("ambiguous static metadata for V11 symbol {symbol}")
    }
}
