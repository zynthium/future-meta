//! `SQLite` schema and version maintenance.

use crate::hash::row_rule_hash;
use crate::latest::LatestRow;
use crate::parse::AllowedRow;
use anyhow::{Result, anyhow};
use future_meta::model::{FeeSpec, TradingStatus};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Minimal history table counts used by update safety checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCounts {
    pub contracts: i64,
    pub fee_versions: i64,
}

/// Result of completing latest table rows with persisted contract metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestCompletion {
    pub rows: Vec<AllowedRow>,
    pub skipped_missing_metadata: usize,
}

/// Open a `SQLite` connection, creating the database parent directory first.
///
/// # Errors
///
/// Returns an error when the parent directory cannot be created or the database
/// cannot be opened.
pub fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = non_empty_parent(path) {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.execute_batch("pragma foreign_keys = on;")?;
    Ok(conn)
}

/// Ensure the daemon history schema exists.
///
/// # Errors
///
/// Returns an error when `SQLite` schema creation fails.
pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        pragma foreign_keys = on;

        create table if not exists contracts(
          id integer primary key,
          symbol text not null unique,
          listing_date text,
          expiry_date text,
          lot_size real not null check(lot_size > 0),
          tick_size real not null check(tick_size > 0),
          first_seen_at text not null,
          last_seen_at text not null,
          active integer not null check(active in (0, 1))
        );

        create table if not exists fee_versions(
          id integer primary key,
          contract_id integer not null,
          rule_hash text not null check(length(rule_hash) > 0),
          buy_margin_rate real,
          sell_margin_rate real,
          open_fee_json text not null check(json_valid(open_fee_json)),
          close_yesterday_fee_json text not null check(json_valid(close_yesterday_fee_json)),
          close_today_fee_json text not null check(json_valid(close_today_fee_json)),
          trading_status text not null check(trading_status in ('Trading', 'NotTrading', 'Unknown')),
          is_main_contract integer not null check(is_main_contract in (0, 1)),
          source_updated_at text,
          valid_from text not null,
          valid_to text check(valid_to is null or julianday(valid_to) > julianday(valid_from)),
          first_seen_at text not null,
          last_seen_at text not null,
          foreign key(contract_id) references contracts(id)
        );

        create index if not exists idx_fee_versions_contract
          on fee_versions(contract_id, valid_from);
        create unique index if not exists idx_fee_versions_open_contract
          on fee_versions(contract_id)
          where valid_to is null;
        create unique index if not exists idx_fee_versions_contract_valid_from
          on fee_versions(contract_id, valid_from);

        create table if not exists source_state(
          source_url text primary key,
          last_probe_hash text,
          last_rule_set_hash text,
          last_success_at text,
          last_error_at text,
          last_error_message text
        );
        ",
    )?;

    Ok(())
}

/// Return the last successful probe hash for a source URL.
///
/// # Errors
///
/// Returns an error when the source state query fails.
pub fn source_probe_hash(conn: &Connection, source_url: &str) -> Result<Option<String>> {
    ensure_schema(conn)?;
    Ok(conn
        .query_row(
            "select last_probe_hash from source_state where source_url = ?1",
            params![source_url],
            |row| row.get(0),
        )
        .optional()?)
}

/// Return the last successful rule-set hash for a source URL.
///
/// # Errors
///
/// Returns an error when the source state query fails.
pub fn source_rule_set_hash(conn: &Connection, source_url: &str) -> Result<Option<String>> {
    ensure_schema(conn)?;
    Ok(conn
        .query_row(
            "select last_rule_set_hash from source_state where source_url = ?1",
            params![source_url],
            |row| row.get(0),
        )
        .optional()?)
}

/// Record a successful source refresh.
///
/// # Errors
///
/// Returns an error when the source state update fails.
pub fn update_source_success(
    conn: &Connection,
    source_url: &str,
    probe_hash: &str,
    rule_set_hash: &str,
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "insert into source_state(source_url, last_probe_hash, last_rule_set_hash, last_success_at)
         values (?1, ?2, ?3, ?4)
         on conflict(source_url) do update set
           last_probe_hash = excluded.last_probe_hash,
           last_rule_set_hash = excluded.last_rule_set_hash,
           last_success_at = excluded.last_success_at,
           last_error_at = null,
           last_error_message = null",
        params![source_url, probe_hash, rule_set_hash, observed_at],
    )?;
    Ok(())
}

