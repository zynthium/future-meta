//! `SQLite` schema and version maintenance.

use crate::hash::row_rule_hash;
use crate::jin10::ContractStaticMetadata;
use crate::latest::LatestRow;
use crate::parse::AllowedRow;
use anyhow::{Result, anyhow};
use future_meta::model::{FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

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

/// A fee mismatch found while comparing a secondary source with the current
/// production rule. This is audit output only; it is never inserted into
/// `fee_versions`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct FeeRuleDifference {
    pub symbol: String,
    pub production: [FeeSpec; 3],
    pub secondary: [FeeSpec; 3],
}

/// A 9qihuo latest-table fee candidate that did not receive same-day Jin10
/// confirmation. Rejections are deliberately kept out of `fee_versions`.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestCandidateRejection {
    pub symbol: String,
    pub reason: String,
}

/// Result of applying the two-source admission gate to latest fee candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestCandidateVerification {
    pub accepted: Vec<AllowedRow>,
    pub unchanged: usize,
    pub rejected: Vec<LatestCandidateRejection>,
}

/// Keep only 9qihuo fee changes corroborated by Jin10 for the same exchange
/// day and the same concrete contract. Rows whose fee tuple is unchanged are
/// omitted, so presentation-only source changes cannot create history.
///
/// This function is intentionally read-only. Callers must abort an update when
/// `rejected` is non-empty rather than partially advancing source state.
///
/// # Errors
///
/// Returns an error when current fee rules cannot be read from the database.
pub fn cross_verify_latest_candidates(
    conn: &Connection,
    candidates: &[AllowedRow],
    jin10_rows: &[AllowedRow],
) -> Result<LatestCandidateVerification> {
    let mut jin10_by_key = BTreeMap::<(String, String), &AllowedRow>::new();
    for row in jin10_rows {
        let Some(day) = source_day(row) else {
            continue;
        };
        jin10_by_key.insert((row.symbol.clone(), day.to_owned()), row);
    }

    let mut accepted = Vec::new();
    let mut unchanged = 0usize;
    let mut rejected = Vec::new();
    for candidate in candidates {
        let candidate_fees = fee_tuple(candidate);
        let Some(current) = current_fee_rule(conn, &candidate.symbol)? else {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: "contract missing from approved baseline".to_owned(),
            });
            continue;
        };
        if same_fee_rules(&current.fees, &candidate_fees) {
            unchanged += 1;
            continue;
        }

        let Some(day) = source_day(candidate) else {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: "9qihuo candidate has no source update day".to_owned(),
            });
            continue;
        };
        let Some(jin10) = jin10_by_key.get(&(candidate.symbol.clone(), day.to_owned())) else {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: "no same-day Jin10 contract observation".to_owned(),
            });
            continue;
        };
        if !same_fee_rules(&candidate_fees, &fee_tuple(jin10)) {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: "9qihuo and Jin10 fee tuples disagree".to_owned(),
            });
            continue;
        }
        accepted.push(candidate.clone());
    }

    Ok(LatestCandidateVerification {
        accepted,
        unchanged,
        rejected,
    })
}

fn source_day(row: &AllowedRow) -> Option<&str> {
    row.source_updated_at
        .as_deref()
        .and_then(|value| value.get(..10))
}

fn fee_tuple(row: &AllowedRow) -> [FeeSpec; 3] {
    [
        row.open_fee.clone(),
        row.close_yesterday_fee.clone(),
        row.close_today_fee.clone(),
    ]
}

/// Compare externally observed fee rows with current production rules without
/// changing the database.
///
/// # Errors
///
/// Returns an error when current fee rules cannot be read from the database.
pub fn compare_fee_rows(
    conn: &Connection,
    rows: &[AllowedRow],
) -> Result<(usize, Vec<FeeRuleDifference>)> {
    let mut compared = 0usize;
    let mut differences = Vec::new();
    for row in rows {
        let Some(current) = current_fee_rule(conn, &row.symbol)? else {
            continue;
        };
        compared += 1;
        let secondary = [
            row.open_fee.clone(),
            row.close_yesterday_fee.clone(),
            row.close_today_fee.clone(),
        ];
        if !same_fee_rules(&current.fees, &secondary) {
            differences.push(FeeRuleDifference {
                symbol: row.symbol.clone(),
                production: current.fees,
                secondary,
            });
        }
    }
    Ok((compared, differences))
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
          source_kind text not null default '9qihuo' check(source_kind in ('9qihuo', 'jin10', 'v11_baseline')),
          source_updated_at text,
          valid_from text not null,
          valid_to text check(valid_to is null or julianday(valid_to) > julianday(valid_from)),
          first_seen_at text not null,
          last_seen_at text not null,
          foreign key(contract_id) references contracts(id)
        );

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

        create table if not exists baseline_state(
          baseline_version text primary key,
          source_sha256 text not null check(length(source_sha256) = 64),
          row_count integer not null check(row_count > 0),
          contract_count integer not null check(contract_count > 0),
          imported_at text not null
        );

        create table if not exists fee_rule_conflicts(
          id integer primary key,
          contract_id integer not null,
          effective_at text not null,
          incumbent_source text not null check(incumbent_source in ('9qihuo', 'jin10')),
          contender_source text not null check(contender_source in ('9qihuo', 'jin10')),
          selected_source text not null check(selected_source in ('9qihuo', 'jin10')),
          incumbent_rule_hash text not null,
          contender_rule_hash text not null,
          incumbent_rule_json text not null check(json_valid(incumbent_rule_json)),
          contender_rule_json text not null check(json_valid(contender_rule_json)),
          reason text not null,
          recorded_at text not null,
          foreign key(contract_id) references contracts(id),
          unique(contract_id, effective_at, incumbent_rule_hash, contender_rule_hash)
        );

        create table if not exists jin10_source_snapshots(
          effective_at text primary key,
          completed_rows integer not null check(completed_rows > 0),
          skipped_invalid_symbols integer not null check(skipped_invalid_symbols >= 0),
          recorded_at text not null
        );
        ",
    )?;

    conn.execute_batch("drop index if exists idx_fee_versions_contract;")?;

    ensure_fee_version_source_kind_column(conn)?;
    repair_fee_versions_before_listing(conn)?;

    Ok(())
}