/// Record a failed source refresh without clearing the last successful state.
///
/// # Errors
///
/// Returns an error when the source state update fails.
pub fn update_source_error(
    conn: &Connection,
    source_url: &str,
    observed_at: &str,
    message: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "insert into source_state(source_url, last_error_at, last_error_message)
         values (?1, ?2, ?3)
         on conflict(source_url) do update set
           last_error_at = excluded.last_error_at,
           last_error_message = excluded.last_error_message",
        params![source_url, observed_at, message],
    )?;
    Ok(())
}

/// Insert or update allowed rows while preserving fee-rule history.
///
/// # Errors
///
/// Returns an error when schema creation, JSON serialization, or database writes
/// fail.
pub fn upsert_allowed_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let observed_at_timestamp = parse_timestamp("observed_at", observed_at)?;
    let prepared = prepare_rows(rows, observed_at)?;
    let tx = conn.transaction()?;

    let mut rows_by_symbol = BTreeMap::<String, Vec<PreparedRow>>::new();
    for row in prepared {
        rows_by_symbol
            .entry(row.row.symbol.clone())
            .or_default()
            .push(row);
    }

    for rows in rows_by_symbol.into_values() {
        let Some(latest_row) = rows
            .iter()
            .max_by(|left, right| left.valid_from_at.cmp(&right.valid_from_at))
            .map(|row| row.row.clone())
        else {
            continue;
        };

        let contract_id = upsert_contract(&tx, &latest_row, observed_at)?;
        validate_observed_at(&tx, contract_id, observed_at_timestamp)?;

        let mut versions = load_existing_versions(&tx, contract_id)?;
        versions.extend(rows.into_iter().map(|prepared| VersionRecord {
            row: prepared.row,
            rule_hash: prepared.rule_hash,
            valid_from: prepared.valid_from,
            valid_from_at: prepared.valid_from_at,
            first_seen_at: observed_at.to_owned(),
            last_seen_at: observed_at.to_owned(),
        }));

        let versions = merge_versions(versions)?;
        replace_fee_versions(&tx, contract_id, &versions)?;
    }

    tx.commit()?;
    Ok(())
}

/// Complete latest total-page rows with persisted contract metadata.
///
/// The total page currently exposes fee and margin rules but not all static
/// contract metadata. Missing static metadata is inherited only from existing
/// seeded contracts; rows that still lack required lot/tick values are skipped.
///
/// # Errors
///
/// Returns an error when schema creation or metadata lookup fails.
pub fn complete_latest_rows(conn: &Connection, rows: &[LatestRow]) -> Result<LatestCompletion> {
    ensure_schema(conn)?;
    let mut completed = Vec::new();
    let mut skipped_missing_metadata = 0usize;

    for row in rows {
        let metadata = load_contract_metadata(conn, &row.symbol)?;
        let listing_date = row.listing_date.clone().or_else(|| {
            metadata
                .as_ref()
                .and_then(|value| value.listing_date.clone())
        });
        let expiry_date = row.expiry_date.clone().or_else(|| {
            metadata
                .as_ref()
                .and_then(|value| value.expiry_date.clone())
        });
        let lot_size = row
            .lot_size
            .or_else(|| metadata.as_ref().map(|value| value.lot_size));
        let tick_size = row
            .tick_size
            .or_else(|| metadata.as_ref().map(|value| value.tick_size));
        let (Some(lot_size), Some(tick_size)) = (lot_size, tick_size) else {
            skipped_missing_metadata += 1;
            continue;
        };
        if !lot_size.is_finite() || lot_size <= 0.0 {
            return Err(anyhow!(
                "invalid latest lot_size for {}: {}",
                row.symbol,
                lot_size
            ));
        }
        if !tick_size.is_finite() || tick_size <= 0.0 {
            return Err(anyhow!(
                "invalid latest tick_size for {}: {}",
                row.symbol,
                tick_size
            ));
        }

        completed.push(AllowedRow {
            symbol: row.symbol.clone(),
            listing_date,
            expiry_date,
            trading_status: row.trading_status.clone(),
            buy_margin_rate: row.buy_margin_rate,
            sell_margin_rate: row.sell_margin_rate,
            open_fee: row.open_fee.clone(),
            close_yesterday_fee: row.close_yesterday_fee.clone(),
            close_today_fee: row.close_today_fee.clone(),
            lot_size,
            tick_size,
            source_updated_at: row.source_updated_at.clone(),
            is_main_contract: row.is_main_contract,
        });
    }

    Ok(LatestCompletion {
        rows: completed,
        skipped_missing_metadata,
    })
}

/// Return current history table counts.
///
/// # Errors
///
/// Returns an error when schema creation or count queries fail.
pub fn history_counts(conn: &Connection) -> Result<HistoryCounts> {
    ensure_schema(conn)?;
    let contracts = conn.query_row("select count(*) from contracts", [], |row| row.get(0))?;
    let fee_versions = conn.query_row("select count(*) from fee_versions", [], |row| row.get(0))?;
    Ok(HistoryCounts {
        contracts,
        fee_versions,
    })
}

/// Fail if a daemon update is about to run without a local seed/history base.
///
/// # Errors
///
/// Returns an error when the database has no contract or fee history rows.
pub fn ensure_seeded(conn: &Connection) -> Result<()> {
    let counts = history_counts(conn)?;
    if counts.contracts == 0 || counts.fee_versions == 0 {
        return Err(anyhow!(
            "seeded daemon database is required before update; run a local full seed and publish ops/future-meta.sqlite.gz first"
        ));
    }
    Ok(())
}

fn replace_fee_versions(
    tx: &Transaction<'_>,
    contract_id: i64,
    versions: &[VersionRecord],
) -> Result<()> {
    tx.execute(
        "delete from fee_versions where contract_id = ?1",
        params![contract_id],
    )?;

    for (index, version) in versions.iter().enumerate() {
        let valid_to = versions.get(index + 1).map(|next| next.valid_from.as_str());
        insert_fee_version(tx, version, contract_id, valid_to)?;
    }

    Ok(())
}

fn load_existing_versions(tx: &Transaction<'_>, contract_id: i64) -> Result<Vec<VersionRecord>> {
    let mut stmt = tx.prepare(
        "select c.symbol, c.listing_date, c.expiry_date, c.lot_size, c.tick_size,
                v.rule_hash, v.buy_margin_rate, v.sell_margin_rate,
                v.open_fee_json, v.close_yesterday_fee_json, v.close_today_fee_json,
                v.trading_status, v.is_main_contract, v.source_updated_at,
                v.valid_from, v.first_seen_at, v.last_seen_at
         from fee_versions v
         join contracts c on c.id = v.contract_id
         where v.contract_id = ?1
         order by v.valid_from, v.id",
    )?;

    let mut rows = stmt.query(params![contract_id])?;
    let mut versions = Vec::new();
    while let Some(record) = rows.next()? {
        let valid_from: String = record.get(14)?;
        versions.push(VersionRecord {
            row: AllowedRow {
                symbol: record.get(0)?,
                listing_date: record.get(1)?,
                expiry_date: record.get(2)?,
                lot_size: record.get(3)?,
                tick_size: record.get(4)?,
                trading_status: parse_trading_status_text(&record.get::<_, String>(11)?)?,
                buy_margin_rate: record.get(6)?,
                sell_margin_rate: record.get(7)?,
                open_fee: parse_fee_json(&record.get::<_, String>(8)?)?,
                close_yesterday_fee: parse_fee_json(&record.get::<_, String>(9)?)?,
                close_today_fee: parse_fee_json(&record.get::<_, String>(10)?)?,
                source_updated_at: record.get(13)?,
                is_main_contract: record.get::<_, i64>(12)? != 0,
            },
            rule_hash: record.get(5)?,
            valid_from_at: parse_timestamp("valid_from", &valid_from)?,
            valid_from,
            first_seen_at: record.get(15)?,
            last_seen_at: record.get(16)?,
        });
    }

    Ok(versions)
}

fn parse_fee_json(json: &str) -> Result<FeeSpec> {
    serde_json::from_str(json).map_err(Into::into)
}

fn parse_trading_status_text(text: &str) -> Result<TradingStatus> {
    match text {
        "Trading" => Ok(TradingStatus::Trading),
        "NotTrading" => Ok(TradingStatus::NotTrading),
        "Unknown" => Ok(TradingStatus::Unknown),
        _ => Err(anyhow!("unknown trading status: {text}")),
    }
}