fn ensure_fee_version_source_kind_column(conn: &Connection) -> Result<()> {
    let exists = conn
        .query_row(
            "select 1 from pragma_table_info('fee_versions') where name = 'source_kind'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !exists {
        conn.execute(
            "alter table fee_versions
             add column source_kind text not null default '9qihuo'",
            [],
        )?;
    }
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
    upsert_rows(conn, rows, observed_at, IngestMode::Live)
}

/// Insert a pre-validated V11 history baseline into a new database.
///
/// Baseline rows use their V11 interval starts as source timestamps and are
/// inactive until a live latest-table update observes the contract.
///
/// # Errors
///
/// Returns an error when the rows cannot be validated or written.
pub fn upsert_v11_baseline_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<()> {
    upsert_rows(conn, rows, observed_at, IngestMode::V11Baseline)
}

/// Mark contracts observed in a trusted latest snapshot as active without
/// changing their fee history. This keeps a V11-imported contract catalogue
/// current even when the fee tuple itself did not change.
///
/// # Errors
///
/// Returns an error when the observation time is invalid or the database write fails.
pub fn mark_latest_contracts_seen(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    parse_timestamp("observed_at", observed_at)?;
    let tx = conn.transaction()?;
    for row in rows {
        tx.execute(
            "update contracts
             set active = 1,
                 last_seen_at = case
                   when julianday(?1) > julianday(last_seen_at) then ?1
                   else last_seen_at end
             where symbol = ?2",
            params![observed_at, row.symbol],
        )?;
        tx.execute(
            "update fee_versions
             set buy_margin_rate = ?1,
                 sell_margin_rate = ?2,
                 trading_status = ?3,
                 is_main_contract = ?4,
                 last_seen_at = case
                   when julianday(?5) > julianday(last_seen_at) then ?5
                   else last_seen_at end
             where contract_id = (select id from contracts where symbol = ?6)
               and valid_to is null",
            params![
                row.buy_margin_rate,
                row.sell_margin_rate,
                trading_status_text(&row.trading_status),
                i64::from(row.is_main_contract),
                observed_at,
                row.symbol,
            ],
        )?;
    }
    tx.commit()?;
    Ok(())
}

/// Insert older verified rows without regressing newer contract observations.
///
/// Historical rows still merge into the same SCD2 rule history, but their
/// observation time may predate data already present in the database.
/// Newly discovered historical contracts are marked inactive until a live
/// source observes them.
///
/// # Errors
///
/// Returns an error when schema creation, timestamp parsing, JSON
/// serialization, or database writes fail.
pub fn backfill_allowed_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<()> {
    upsert_rows(conn, rows, observed_at, IngestMode::Historical)
}

/// Close open versions for historical-only contracts at their last source day.
///
/// This prevents a finite source snapshot range from being queried as if it
/// continued indefinitely. Contracts already confirmed by a live source stay
/// open and are not touched.
///
/// # Errors
///
/// Returns an error when a supplied observation timestamp is invalid or an
/// update cannot be committed.
pub fn close_historical_fee_versions(
    conn: &mut Connection,
    last_observed_at_by_symbol: &BTreeMap<String, String>,
) -> Result<()> {
    ensure_schema(conn)?;
    let tx = conn.transaction()?;
    for (symbol, last_observed_at) in last_observed_at_by_symbol {
        let cutoff = (exchange_day_start(parse_timestamp("last_observed_at", last_observed_at)?)
            + Duration::days(1))
        .format(&Rfc3339)?;
        tx.execute(
            "update fee_versions
             set valid_to = ?1
             where id = (
               select fv.id
               from fee_versions fv
               join contracts c on c.id = fv.contract_id
               where c.symbol = ?2
                 and c.active = 0
                 and fv.valid_to is null
                 and julianday(fv.valid_from) < julianday(?1)
             )",
            params![cutoff, symbol],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn upsert_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
    mode: IngestMode,
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

        let contract_id = upsert_contract(&tx, &latest_row, observed_at, mode)?;
        if mode == IngestMode::Live {
            validate_observed_at(&tx, contract_id, observed_at_timestamp)?;
        }

        let mut versions = load_existing_versions(&tx, contract_id)?;
        versions.extend(rows.into_iter().map(|prepared| VersionRecord {
            row: prepared.row,
            rule_hash: prepared.rule_hash,
            valid_from: prepared.valid_from,
            valid_from_at: prepared.valid_from_at,
            first_seen_at: observed_at.to_owned(),
            last_seen_at: observed_at.to_owned(),
            source_kind: mode.source_kind(),
        }));

        if mode == IngestMode::Historical {
            let conflicts = reconcile_historical_source_conflicts(&mut versions)?;
            record_fee_rule_conflicts(&tx, contract_id, &conflicts, observed_at)?;
            versions = collapse_same_source_status_variants(versions)?;
        }
        let versions = merge_versions(versions)?;
        replace_fee_versions(&tx, contract_id, &versions)?;
    }

    tx.commit()?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IngestMode {
    Live,
    Historical,
    V11Baseline,
}

impl IngestMode {
    const fn source_kind(self) -> FeeSource {
        match self {
            Self::Live => FeeSource::NineQihuo,
            Self::Historical => FeeSource::Jin10,
            Self::V11Baseline => FeeSource::V11Baseline,
        }
    }
}

/// Apply latest-table rows while preserving an existing CSV rule when both
/// 9qihuo views report different rules for the same source update second.
///
/// The total table omits static metadata and is therefore a fallback to the
/// per-variety CSV seed. A disagreement for one source update must not replace
/// a rule already recorded from that CSV.
///
/// Returns the number of conflicting latest rows skipped.
///
/// # Errors
///
/// Returns an error when schema access, timestamps, or version maintenance
/// fails. Conflicts from different source update seconds remain errors.
pub fn upsert_latest_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    let product_candidate_counts = product_fee_kind_counts(rows)?;
    let current_product_counts = current_product_fee_kind_counts(conn)?;
    let mut skipped = 0usize;
    let mut accepted = Vec::with_capacity(rows.len());

    for row in rows {
        if is_isolated_tenth_placeholder(row, &product_candidate_counts, &current_product_counts)? {
            skipped += 1;
            continue;
        }
        let Some(row) = sanitize_latest_row(conn, row, observed_at)? else {
            skipped += 1;
            continue;
        };
        let candidate_fees = [
            row.open_fee.clone(),
            row.close_yesterday_fee.clone(),
            row.close_today_fee.clone(),
        ];
        if current_fee_rule(conn, &row.symbol)?
            .is_some_and(|current| same_fee_rules(&current.fees, &candidate_fees))
        {
            // The archive is a fee-history store. Margin, trading-state, and
            // main-contract presentation changes must not manufacture a fee
            // version when the three commission terms are unchanged.
            skipped += 1;
            continue;
        }
        if repair_isolated_tenth_incumbent(conn, &row, &current_product_counts, observed_at)? {
            skipped += 1;
            continue;
        }
        if latest_conflicts_with_csv_rule(conn, &row, observed_at)? {
            skipped += 1;
        } else {
            accepted.push(row);
        }
    }

    upsert_allowed_rows(conn, &accepted, observed_at)?;
    Ok(skipped)
}

type FeeKindSignature = [u8; 3];

fn fee_kind_signature(fees: &[FeeSpec; 3]) -> FeeKindSignature {
    fees.iter()
        .map(|fee| match fee.kind {
            future_meta::model::FeeKind::CnyPerLot => 0,
            future_meta::model::FeeKind::TurnoverRatePerTenThousand => 1,
            future_meta::model::FeeKind::Zero => 2,
            future_meta::model::FeeKind::Unknown => 3,
        })
        .collect::<Vec<_>>()
        .try_into()
        .expect("three fee kinds")
}

fn row_fee_kind_signature(row: &AllowedRow) -> FeeKindSignature {
    fee_kind_signature(&[
        row.open_fee.clone(),
        row.close_yesterday_fee.clone(),
        row.close_today_fee.clone(),
    ])
}

fn product_fee_kind_counts(
    rows: &[AllowedRow],
) -> Result<BTreeMap<String, BTreeMap<FeeKindSignature, usize>>> {
    let mut counts = BTreeMap::<String, BTreeMap<FeeKindSignature, usize>>::new();
    for row in rows {
        let product = derive_underlying_symbol(&row.symbol)?;
        *counts
            .entry(product)
            .or_default()
            .entry(row_fee_kind_signature(row))
            .or_default() += 1;
    }
    Ok(counts)
}

fn current_product_fee_kind_counts(
    conn: &Connection,
) -> Result<BTreeMap<String, BTreeMap<FeeKindSignature, usize>>> {
    let mut statement = conn.prepare(
        "select c.symbol, v.open_fee_json, v.close_yesterday_fee_json,
                v.close_today_fee_json
         from contracts c
         join fee_versions v on v.contract_id = c.id
         where v.valid_to is null",
    )?;
    let mut rows = statement.query([])?;
    let mut counts = BTreeMap::<String, BTreeMap<FeeKindSignature, usize>>::new();
    while let Some(record) = rows.next()? {
        let symbol: String = record.get(0)?;
        let fees = [
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(1)?)?,
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(2)?)?,
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(3)?)?,
        ];
        let product = derive_underlying_symbol(&symbol)?;
        *counts
            .entry(product)
            .or_default()
            .entry(fee_kind_signature(&fees))
            .or_default() += 1;
    }
    Ok(counts)
}

fn is_uniform_tenth_fixed(fees: &[FeeSpec; 3]) -> bool {
    let non_zero = fees
        .iter()
        .filter(|fee| !is_zero_fee(fee))
        .collect::<Vec<_>>();
    !non_zero.is_empty()
        && non_zero.iter().all(|fee| {
            fee.kind == future_meta::model::FeeKind::CnyPerLot
                && fee
                    .value
                    .is_some_and(|value| (value - 0.1).abs() < f64::EPSILON)
        })
}