fn merge_versions(mut versions: Vec<VersionRecord>) -> Result<Vec<VersionRecord>> {
    versions.sort_by(|left, right| {
        left.valid_from_at
            .cmp(&right.valid_from_at)
            .then_with(|| left.row.symbol.cmp(&right.row.symbol))
            .then_with(|| left.rule_hash.cmp(&right.rule_hash))
    });

    let mut unique_times = Vec::<VersionRecord>::new();
    for version in versions {
        if let Some(last) = unique_times.last_mut()
            && last.valid_from_at == version.valid_from_at
        {
            if last.rule_hash != version.rule_hash {
                return Err(anyhow!(
                    "conflicting rules for duplicate symbol {} at {}",
                    version.row.symbol,
                    version.valid_from
                ));
            }
            merge_equivalent_version(last, version)?;
            continue;
        }

        unique_times.push(version);
    }

    let mut collapsed = Vec::<VersionRecord>::new();
    for version in unique_times {
        if let Some(last) = collapsed.last_mut()
            && last.rule_hash == version.rule_hash
        {
            merge_equivalent_version(last, version)?;
            continue;
        }

        collapsed.push(version);
    }

    Ok(collapsed)
}

fn merge_equivalent_version(left: &mut VersionRecord, right: VersionRecord) -> Result<()> {
    left.row.source_updated_at = max_optional_source_updated(
        left.row.source_updated_at.take(),
        right.row.source_updated_at,
    )?;
    left.first_seen_at =
        min_timestamp_text("first_seen_at", &left.first_seen_at, &right.first_seen_at)?;
    left.last_seen_at =
        max_timestamp_text("last_seen_at", &left.last_seen_at, &right.last_seen_at)?;
    Ok(())
}

fn min_timestamp_text(field: &str, left: &str, right: &str) -> Result<String> {
    if parse_timestamp(field, left)? <= parse_timestamp(field, right)? {
        Ok(left.to_owned())
    } else {
        Ok(right.to_owned())
    }
}

fn max_timestamp_text(field: &str, left: &str, right: &str) -> Result<String> {
    if parse_timestamp(field, left)? >= parse_timestamp(field, right)? {
        Ok(left.to_owned())
    } else {
        Ok(right.to_owned())
    }
}

fn max_optional_source_updated(
    left: Option<String>,
    right: Option<String>,
) -> Result<Option<String>> {
    match (left, right) {
        (Some(left), Some(right)) => {
            if parse_source_updated_at(&left)? >= parse_source_updated_at(&right)? {
                Ok(Some(left))
            } else {
                Ok(Some(right))
            }
        }
        (Some(value), None) | (None, Some(value)) => Ok(Some(value)),
        (None, None) => Ok(None),
    }
}