fn is_isolated_tenth_placeholder(
    row: &AllowedRow,
    candidate_counts: &BTreeMap<String, BTreeMap<FeeKindSignature, usize>>,
    current_counts: &BTreeMap<String, BTreeMap<FeeKindSignature, usize>>,
) -> Result<bool> {
    let fees = [
        row.open_fee.clone(),
        row.close_yesterday_fee.clone(),
        row.close_today_fee.clone(),
    ];
    if !is_uniform_tenth_fixed(&fees) {
        return Ok(false);
    }
    let product = derive_underlying_symbol(&row.symbol)?;
    let signature = fee_kind_signature(&fees);
    let candidate_product = candidate_counts.get(&product);
    let candidate = candidate_product
        .and_then(|counts| counts.get(&signature))
        .copied()
        .unwrap_or_default();
    let candidate_total = candidate_product
        .map(|counts| counts.values().sum::<usize>())
        .unwrap_or_default();
    let batch_has_competing_type = candidate_total >= 2
        && candidate < candidate_total
        && candidate_product.is_some_and(|counts| {
            counts
                .iter()
                .any(|(kind, count)| *kind != signature && *count >= 2)
        });

    let Some(current_product) = current_counts.get(&product) else {
        return Ok(batch_has_competing_type);
    };
    let current_total = current_product.values().sum::<usize>();
    let Some((dominant_signature, dominant_count)) =
        current_product.iter().max_by_key(|(_, count)| *count)
    else {
        return Ok(batch_has_competing_type);
    };
    let current_has_competing_type = *dominant_signature != signature
        && current_total >= 5
        && *dominant_count * 5 >= current_total * 4;
    Ok(batch_has_competing_type || current_has_competing_type)
}

fn repair_isolated_tenth_incumbent(
    conn: &Connection,
    row: &AllowedRow,
    current_counts: &BTreeMap<String, BTreeMap<FeeKindSignature, usize>>,
    observed_at: &str,
) -> Result<bool> {
    let Some(current) = current_fee_rule(conn, &row.symbol)? else {
        return Ok(false);
    };
    if !is_uniform_tenth_fixed(&current.fees) {
        return Ok(false);
    }

    let product = derive_underlying_symbol(&row.symbol)?;
    let Some(product_counts) = current_counts.get(&product) else {
        return Ok(false);
    };
    let current_total = product_counts.values().sum::<usize>();
    let Some((dominant_signature, dominant_count)) =
        product_counts.iter().max_by_key(|(_, count)| *count)
    else {
        return Ok(false);
    };
    if current_total < 5
        || *dominant_count * 5 < current_total * 4
        || *dominant_signature == fee_kind_signature(&current.fees)
        || fee_kind_signature(&[
            row.open_fee.clone(),
            row.close_yesterday_fee.clone(),
            row.close_today_fee.clone(),
        ]) != *dominant_signature
    {
        return Ok(false);
    }

    let open_json = serde_json::to_string(&row.open_fee)?;
    let close_yesterday_json = serde_json::to_string(&row.close_yesterday_fee)?;
    let close_today_json = serde_json::to_string(&row.close_today_fee)?;
    let rule_hash = row_rule_hash(row);
    conn.execute(
        "update fee_versions
         set rule_hash = ?1, open_fee_json = ?2,
             close_yesterday_fee_json = ?3, close_today_fee_json = ?4,
             source_updated_at = ?5, last_seen_at = ?6
         where contract_id = (select id from contracts where symbol = ?7)
           and valid_to is null",
        params![
            rule_hash,
            open_json,
            close_yesterday_json,
            close_today_json,
            row.source_updated_at,
            observed_at,
            row.symbol,
        ],
    )?;
    Ok(true)
}

#[derive(Debug)]
struct CurrentFeeRule {
    valid_from_at: OffsetDateTime,
    fees: [FeeSpec; 3],
    source_updated_at: Option<String>,
}

fn current_fee_rule(conn: &Connection, symbol: &str) -> Result<Option<CurrentFeeRule>> {
    let raw = conn
        .query_row(
        "select v.valid_from, v.open_fee_json, v.close_yesterday_fee_json, v.close_today_fee_json, v.source_updated_at
           from fee_versions v
           join contracts c on c.id = v.contract_id
          where c.symbol = ?1
          order by v.valid_from desc, v.id desc
          limit 1",
        params![symbol],
        |record| {
            Ok((
                record.get::<_, String>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, String>(2)?,
                record.get::<_, String>(3)?,
                record.get::<_, Option<String>>(4)?,
            ))
        },
    )
    .optional()?;
    let Some((valid_from, open_fee, close_yesterday_fee, close_today_fee, source_updated_at)) = raw
    else {
        return Ok(None);
    };
    Ok(Some(CurrentFeeRule {
        valid_from_at: parse_timestamp("current valid_from", &valid_from)?,
        fees: [
            parse_fee_json(&open_fee)?,
            parse_fee_json(&close_yesterday_fee)?,
            parse_fee_json(&close_today_fee)?,
        ],
        source_updated_at,
    }))
}

/// Protect the live database from known bad 9qihuo snapshots.
///
/// The total-page source is a current cross-section, not an authoritative
/// historical ledger. A stale row must never rewrite an already observed
/// effective day. Known `0.1元` placeholders and recurring uniform
/// `+0.09`/`+0.1` fixed-fee collection offsets are handled here.
fn sanitize_latest_row(
    conn: &Connection,
    row: &AllowedRow,
    observed_at: &str,
) -> Result<Option<AllowedRow>> {
    let Some(current) = current_fee_rule(conn, &row.symbol)? else {
        return Ok(Some(row.clone()));
    };
    let (_, candidate_valid_from_at) = row_valid_from(row, observed_at)?;
    let candidate_fees = [
        row.open_fee.clone(),
        row.close_yesterday_fee.clone(),
        row.close_today_fee.clone(),
    ];
    if candidate_fees
        .iter()
        .any(|fee| fee.kind == future_meta::model::FeeKind::Unknown)
    {
        return Ok(None);
    }
    if candidate_valid_from_at <= current.valid_from_at
        && !same_fee_rules(&candidate_fees, &current.fees)
    {
        return Ok(None);
    }
    if let (Some(candidate_at), Some(existing_at)) = (
        row.source_updated_at.as_deref(),
        current.source_updated_at.as_deref(),
    ) && parse_source_updated_at(candidate_at)? <= parse_source_updated_at(existing_at)?
        && !same_fee_rules(&candidate_fees, &current.fees)
    {
        return Ok(None);
    }
    if is_known_tenth_placeholder(&candidate_fees, &current.fees) {
        return Ok(None);
    }

    let mut sanitized = row.clone();
    if has_known_fixed_offset(&candidate_fees, &current.fees) {
        sanitized.open_fee = current.fees[0].clone();
        sanitized.close_yesterday_fee = current.fees[1].clone();
        sanitized.close_today_fee = current.fees[2].clone();
    }
    Ok(Some(sanitized))
}