fn validate_observed_at(
    tx: &Transaction<'_>,
    contract_id: i64,
    observed_at: OffsetDateTime,
) -> Result<()> {
    let last_seen_at = tx
        .query_row(
            "select max(last_seen_at) from fee_versions where contract_id = ?1",
            params![contract_id],
            |record| record.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();

    if let Some(last_seen_at) = last_seen_at {
        let last_seen_timestamp = parse_timestamp("last_seen_at", &last_seen_at)?;
        if observed_at < last_seen_timestamp {
            return Err(anyhow!(
                "observed_at is older than current last_seen_at: {observed_at} < {last_seen_at}"
            ));
        }
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct VersionRecord {
    row: AllowedRow,
    rule_hash: String,
    valid_from: String,
    valid_from_at: OffsetDateTime,
    first_seen_at: String,
    last_seen_at: String,
}

#[derive(Debug)]
struct PreparedRow {
    row: AllowedRow,
    rule_hash: String,
    valid_from: String,
    valid_from_at: OffsetDateTime,
}

fn prepare_rows(rows: &[AllowedRow], observed_at: &str) -> Result<Vec<PreparedRow>> {
    rows.iter()
        .map(|row| {
            let (valid_from, valid_from_at) = row_valid_from(row, observed_at)?;
            Ok(PreparedRow {
                row: row.clone(),
                rule_hash: row_rule_hash(row),
                valid_from,
                valid_from_at,
            })
        })
        .collect()
}

fn row_valid_from(row: &AllowedRow, observed_at: &str) -> Result<(String, OffsetDateTime)> {
    let valid_from_at = row
        .source_updated_at
        .as_deref()
        .map(parse_source_updated_at)
        .transpose()?
        .unwrap_or_else(|| {
            parse_timestamp("observed_at", observed_at)
                .expect("observed_at should have been validated before row preparation")
        });
    Ok((valid_from_at.format(&Rfc3339)?, valid_from_at))
}

fn parse_source_updated_at(value: &str) -> Result<OffsetDateTime> {
    let trimmed = value.trim();
    if let Ok(timestamp) = OffsetDateTime::parse(trimmed, &Rfc3339) {
        return Ok(timestamp);
    }

    let mut parts = trimmed.split_whitespace();
    let date = parts
        .next()
        .ok_or_else(|| anyhow!("invalid source_updated_at timestamp {value}"))?;
    let time = parts
        .next()
        .ok_or_else(|| anyhow!("invalid source_updated_at timestamp {value}"))?;
    if parts.next().is_some() {
        return Err(anyhow!("invalid source_updated_at timestamp {value}"));
    }

    parse_timestamp("source_updated_at", &format!("{date}T{time}+08:00"))
}

/// Inspect database contents.
///
/// # Errors
///
/// Returns an error if inspection fails.
pub fn inspect(db: &Path) -> Result<()> {
    let conn = connect(db)?;
    let counts = history_counts(&conn)?;
    println!(
        "contracts={} fee_versions={}",
        counts.contracts, counts.fee_versions
    );
    Ok(())
}

fn non_empty_parent(path: &Path) -> Option<PathBuf> {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
}

#[derive(Debug)]
struct ContractMetadata {
    listing_date: Option<String>,
    expiry_date: Option<String>,
    lot_size: f64,
    tick_size: f64,
}

fn load_contract_metadata(conn: &Connection, symbol: &str) -> Result<Option<ContractMetadata>> {
    conn.query_row(
        "select listing_date, expiry_date, lot_size, tick_size from contracts where symbol = ?1",
        params![symbol],
        |row| {
            Ok(ContractMetadata {
                listing_date: row.get(0)?,
                expiry_date: row.get(1)?,
                lot_size: row.get(2)?,
                tick_size: row.get(3)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn upsert_contract(tx: &Transaction<'_>, row: &AllowedRow, observed_at: &str) -> Result<i64> {
    tx.execute(
        "insert into contracts(
           symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         )
         values (?1, ?2, ?3, ?4, ?5, ?6, ?6, 1)
         on conflict(symbol) do update set
           listing_date = excluded.listing_date,
           expiry_date = excluded.expiry_date,
           lot_size = excluded.lot_size,
           tick_size = excluded.tick_size,
           last_seen_at = excluded.last_seen_at,
           active = 1",
        params![
            row.symbol.as_str(),
            row.listing_date.as_deref(),
            row.expiry_date.as_deref(),
            row.lot_size,
            row.tick_size,
            observed_at,
        ],
    )?;

    Ok(tx.query_row(
        "select id from contracts where symbol = ?1",
        params![row.symbol.as_str()],
        |record| record.get(0),
    )?)
}

fn insert_fee_version(
    tx: &Transaction<'_>,
    version: &VersionRecord,
    contract_id: i64,
    valid_to: Option<&str>,
) -> Result<()> {
    let row = &version.row;
    tx.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         )
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            contract_id,
            version.rule_hash.as_str(),
            row.buy_margin_rate,
            row.sell_margin_rate,
            serde_json::to_string(&row.open_fee)?,
            serde_json::to_string(&row.close_yesterday_fee)?,
            serde_json::to_string(&row.close_today_fee)?,
            trading_status_text(&row.trading_status),
            bool_to_i64(row.is_main_contract),
            row.source_updated_at.as_deref(),
            version.valid_from.as_str(),
            valid_to,
            version.first_seen_at.as_str(),
            version.last_seen_at.as_str(),
        ],
    )?;

    Ok(())
}

const fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn trading_status_text(status: &TradingStatus) -> &'static str {
    match status {
        TradingStatus::Trading => "Trading",
        TradingStatus::NotTrading => "NotTrading",
        TradingStatus::Unknown => "Unknown",
    }
}

fn parse_timestamp(field: &str, value: &str) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|err| anyhow!("invalid {field} timestamp {value}: {err}"))
}