fn same_fee_rules(left: &[FeeSpec; 3], right: &[FeeSpec; 3]) -> bool {
    left.iter().zip(right).all(|(left, right)| {
        left.kind == right.kind && left.value.map(f64::to_bits) == right.value.map(f64::to_bits)
    })
}

fn is_zero_fee(fee: &FeeSpec) -> bool {
    fee.kind == future_meta::model::FeeKind::Zero || fee.value == Some(0.0)
}

fn is_known_tenth_placeholder(candidate: &[FeeSpec; 3], incumbent: &[FeeSpec; 3]) -> bool {
    let non_zero = candidate
        .iter()
        .filter(|fee| !is_zero_fee(fee))
        .collect::<Vec<_>>();
    !non_zero.is_empty()
        && non_zero.iter().all(|fee| {
            fee.kind == future_meta::model::FeeKind::CnyPerLot
                && fee
                    .value
                    .is_some_and(|value| (value - 0.1).abs() < f64::EPSILON)
        })
        && incumbent.iter().any(|fee| {
            !is_zero_fee(fee)
                && (fee.kind != future_meta::model::FeeKind::CnyPerLot
                    || fee
                        .value
                        .is_none_or(|value| (value - 0.1).abs() > f64::EPSILON))
        })
}

fn has_known_fixed_offset(candidate: &[FeeSpec; 3], incumbent: &[FeeSpec; 3]) -> bool {
    const OFFSET_TOLERANCE: f64 = 1e-8;
    let mut found_offset = false;
    for (candidate, incumbent) in candidate.iter().zip(incumbent) {
        if is_zero_fee(candidate) && is_zero_fee(incumbent) {
            continue;
        }
        let (Some(candidate_value), Some(incumbent_value)) = (candidate.value, incumbent.value)
        else {
            return false;
        };
        if candidate.kind != future_meta::model::FeeKind::CnyPerLot
            || incumbent.kind != future_meta::model::FeeKind::CnyPerLot
        {
            return false;
        }
        let delta = candidate_value - incumbent_value;
        if [0.01, 0.09, 0.1]
            .iter()
            .all(|offset| (delta - offset).abs() > OFFSET_TOLERANCE)
        {
            return false;
        }
        found_offset = true;
    }
    found_offset
}

fn latest_conflicts_with_csv_rule(
    conn: &Connection,
    row: &AllowedRow,
    observed_at: &str,
) -> Result<bool> {
    let Some(source_updated_at) = row.source_updated_at.as_deref() else {
        return Ok(false);
    };
    let candidate_at = parse_source_updated_at(source_updated_at)?;
    let (valid_from, _) = row_valid_from(row, observed_at)?;
    let candidate_hash = row_rule_hash(row);
    let existing = conn
        .query_row(
            "select fee_versions.rule_hash, fee_versions.source_updated_at
             from fee_versions
             join contracts on contracts.id = fee_versions.contract_id
             where contracts.symbol = ?1 and fee_versions.valid_from = ?2",
            params![row.symbol, valid_from],
            |record| {
                Ok((
                    record.get::<_, String>(0)?,
                    record.get::<_, Option<String>>(1)?,
                ))
            },
        )
        .optional()?;

    let Some((existing_hash, Some(existing_source_updated_at))) = existing else {
        return Ok(false);
    };
    let existing_at = parse_source_updated_at(&existing_source_updated_at)?;

    Ok(
        existing_at.unix_timestamp() == candidate_at.unix_timestamp()
            && existing_hash != candidate_hash,
    )
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

/// Load consistent product-level lot and tick metadata from seeded contracts.
///
/// # Errors
///
/// Returns an error when a persisted contract symbol is invalid or contracts
/// of one product disagree on its lot size or tick size.
pub fn product_static_metadata(
    conn: &Connection,
) -> Result<BTreeMap<String, ContractStaticMetadata>> {
    let candidates = product_static_metadata_candidates(conn)?;
    let mut metadata = BTreeMap::new();
    for (product, candidates) in candidates {
        let [candidate] = candidates.as_slice() else {
            return Err(anyhow!(
                "inconsistent static metadata for {product}: {} candidates",
                candidates.len()
            ));
        };
        metadata.insert(product, *candidate);
    }
    Ok(metadata)
}

/// Load all distinct product-level lot and tick candidates from seeded contracts.
///
/// A product may have multiple candidates when an exchange changes its tick
/// size. Consumers must use an independent source field to choose one.
///
/// # Errors
///
/// Returns an error when a persisted contract symbol or static numeric value
/// is invalid.
pub fn product_static_metadata_candidates(
    conn: &Connection,
) -> Result<BTreeMap<String, Vec<ContractStaticMetadata>>> {
    ensure_schema(conn)?;
    let mut statement = conn.prepare(
        "select symbol, lot_size, tick_size
         from contracts
         order by symbol",
    )?;
    let mut rows = statement.query([])?;
    let mut metadata = BTreeMap::<String, Vec<ContractStaticMetadata>>::new();

    while let Some(row) = rows.next()? {
        let symbol: String = row.get(0)?;
        let candidate = ContractStaticMetadata {
            lot_size: row.get(1)?,
            tick_size: row.get(2)?,
        };
        if !candidate.lot_size.is_finite() || candidate.lot_size <= 0.0 {
            return Err(anyhow!(
                "invalid persisted lot_size for {symbol}: {}",
                candidate.lot_size
            ));
        }
        if !candidate.tick_size.is_finite() || candidate.tick_size <= 0.0 {
            return Err(anyhow!(
                "invalid persisted tick_size for {symbol}: {}",
                candidate.tick_size
            ));
        }
        let product = derive_underlying_symbol(&symbol)?;
        let candidates = metadata.entry(product).or_default();
        if !candidates.contains(&candidate) {
            candidates.push(candidate);
        }
    }

    add_official_historical_static_candidates(&mut metadata);
    for candidates in metadata.values_mut() {
        candidates.sort_by(|left, right| {
            left.lot_size
                .total_cmp(&right.lot_size)
                .then_with(|| left.tick_size.total_cmp(&right.tick_size))
        });
    }

    Ok(metadata)
}

fn add_official_historical_static_candidates(
    metadata: &mut BTreeMap<String, Vec<ContractStaticMetadata>>,
) {
    // CZCE Notice [2022] No. 74 made the WH contract specification effective
    // on 2022-12-01: 20 tonnes/lot and 1 yuan/tonne. WH is delisted from the
    // current 9qihuo seed but still occurs in verified Jin10 2025 snapshots.
    let strong_wheat = ContractStaticMetadata {
        lot_size: 20.0,
        tick_size: 1.0,
    };
    let candidates = metadata.entry("CZCE.WH".to_owned()).or_default();
    if !candidates.contains(&strong_wheat) {
        candidates.push(strong_wheat);
    }

    // DCE Notice [2026] No. 32 changed p and y from 2 yuan/tonne to 1 on
    // 2026-04-10. GFEX Notice [2024] No. 337 changed lc from 50 yuan/tonne
    // to 20 on 2024-12-17. Current 9qihuo rows contain newer ticks while the
    // Jin10 history starts before both effective dates.
    for (product, current, historical) in [
        (
            "DCE.p",
            ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ),
        (
            "DCE.y",
            ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ),
        (
            "GFEX.lc",
            ContractStaticMetadata {
                lot_size: 1.0,
                tick_size: 20.0,
            },
            ContractStaticMetadata {
                lot_size: 1.0,
                tick_size: 50.0,
            },
        ),
    ] {
        let Some(candidates) = metadata.get_mut(product) else {
            continue;
        };
        if candidates.contains(&current) && !candidates.contains(&historical) {
            candidates.push(historical);
        }
    }
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

/// Record a verified Jin10 source snapshot after its rows are backfilled.
///
/// # Errors
///
/// Returns an error when the effective timestamp is invalid or the audit row
/// cannot be written.
pub fn record_jin10_source_snapshot(
    conn: &Connection,
    effective_at: &str,
    completed_rows: usize,
    skipped_invalid_symbols: usize,
) -> Result<()> {
    ensure_schema(conn)?;
    parse_timestamp("effective_at", effective_at)?;
    if completed_rows == 0 {
        return Err(anyhow!("Jin10 snapshot has no completed rows"));
    }
    conn.execute(
        "insert into jin10_source_snapshots(
           effective_at, completed_rows, skipped_invalid_symbols, recorded_at
         ) values (?1, ?2, ?3, ?1)
         on conflict(effective_at) do update set
           completed_rows = excluded.completed_rows,
           skipped_invalid_symbols = excluded.skipped_invalid_symbols,
           recorded_at = excluded.recorded_at",
        params![effective_at, completed_rows, skipped_invalid_symbols],
    )?;
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
                v.trading_status, v.is_main_contract, v.source_kind, v.source_updated_at,
                v.valid_from, v.first_seen_at, v.last_seen_at
         from fee_versions v
         join contracts c on c.id = v.contract_id
         where v.contract_id = ?1
         order by v.valid_from, v.id",
    )?;

    let mut rows = stmt.query(params![contract_id])?;
    let mut versions = Vec::new();
    while let Some(record) = rows.next()? {
        let valid_from: String = record.get(15)?;
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
                source_updated_at: record.get(14)?,
                is_main_contract: record.get::<_, i64>(12)? != 0,
            },
            rule_hash: record.get(5)?,
            valid_from_at: parse_timestamp("valid_from", &valid_from)?,
            valid_from,
            first_seen_at: record.get(16)?,
            last_seen_at: record.get(17)?,
            source_kind: parse_fee_source(&record.get::<_, String>(13)?)?,
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

/// Reconcile a later 9qihuo observation with a historical snapshot's
/// effective-day boundary.
///
/// When a verified historical snapshot supplies the beginning-of-day rule,
/// move a conflicting 9qihuo row to its exact later source update time instead
/// of treating either source as valid before it was observed.
fn reconcile_historical_source_conflicts(
    versions: &mut [VersionRecord],
) -> Result<Vec<FeeRuleConflict>> {
    versions.sort_by_key(|version| version.valid_from_at);

    let mut conflicts = Vec::new();
    let mut start = 0usize;
    while start < versions.len() {
        let valid_from_at = versions[start].valid_from_at;
        let mut end = start + 1;
        while end < versions.len() && versions[end].valid_from_at == valid_from_at {
            end += 1;
        }

        if end - start > 1 {
            let contenders = versions[start..end]
                .iter()
                .filter(|version| version.source_kind == FeeSource::Jin10)
                .cloned()
                .collect::<Vec<_>>();
            for version in &mut versions[start..end] {
                if version.source_kind != FeeSource::NineQihuo {
                    continue;
                }
                let Some(source_updated_at) = version.row.source_updated_at.as_deref() else {
                    continue;
                };
                let source_at = parse_source_updated_at(source_updated_at)?;
                if source_at <= version.valid_from_at {
                    continue;
                }
                let conflicting_contenders = contenders
                    .iter()
                    .filter(|contender| contender.rule_hash != version.rule_hash)
                    .cloned()
                    .collect::<Vec<_>>();
                if !conflicting_contenders.is_empty() {
                    let effective_at = version.valid_from.clone();
                    for contender in conflicting_contenders {
                        conflicts.push(FeeRuleConflict {
                            incumbent: version.clone(),
                            contender,
                            effective_at: effective_at.clone(),
                        });
                    }
                    version.valid_from_at = source_at;
                    version.valid_from = source_at.format(&Rfc3339)?;
                }
            }
        }

        start = end;
    }

    Ok(conflicts)
}

#[derive(Debug, Clone)]
struct FeeRuleConflict {
    incumbent: VersionRecord,
    contender: VersionRecord,
    effective_at: String,
}

fn record_fee_rule_conflicts(
    tx: &Transaction<'_>,
    contract_id: i64,
    conflicts: &[FeeRuleConflict],
    recorded_at: &str,
) -> Result<()> {
    for conflict in conflicts {
        tx.execute(
            "insert into fee_rule_conflicts(
               contract_id, effective_at, incumbent_source, contender_source,
               selected_source, incumbent_rule_hash, contender_rule_hash,
               incumbent_rule_json, contender_rule_json, reason, recorded_at
             ) values (?1, ?2, ?3, ?4, 'jin10', ?5, ?6, ?7, ?8,
                       'historical_snapshot_precedes_9q_observation', ?9)
             on conflict(contract_id, effective_at, incumbent_rule_hash, contender_rule_hash)
             do nothing",
            params![
                contract_id,
                conflict.effective_at,
                conflict.incumbent.source_kind.as_str(),
                conflict.contender.source_kind.as_str(),
                conflict.incumbent.rule_hash,
                conflict.contender.rule_hash,
                serde_json::to_string(&conflict.incumbent.row)?,
                serde_json::to_string(&conflict.contender.row)?,
                recorded_at,
            ],
        )?;
    }
    Ok(())
}

/// Coalesce same-instant rows that differ only in state metadata.
///
/// Historical seed revisions occasionally describe one source second twice:
/// first as not trading, then as the main trading contract. Equal timestamps
/// have no reliable sequence, so prefer the tradeable/main record rather than
/// inventing an intrasecond interval.
fn collapse_same_source_status_variants(
    mut versions: Vec<VersionRecord>,
) -> Result<Vec<VersionRecord>> {
    versions.sort_by_key(|version| version.valid_from_at);

    let mut collapsed = Vec::<VersionRecord>::new();
    for version in versions {
        let Some(last) = collapsed.last_mut() else {
            collapsed.push(version);
            continue;
        };
        if last.valid_from_at != version.valid_from_at {
            collapsed.push(version);
            continue;
        }

        let same_source_update = same_source_update(&last.row, &version.row)?;
        let state_differs = version_state_rank(&last.row) != version_state_rank(&version.row);
        if !(same_fee_terms(&last.row, &version.row) || same_source_update && state_differs) {
            collapsed.push(version);
            continue;
        }

        if version_state_rank(&version.row) > version_state_rank(&last.row) {
            let previous = std::mem::replace(last, version);
            merge_equivalent_version(last, previous)?;
        } else {
            merge_equivalent_version(last, version)?;
        }
    }

    Ok(collapsed)
}

fn same_source_update(left: &AllowedRow, right: &AllowedRow) -> Result<bool> {
    match (
        left.source_updated_at.as_deref(),
        right.source_updated_at.as_deref(),
    ) {
        (Some(left), Some(right)) => {
            Ok(parse_source_updated_at(left)? == parse_source_updated_at(right)?)
        }
        _ => Ok(false),
    }
}

fn same_fee_terms(left: &AllowedRow, right: &AllowedRow) -> bool {
    left.symbol == right.symbol
        && left.listing_date == right.listing_date
        && left.expiry_date == right.expiry_date
        && left.buy_margin_rate == right.buy_margin_rate
        && left.sell_margin_rate == right.sell_margin_rate
        && left.open_fee == right.open_fee
        && left.close_yesterday_fee == right.close_yesterday_fee
        && left.close_today_fee == right.close_today_fee
        && left.lot_size.to_bits() == right.lot_size.to_bits()
        && left.tick_size.to_bits() == right.tick_size.to_bits()
}

fn version_state_rank(row: &AllowedRow) -> (u8, bool) {
    let trading_rank = match row.trading_status {
        TradingStatus::Trading => 2,
        TradingStatus::Unknown => 1,
        TradingStatus::NotTrading => 0,
    };
    (trading_rank, row.is_main_contract)
}

fn merge_equivalent_version(left: &mut VersionRecord, right: VersionRecord) -> Result<()> {
    if left.source_kind == FeeSource::Jin10 && right.source_kind == FeeSource::NineQihuo {
        left.source_kind = FeeSource::NineQihuo;
    }
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
    source_kind: FeeSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FeeSource {
    NineQihuo,
    Jin10,
    V11Baseline,
}

impl FeeSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NineQihuo => "9qihuo",
            Self::Jin10 => "jin10",
            Self::V11Baseline => "v11_baseline",
        }
    }
}

fn parse_fee_source(value: &str) -> Result<FeeSource> {
    match value {
        "9qihuo" => Ok(FeeSource::NineQihuo),
        "jin10" => Ok(FeeSource::Jin10),
        "v11_baseline" => Ok(FeeSource::V11Baseline),
        other => Err(anyhow!("unknown fee source: {other}")),
    }
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
    let source_or_observed_at = row
        .source_updated_at
        .as_deref()
        .map(parse_source_updated_at)
        .transpose()?
        .unwrap_or_else(|| {
            parse_timestamp("observed_at", observed_at)
                .expect("observed_at should have been validated before row preparation")
        });
    let source_day_start = exchange_day_start(source_or_observed_at);
    let valid_from_at = row
        .listing_date
        .as_deref()
        .map(contract_listing_day_start)
        .transpose()?
        .map_or(source_day_start, |listing_day_start| {
            source_day_start.max(listing_day_start)
        });
    Ok((valid_from_at.format(&Rfc3339)?, valid_from_at))
}

fn contract_listing_day_start(value: &str) -> Result<OffsetDateTime> {
    let value = value.trim();
    let rfc3339 = if value.len() == 8 && value.bytes().all(|byte| byte.is_ascii_digit()) {
        format!(
            "{}-{}-{}T00:00:00+08:00",
            &value[..4],
            &value[4..6],
            &value[6..]
        )
    } else {
        format!("{value}T00:00:00+08:00")
    };
    Ok(exchange_day_start(parse_timestamp(
        "listing_date",
        &rfc3339,
    )?))
}

fn repair_fee_versions_before_listing(conn: &Connection) -> Result<()> {
    let candidates = {
        let mut statement = conn.prepare(
            "select fv.id, fv.contract_id, fv.valid_from, fv.valid_to, c.listing_date
             from fee_versions fv
             join contracts c on c.id = fv.contract_id
             where c.listing_date is not null",
        )?;
        statement
            .query_map([], |record| {
                Ok((
                    record.get::<_, i64>(0)?,
                    record.get::<_, i64>(1)?,
                    record.get::<_, String>(2)?,
                    record.get::<_, Option<String>>(3)?,
                    record.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut repairs = Vec::new();
    for (version_id, contract_id, valid_from, valid_to, listing_date) in candidates {
        let valid_from_at = parse_timestamp("valid_from", &valid_from)?;
        let listing_day_start = contract_listing_day_start(&listing_date)?;
        if valid_from_at >= listing_day_start {
            continue;
        }

        let is_initial: bool = conn.query_row(
            "select not exists(
                select 1 from fee_versions
                where contract_id = ?1 and valid_from < ?2
            )",
            params![contract_id, valid_from],
            |record| record.get(0),
        )?;
        if !is_initial {
            return Err(anyhow!(
                "cannot safely repair fee version {version_id}: it is not the initial contract version"
            ));
        }

        if let Some(valid_to) = valid_to {
            let valid_to_at = parse_timestamp("valid_to", &valid_to)?;
            if listing_day_start >= valid_to_at {
                return Err(anyhow!(
                    "cannot safely repair fee version {version_id}: contract listing starts after its validity"
                ));
            }
        }

        let repaired_valid_from = listing_day_start.format(&Rfc3339)?;
        let conflicts_with_existing: bool = conn.query_row(
            "select exists(
                select 1 from fee_versions
                where contract_id = ?1 and id <> ?2 and valid_from = ?3
            )",
            params![contract_id, version_id, repaired_valid_from],
            |record| record.get(0),
        )?;
        if conflicts_with_existing {
            return Err(anyhow!(
                "cannot safely repair fee version {version_id}: listing day already has a version"
            ));
        }

        repairs.push((version_id, repaired_valid_from));
    }

    if repairs.is_empty() {
        return Ok(());
    }

    conn.execute_batch("begin immediate")?;
    for (version_id, valid_from) in repairs {
        if let Err(err) = conn.execute(
            "update fee_versions set valid_from = ?1 where id = ?2",
            params![valid_from, version_id],
        ) {
            conn.execute_batch("rollback")?;
            return Err(err.into());
        }
    }
    conn.execute_batch("commit")?;
    Ok(())
}

fn exchange_day_start(at: OffsetDateTime) -> OffsetDateTime {
    at.to_offset(exchange_offset())
        .date()
        .midnight()
        .assume_offset(exchange_offset())
}

fn exchange_offset() -> UtcOffset {
    UtcOffset::from_hms(8, 0, 0).expect("valid exchange offset")
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

fn upsert_contract(
    tx: &Transaction<'_>,
    row: &AllowedRow,
    observed_at: &str,
    mode: IngestMode,
) -> Result<i64> {
    tx.execute(
        "insert into contracts(
         symbol, listing_date, expiry_date, lot_size, tick_size,
          first_seen_at, last_seen_at, active
         )
         values (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7)
         on conflict(symbol) do update set
           listing_date = coalesce(excluded.listing_date, contracts.listing_date),
           expiry_date = coalesce(excluded.expiry_date, contracts.expiry_date),
           lot_size = excluded.lot_size,
           tick_size = excluded.tick_size,
           first_seen_at = case
             when julianday(excluded.first_seen_at) < julianday(contracts.first_seen_at)
             then excluded.first_seen_at else contracts.first_seen_at end,
           last_seen_at = case
             when julianday(excluded.last_seen_at) > julianday(contracts.last_seen_at)
             then excluded.last_seen_at else contracts.last_seen_at end,
           active = max(contracts.active, excluded.active)",
        params![
            row.symbol.as_str(),
            row.listing_date.as_deref(),
            row.expiry_date.as_deref(),
            row.lot_size,
            row.tick_size,
            observed_at,
            bool_to_i64(mode == IngestMode::Live),
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
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         )
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
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
            version.source_kind.as_str(),
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
