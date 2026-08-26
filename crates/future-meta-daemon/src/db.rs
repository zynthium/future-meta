//! `SQLite` schema and version maintenance.

use crate::announcement::{AnnouncementCandidate, AnnouncementDocument, AnnouncementSource};
use crate::hash::row_rule_hash;
use crate::jin10::ContractStaticMetadata;
use crate::latest::LatestRow;
use crate::parse::AllowedRow;
use anyhow::{Result, anyhow};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

/// A normal incremental source refresh should only contain a small number of
/// independently corroborated fee changes. Larger exchange-wide adjustments
/// must be imported from staged official evidence instead of two display
/// sources that can share the same upstream defect.
const MAX_AUTOMATIC_FEE_CHANGES_PER_SNAPSHOT: usize = 12;
const MAX_ANNOUNCEMENT_SCAN_AGE: Duration = Duration::hours(1);
const MAX_UNRESOLVED_CANDIDATE_AGE: Duration = Duration::hours(24);

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
    pub missing_metadata_symbols: Vec<String>,
}

/// Refuse a publishing update when any parsed latest row lacks contract metadata.
///
/// Read-only diagnostics may still inspect a partial [`LatestCompletion`], but a
/// publisher must never silently omit newly listed contracts.
///
/// # Errors
///
/// Returns an error containing the number of omitted rows.
pub fn require_complete_latest_metadata(completion: &LatestCompletion) -> Result<()> {
    if completion.skipped_missing_metadata > 0 {
        return Err(anyhow!(
            "latest snapshot skipped {} contract(s) with missing metadata; refusing to publish",
            completion.skipped_missing_metadata
        ));
    }
    Ok(())
}

/// Fill metadata for newly listed contracts only when 9qihuo's product CSV,
/// the latest table's tick value, and Jin10 independently agree.
///
/// # Errors
///
/// Returns an error when any row needing metadata lacks either corroborating
/// source, has inconsistent source observations, or fails the tick-value
/// identity `lot_size * tick_size`.
pub fn corroborate_new_contract_metadata(
    latest_rows: &[LatestRow],
    csv_rows: &[AllowedRow],
    jin10_rows: &[AllowedRow],
) -> Result<Vec<LatestRow>> {
    let mut enriched = Vec::with_capacity(latest_rows.len());
    for latest in latest_rows {
        if latest.lot_size.is_some() && latest.tick_size.is_some() {
            enriched.push(latest.clone());
            continue;
        }

        let csv = unique_static_metadata(csv_rows, &latest.symbol, "9qihuo product CSV")?;
        let jin10 = if jin10_rows.iter().any(|row| row.symbol == latest.symbol) {
            unique_static_metadata(jin10_rows, &latest.symbol, "Jin10")?
        } else {
            product_level_jin10_evidence(latest, csv, jin10_rows)?
        };
        if !same_number(csv.lot_size, jin10.lot_size)
            || !same_number(csv.tick_size, jin10.tick_size)
        {
            return Err(anyhow!(
                "new contract metadata disagreement for {}: 9qihuo={}x{}, Jin10={}x{}",
                latest.symbol,
                csv.lot_size,
                csv.tick_size,
                jin10.lot_size,
                jin10.tick_size
            ));
        }
        let tick_value = latest.tick_value.ok_or_else(|| {
            anyhow!(
                "new contract {} has no latest-table tick value; refusing metadata admission",
                latest.symbol
            )
        })?;
        if !same_number(csv.lot_size * csv.tick_size, tick_value) {
            return Err(anyhow!(
                "new contract tick value mismatch for {}: metadata={}x{}, latest={}",
                latest.symbol,
                csv.lot_size,
                csv.tick_size,
                tick_value
            ));
        }

        let mut row = latest.clone();
        row.listing_date.clone_from(&csv.listing_date);
        row.expiry_date.clone_from(&csv.expiry_date);
        row.lot_size = Some(csv.lot_size);
        row.tick_size = Some(csv.tick_size);
        enriched.push(row);
    }
    Ok(enriched)
}

fn unique_static_metadata<'a>(
    rows: &'a [AllowedRow],
    symbol: &str,
    source: &str,
) -> Result<&'a AllowedRow> {
    let mut matches = rows.iter().filter(|row| row.symbol == symbol);
    let first = matches
        .next()
        .ok_or_else(|| anyhow!("new contract {symbol} missing from {source}"))?;
    for other in matches {
        if !same_number(first.lot_size, other.lot_size)
            || !same_number(first.tick_size, other.tick_size)
        {
            return Err(anyhow!(
                "new contract {symbol} has inconsistent metadata within {source}"
            ));
        }
    }
    Ok(first)
}

fn same_number(left: f64, right: f64) -> bool {
    (left - right).abs() <= left.abs().max(right.abs()).max(1.0) * 1e-9
}

fn product_level_jin10_evidence<'a>(
    latest: &LatestRow,
    csv: &AllowedRow,
    jin10_rows: &'a [AllowedRow],
) -> Result<&'a AllowedRow> {
    let product = derive_underlying_symbol(&latest.symbol)?;
    for row in jin10_rows {
        if derive_underlying_symbol(&row.symbol)? == product
            && same_number(row.lot_size, csv.lot_size)
            && same_number(row.tick_size, csv.tick_size)
        {
            return Ok(row);
        }
    }
    Err(anyhow!(
        "new contract {} has no matching Jin10 product-level fallback",
        latest.symbol
    ))
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

/// A 9qihuo latest-table fee candidate rejected for missing corroboration or a
/// safety-sensitive transition. Rejections are deliberately kept out of
/// `fee_versions` and high-risk candidates must be staged with exchange
/// original evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestCandidateRejection {
    pub symbol: String,
    pub reason: String,
}

/// Result of applying the two-source admission gate to latest fee candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct LatestCandidateVerification {
    pub accepted: Vec<AllowedRow>,
    pub new_contracts: Vec<AllowedRow>,
    pub degraded_new_contracts: Vec<String>,
    pub unchanged: usize,
    pub rejected: Vec<LatestCandidateRejection>,
}

/// Read-only audit detail for a latest-table candidate rejected by the
/// two-source admission gate.  This intentionally captures each source's
/// exact fee tuple so an operator can distinguish a missing observation from
/// a conflicting observation without querying mutable history tables.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct LatestCandidateDiagnostic {
    pub symbol: String,
    pub source_updated_at: Option<String>,
    pub production: [FeeSpec; 3],
    pub qihuo: [FeeSpec; 3],
    pub jin10: Option<[FeeSpec; 3]>,
    pub jin10_source_updated_at: Option<String>,
    pub rejection_reason: String,
}

/// Discovery state required before a third-party latest-table refresh may run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncementHealth {
    /// Broker sources with a current successful scan that has not subsequently failed.
    pub fresh_sources: Vec<String>,
    /// Unresolved fee-change candidates that have not yet reached the blocking age.
    pub pending_candidates: usize,
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
#[allow(clippy::too_many_lines)]
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
    let product_candidate_counts = product_fee_kind_counts(candidates)?;
    let current_product_counts = current_product_fee_kind_counts(conn)?;

    let mut accepted = Vec::new();
    let mut new_contracts = Vec::new();
    let mut degraded_new_contracts = Vec::new();
    let mut unchanged = 0usize;
    let mut rejected = Vec::new();
    for candidate in candidates {
        let candidate_fees = fee_tuple(candidate);
        let Some(current) = current_fee_rule(conn, &candidate.symbol)? else {
            if let Some(inherited) = inherited_product_fee_rule(conn, candidate)?
                && !same_fee_rules(&candidate_fees, &inherited)
            {
                rejected.push(LatestCandidateRejection {
                    symbol: candidate.symbol.clone(),
                    reason: "new contract does not inherit the existing product fee rule"
                        .to_owned(),
                });
                continue;
            }
            let jin10 = source_day(candidate).and_then(|day| {
                jin10_by_key
                    .get(&(candidate.symbol.clone(), day.to_owned()))
                    .copied()
            });
            if jin10.is_some_and(|row| same_fee_rules(&candidate_fees, &fee_tuple(row))) {
                new_contracts.push(candidate.clone());
            } else if jin10.is_none()
                && has_same_day_product_level_jin10_match(conn, candidate, jin10_rows)?
            {
                new_contracts.push(candidate.clone());
                degraded_new_contracts.push(candidate.symbol.clone());
            } else {
                rejected.push(LatestCandidateRejection {
                    symbol: candidate.symbol.clone(),
                    reason: "new contract lacks same-day matching Jin10 fee tuple".to_owned(),
                });
            }
            continue;
        };
        if same_fee_rules(&current.fees, &candidate_fees) {
            unchanged += 1;
            continue;
        }

        if is_isolated_tenth_placeholder(
            candidate,
            &product_candidate_counts,
            &current_product_counts,
        )? {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: "isolated 0.1 CNY candidate requires official evidence".to_owned(),
            });
            continue;
        }

        if let Some(reason) = live_candidate_safety_rejection(&current.fees, &candidate_fees) {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol.clone(),
                reason: reason.to_owned(),
            });
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

    if accepted.len() > MAX_AUTOMATIC_FEE_CHANGES_PER_SNAPSHOT {
        for candidate in accepted.drain(..) {
            rejected.push(LatestCandidateRejection {
                symbol: candidate.symbol,
                reason: "large fee-change batch requires staged official evidence".to_owned(),
            });
        }
    }

    Ok(LatestCandidateVerification {
        accepted,
        new_contracts,
        degraded_new_contracts,
        unchanged,
        rejected,
    })
}

/// Return source-by-source evidence for every latest candidate rejected by
/// [`cross_verify_latest_candidates`].  This performs no writes.
///
/// # Errors
///
/// Returns an error when the current fee state cannot be read or candidate
/// verification encounters malformed source timestamps or fee values.
pub fn diagnose_rejected_latest_candidates(
    conn: &Connection,
    candidates: &[AllowedRow],
    jin10_rows: &[AllowedRow],
) -> Result<Vec<LatestCandidateDiagnostic>> {
    let verification = cross_verify_latest_candidates(conn, candidates, jin10_rows)?;
    let rejection_reasons = verification
        .rejected
        .into_iter()
        .map(|rejection| (rejection.symbol, rejection.reason))
        .collect::<BTreeMap<_, _>>();
    let mut jin10_by_key = BTreeMap::<(String, String), &AllowedRow>::new();
    for row in jin10_rows {
        let Some(day) = source_day(row) else {
            continue;
        };
        jin10_by_key.insert((row.symbol.clone(), day.to_owned()), row);
    }

    let mut diagnostics = Vec::new();
    for candidate in candidates {
        let Some(rejection_reason) = rejection_reasons.get(&candidate.symbol) else {
            continue;
        };
        let current = current_fee_rule(conn, &candidate.symbol)?;
        let jin10 = source_day(candidate).and_then(|day| {
            jin10_by_key
                .get(&(candidate.symbol.clone(), day.to_owned()))
                .copied()
        });
        diagnostics.push(LatestCandidateDiagnostic {
            symbol: candidate.symbol.clone(),
            source_updated_at: candidate.source_updated_at.clone(),
            production: current.map_or_else(|| fee_tuple(candidate), |rule| rule.fees),
            qihuo: fee_tuple(candidate),
            jin10: jin10.map(fee_tuple),
            jin10_source_updated_at: jin10.and_then(|row| row.source_updated_at.clone()),
            rejection_reason: rejection_reason.clone(),
        });
    }
    Ok(diagnostics)
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

fn has_same_day_product_level_jin10_match(
    conn: &Connection,
    candidate: &AllowedRow,
    jin10_rows: &[AllowedRow],
) -> Result<bool> {
    let Some(day) = source_day(candidate) else {
        return Ok(false);
    };
    let product = derive_underlying_symbol(&candidate.symbol)?;
    let mut static_verified = false;
    let mut jin10_fee_verified = false;
    for row in jin10_rows {
        if source_day(row) == Some(day)
            && derive_underlying_symbol(&row.symbol)? == product
            && same_number(row.lot_size, candidate.lot_size)
            && same_number(row.tick_size, candidate.tick_size)
        {
            static_verified = true;
            jin10_fee_verified |= same_fee_rules(&fee_tuple(row), &fee_tuple(candidate));
        }
    }
    if !static_verified {
        return Ok(false);
    }
    if jin10_fee_verified {
        return Ok(true);
    }

    let symbols = {
        let mut statement = conn.prepare("select symbol from contracts order by symbol")?;
        statement
            .query_map([], |record| record.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    for symbol in symbols {
        if derive_underlying_symbol(&symbol)? == product
            && current_fee_rule(conn, &symbol)?
                .is_some_and(|rule| same_fee_rules(&rule.fees, &fee_tuple(candidate)))
        {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Find the fee tuple from the most recently listed existing contract in the
/// same product. A new contract may be admitted without an official change
/// notice only when its two-source observation carries this inherited tuple.
/// If the nearest prior listing has conflicting current rules, return `None`
/// so the caller keeps the candidate in the evidence-gated path.
fn inherited_product_fee_rule(
    conn: &Connection,
    candidate: &AllowedRow,
) -> Result<Option<[FeeSpec; 3]>> {
    let product = derive_underlying_symbol(&candidate.symbol)?;
    let candidate_listing = candidate
        .listing_date
        .as_deref()
        .map(contract_listing_day_start)
        .transpose()?;
    let mut statement = conn.prepare(
        "select c.symbol, c.listing_date, v.open_fee_json,
                v.close_yesterday_fee_json, v.close_today_fee_json
           from contracts c
           join fee_versions v on v.contract_id = c.id
          where v.valid_to is null
          order by c.listing_date desc, c.symbol desc",
    )?;
    let mut rows = statement.query([])?;
    let mut nearest: Option<(Option<OffsetDateTime>, [FeeSpec; 3])> = None;
    while let Some(record) = rows.next()? {
        let symbol: String = record.get(0)?;
        if derive_underlying_symbol(&symbol)? != product {
            continue;
        }
        let listing = record
            .get::<_, Option<String>>(1)?
            .as_deref()
            .map(contract_listing_day_start)
            .transpose()?;
        if let (Some(candidate_listing), Some(listing)) = (candidate_listing, listing)
            && listing >= candidate_listing
        {
            continue;
        }
        let fees = [
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(2)?)?,
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(3)?)?,
            serde_json::from_str::<FeeSpec>(&record.get::<_, String>(4)?)?,
        ];
        if let Some((nearest_listing, nearest_fees)) = &nearest {
            if listing < *nearest_listing {
                continue;
            }
            if listing == *nearest_listing && !same_fee_rules(nearest_fees, &fees) {
                return Ok(None);
            }
        }
        nearest = Some((listing, fees));
    }
    Ok(nearest.map(|(_, fees)| fees))
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

/// Compare external fee rows with the production rule effective at a historical
/// snapshot boundary, without changing the database.
///
/// # Errors
///
/// Returns an error when the comparison timestamp or fee rules cannot be read.
pub fn compare_fee_rows_as_of(
    conn: &Connection,
    rows: &[AllowedRow],
    effective_at: &str,
) -> Result<(usize, Vec<FeeRuleDifference>)> {
    parse_timestamp("effective_at", effective_at)?;
    let mut compared = 0usize;
    let mut differences = Vec::new();
    for row in rows {
        let Some(current) = fee_rule_as_of(conn, &row.symbol, effective_at)? else {
            continue;
        };
        compared += 1;
        let secondary = fee_tuple(row);
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
#[allow(clippy::too_many_lines)]
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

        create table if not exists contract_spec_versions(
          id integer primary key,
          contract_id integer not null,
          lot_size real not null check(lot_size > 0),
          tick_size real not null check(tick_size > 0),
          valid_from text not null,
          valid_to text check(valid_to is null or julianday(valid_to) > julianday(valid_from)),
          source_kind text not null check(source_kind in ('9qihuo', 'jin10', 'v11_baseline', 'official')),
          source_url text,
          first_seen_at text not null,
          last_seen_at text not null,
          foreign key(contract_id) references contracts(id)
        );
        create unique index if not exists idx_contract_spec_versions_open_contract
          on contract_spec_versions(contract_id)
          where valid_to is null;
        create unique index if not exists idx_contract_spec_versions_contract_valid_from
          on contract_spec_versions(contract_id, valid_from);

        create table if not exists contract_metadata_admissions(
          contract_id integer primary key,
          verification_level text not null
            check(verification_level in ('exact_contract', 'degraded_product')),
          primary_source_url text not null,
          secondary_source_url text not null,
          admitted_at text not null,
          last_verified_at text not null,
          foreign key(contract_id) references contracts(id)
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
          source_kind text not null default '9qihuo' check(source_kind in ('9qihuo', 'jin10', 'v11_baseline', 'official')),
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

        create table if not exists announcement_source_state(
          source text primary key,
          last_success_at text,
          last_published_at text,
          last_error_at text,
          last_error_message text
        );

        create table if not exists announcement_documents(
          source text not null,
          article_id text not null,
          title text not null,
          published_at text not null,
          broker_url text not null,
          first_seen_at text not null,
          last_seen_at text not null,
          primary key(source, article_id)
        );

        create table if not exists announcement_document_snapshots(
          id integer primary key,
          source text not null,
          article_id text not null,
          body_sha256 text not null check(length(body_sha256) = 64),
          body_html text not null,
          body_text text not null,
          official_urls_json text not null check(json_valid(official_urls_json)),
          fetched_at text not null,
          unique(source, article_id, body_sha256),
          foreign key(source, article_id) references announcement_documents(source, article_id)
        );

        create table if not exists announcement_candidates(
          source text not null,
          article_id text not null,
          keywords_json text not null check(json_valid(keywords_json)),
          official_urls_json text not null check(json_valid(official_urls_json)),
          detected_at text not null,
          resolved_at text,
          primary key(source, article_id),
          foreign key(source, article_id) references announcement_documents(source, article_id)
        );

        create table if not exists official_document_snapshots(
          canonical_url text not null,
          body_sha256 text not null check(length(body_sha256) = 64),
          body text not null,
          fetched_at text not null,
          primary key(canonical_url, body_sha256)
        );

        create table if not exists fee_version_evidence(
          contract_id integer not null,
          valid_from text not null,
          rule_hash text not null,
          evidence_level text not null
            check(evidence_level in ('paired_official', 'official_parameter')),
          canonical_url text not null,
          body_sha256 text not null check(length(body_sha256) = 64),
          recorded_at text not null,
          primary key(contract_id, valid_from, rule_hash, canonical_url, body_sha256),
            foreign key(contract_id) references contracts(id)
        );
        create table if not exists contract_spec_evidence(
            contract_id integer not null,
            valid_from text not null,
            canonical_url text not null,
            body_sha256 text not null check(length(body_sha256) = 64),
            recorded_at text not null,
            primary key(contract_id, valid_from, canonical_url, body_sha256),
            foreign key(contract_id) references contracts(id)
        );
        create table if not exists contract_lifecycle_evidence(
            contract_id integer not null,
            listing_date text not null,
            expiry_date text not null,
            canonical_url text not null,
            body_sha256 text not null check(length(body_sha256) = 64),
            recorded_at text not null,
            primary key(contract_id, canonical_url, body_sha256),
            foreign key(contract_id) references contracts(id)
        );
        ",
    )?;

    conn.execute_batch("drop index if exists idx_fee_versions_contract;")?;

    ensure_fee_version_source_kind_column(conn)?;
    repair_fee_versions_before_listing(conn)?;
    seed_missing_contract_spec_versions(conn)?;

    Ok(())
}

fn seed_missing_contract_spec_versions(conn: &Connection) -> Result<()> {
    conn.execute(
        "insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         )
         select c.id, c.lot_size, c.tick_size,
                coalesce(
                  case
                    when length(c.listing_date) = 8 then
                      substr(c.listing_date, 1, 4) || '-' ||
                      substr(c.listing_date, 5, 2) || '-' ||
                      substr(c.listing_date, 7, 2) || 'T00:00:00+08:00'
                    when length(c.listing_date) = 10 then
                      c.listing_date || 'T00:00:00+08:00'
                  end,
                  (select min(fv.valid_from) from fee_versions fv where fv.contract_id = c.id),
                  c.first_seen_at
                ),
                null, 'v11_baseline', null, c.first_seen_at, c.last_seen_at
         from contracts c
         where not exists (
           select 1 from contract_spec_versions csv where csv.contract_id = c.id
         )",
        [],
    )?;
    Ok(())
}

/// Apply the reviewed exchange-wide contract-specification changes currently
/// known to this dataset.
///
/// Each transition is sourced from an exchange notice and applies to every
/// contract of the product that was still listed on the effective date. The
/// operation is idempotent per contract and preserves contracts that expired
/// before the transition.
///
/// # Errors
///
/// Returns an error when timestamps, symbols, or database writes are invalid.
pub fn migrate_known_contract_spec_history(
    conn: &mut Connection,
    observed_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    parse_timestamp("observed_at", observed_at)?;
    let contracts = load_contract_identities(conn)?;
    let tx = conn.transaction()?;
    let mut changed = 0usize;
    for transition in KNOWN_CONTRACT_SPEC_TRANSITIONS {
        changed += apply_known_contract_spec_transition(&tx, &transition, &contracts, observed_at)?;
    }
    tx.commit()?;
    Ok(changed)
}

#[derive(Clone, Copy)]
struct KnownContractSpecTransition {
    product: &'static str,
    effective_at: &'static str,
    lot_size: f64,
    old_tick: f64,
    new_tick: f64,
    source_url: &'static str,
}

const KNOWN_CONTRACT_SPEC_TRANSITIONS: [KnownContractSpecTransition; 4] = [
    KnownContractSpecTransition {
        product: "DCE.p",
        effective_at: "2026-04-10T00:00:00+08:00",
        lot_size: 10.0,
        old_tick: 2.0,
        new_tick: 1.0,
        source_url: "http://www.dce.com.cn/dce/content/2026/ywggytz/18628268.html",
    },
    KnownContractSpecTransition {
        product: "DCE.y",
        effective_at: "2026-04-10T00:00:00+08:00",
        lot_size: 10.0,
        old_tick: 2.0,
        new_tick: 1.0,
        source_url: "http://www.dce.com.cn/dce/content/2026/ywggytz/18628268.html",
    },
    KnownContractSpecTransition {
        product: "GFEX.lc",
        effective_at: "2024-12-18T00:00:00+08:00",
        lot_size: 1.0,
        old_tick: 50.0,
        new_tick: 20.0,
        source_url: "http://www.gfex.com.cn/gfex/tzts/202412/917905b781b040d1bfc189c0b5559d24.shtml",
    },
    KnownContractSpecTransition {
        product: "INE.ec",
        effective_at: "2026-05-11T00:00:00+08:00",
        lot_size: 50.0,
        old_tick: 0.1,
        new_tick: 0.5,
        source_url: "https://www.ine.cn/publicnotice/notice/202601/t20260116_830126.html",
    },
];

#[derive(Debug)]
struct ContractIdentity {
    id: i64,
    symbol: String,
    listing_date: Option<String>,
    expiry_date: Option<String>,
}

fn load_contract_identities(conn: &Connection) -> Result<Vec<ContractIdentity>> {
    let mut statement =
        conn.prepare("select id, symbol, listing_date, expiry_date from contracts order by id")?;
    Ok(statement
        .query_map([], |record| {
            Ok(ContractIdentity {
                id: record.get(0)?,
                symbol: record.get(1)?,
                listing_date: record.get(2)?,
                expiry_date: record.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn apply_known_contract_spec_transition(
    tx: &Transaction<'_>,
    transition: &KnownContractSpecTransition,
    contracts: &[ContractIdentity],
    observed_at: &str,
) -> Result<usize> {
    let effective = parse_timestamp("contract spec effective_at", transition.effective_at)?;
    let mut changed = 0usize;
    for contract in contracts {
        if derive_underlying_symbol(&contract.symbol)? != transition.product {
            continue;
        }
        let transition_exists = official_spec_transition_exists(tx, contract.id, transition)?;
        if contract_expired_before(contract, effective)? {
            if transition_exists {
                repair_expired_contract_spec_transition(tx, contract, transition, observed_at)?;
                changed += 1;
            }
            continue;
        }
        if transition_exists {
            continue;
        }
        replace_contract_spec_transition(tx, contract, transition, effective, observed_at)?;
        changed += 1;
    }
    Ok(changed)
}

fn repair_expired_contract_spec_transition(
    tx: &Transaction<'_>,
    contract: &ContractIdentity,
    transition: &KnownContractSpecTransition,
    observed_at: &str,
) -> Result<()> {
    let earliest: String = tx.query_row(
        "select min(valid_from) from contract_spec_versions where contract_id = ?1",
        params![contract.id],
        |record| record.get(0),
    )?;
    tx.execute(
        "delete from contract_spec_versions where contract_id = ?1",
        params![contract.id],
    )?;
    tx.execute(
        "insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (?1, ?2, ?3, ?4, null, 'v11_baseline', null, ?5, ?5)",
        params![
            contract.id,
            transition.lot_size,
            transition.old_tick,
            earliest,
            observed_at
        ],
    )?;
    tx.execute(
        "update contracts set lot_size = ?1, tick_size = ?2 where id = ?3",
        params![transition.lot_size, transition.old_tick, contract.id],
    )?;
    Ok(())
}

fn contract_expired_before(contract: &ContractIdentity, effective: OffsetDateTime) -> Result<bool> {
    if let Some(expiry_date) = contract.expiry_date.as_deref() {
        return Ok(contract_listing_day_start(expiry_date)? < effective);
    }
    let Some((year, month)) = inferred_contract_year_month(&contract.symbol, effective.year())?
    else {
        return Ok(false);
    };
    let effective_month = u8::from(effective.month());
    Ok((year, month) < (effective.year(), effective_month))
}

fn inferred_contract_year_month(symbol: &str, reference_year: i32) -> Result<Option<(i32, u8)>> {
    let (_, local) = symbol
        .split_once('.')
        .ok_or_else(|| anyhow!("invalid contract symbol {symbol}"))?;
    let suffix = local
        .trim_start_matches(|character: char| character.is_ascii_alphabetic())
        .as_bytes();
    let (year, month) = match suffix {
        [year_tens, year_ones, month_tens, month_ones] if suffix.iter().all(u8::is_ascii_digit) => {
            let year = 2000 + i32::from((year_tens - b'0') * 10 + (year_ones - b'0'));
            let month = (month_tens - b'0') * 10 + (month_ones - b'0');
            (year, month)
        }
        [year_digit, month_tens, month_ones] if suffix.iter().all(u8::is_ascii_digit) => {
            let mut year =
                reference_year - reference_year.rem_euclid(10) + i32::from(year_digit - b'0');
            if year > reference_year + 5 {
                year -= 10;
            }
            let month = (month_tens - b'0') * 10 + (month_ones - b'0');
            (year, month)
        }
        _ => return Ok(None),
    };
    if !(1..=12).contains(&month) {
        return Err(anyhow!("invalid contract month in symbol {symbol}"));
    }
    Ok(Some((year, month)))
}

fn official_spec_transition_exists(
    tx: &Transaction<'_>,
    contract_id: i64,
    transition: &KnownContractSpecTransition,
) -> Result<bool> {
    Ok(tx
        .query_row(
            "select 1 from contract_spec_versions
             where contract_id = ?1 and source_kind = 'official'
               and source_url = ?2 and tick_size = ?3 limit 1",
            params![contract_id, transition.source_url, transition.new_tick],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

fn replace_contract_spec_transition(
    tx: &Transaction<'_>,
    contract: &ContractIdentity,
    transition: &KnownContractSpecTransition,
    effective: OffsetDateTime,
    observed_at: &str,
) -> Result<()> {
    let earliest: String = tx.query_row(
        "select min(valid_from) from contract_spec_versions where contract_id = ?1",
        params![contract.id],
        |record| record.get(0),
    )?;
    let earliest_at = parse_timestamp("contract spec earliest", &earliest)?;
    let listing = contract
        .listing_date
        .as_deref()
        .map(contract_listing_day_start)
        .transpose()?
        .unwrap_or(earliest_at);
    let initial_at = listing.max(earliest_at);
    let initial = initial_at.format(&Rfc3339)?;
    tx.execute(
        "delete from contract_spec_versions where contract_id = ?1",
        params![contract.id],
    )?;
    if initial_at < effective {
        insert_official_contract_spec(
            tx,
            contract.id,
            transition,
            transition.old_tick,
            &initial,
            Some(transition.effective_at),
            observed_at,
        )?;
    }
    let new_valid_from = if initial_at < effective {
        transition.effective_at
    } else {
        &initial
    };
    insert_official_contract_spec(
        tx,
        contract.id,
        transition,
        transition.new_tick,
        new_valid_from,
        None,
        observed_at,
    )?;
    tx.execute(
        "update contracts set lot_size = ?1, tick_size = ?2 where id = ?3",
        params![transition.lot_size, transition.new_tick, contract.id],
    )?;
    Ok(())
}

fn insert_official_contract_spec(
    tx: &Transaction<'_>,
    contract_id: i64,
    transition: &KnownContractSpecTransition,
    tick_size: f64,
    valid_from: &str,
    valid_to: Option<&str>,
    observed_at: &str,
) -> Result<()> {
    tx.execute(
        "insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (?1, ?2, ?3, ?4, ?5, 'official', ?6, ?7, ?7)",
        params![
            contract_id,
            transition.lot_size,
            tick_size,
            valid_from,
            valid_to,
            transition.source_url,
            observed_at
        ],
    )?;
    Ok(())
}

/// Record how newly listed contract metadata was independently corroborated.
///
/// # Errors
///
/// Returns an error if a verified new contract has not yet been inserted or a
/// database write fails.
/// Persist newly listed contracts that passed the cross-source admission gate.
///
/// This is intentionally independent from fee-change admission: a rejected
/// incumbent fee candidate must not prevent a correctly inherited new
/// contract from being added to the next snapshot.
///
/// # Errors
///
/// Returns an error when row persistence or admission evidence recording
/// fails.
pub fn persist_new_contract_admissions(
    conn: &mut Connection,
    verification: &LatestCandidateVerification,
    observed_at: &str,
) -> Result<()> {
    if verification.new_contracts.is_empty() {
        return Ok(());
    }
    upsert_allowed_rows(conn, &verification.new_contracts, observed_at)?;
    record_new_contract_metadata_admissions(conn, verification, observed_at)
}

/// Record how newly listed contract metadata independently corroborated.
///
/// # Errors
///
/// Returns an error if a verified new contract has not yet been inserted or
/// the database write fails.
pub fn record_new_contract_metadata_admissions(
    conn: &Connection,
    verification: &LatestCandidateVerification,
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    parse_timestamp("observed_at", observed_at)?;
    for row in &verification.new_contracts {
        let contract_id = conn
            .query_row(
                "select id from contracts where symbol = ?1",
                params![row.symbol],
                |record| record.get::<_, i64>(0),
            )
            .optional()?
            .ok_or_else(|| {
                anyhow!(
                    "new contract {} was not inserted before admission",
                    row.symbol
                )
            })?;
        let degraded = verification
            .degraded_new_contracts
            .iter()
            .any(|symbol| symbol == &row.symbol);
        let level = if degraded {
            "degraded_product"
        } else {
            "exact_contract"
        };
        conn.execute(
            "insert into contract_metadata_admissions(
               contract_id, verification_level, primary_source_url,
               secondary_source_url, admitted_at, last_verified_at
             ) values (?1, ?2, ?3, ?4, ?5, ?5)
             on conflict(contract_id) do update set
               verification_level = excluded.verification_level,
               last_verified_at = excluded.last_verified_at",
            params![
                contract_id,
                level,
                "https://www.9qihuo.com/qihuoshouxufei",
                "https://mp-api.jin10.com/api/dynamic-data/child",
                observed_at
            ],
        )?;
    }
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
    let definition = conn.query_row(
        "select sql from sqlite_master where type = 'table' and name = 'fee_versions'",
        [],
        |row| row.get::<_, String>(0),
    )?;
    if !definition.contains("'official'") {
        conn.execute_batch(
            "
            begin;
            drop index if exists idx_fee_versions_open_contract;
            drop index if exists idx_fee_versions_contract_valid_from;
            create table fee_versions_with_official_source(
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
              source_kind text not null default '9qihuo' check(source_kind in ('9qihuo', 'jin10', 'v11_baseline', 'official')),
              source_updated_at text,
              valid_from text not null,
              valid_to text check(valid_to is null or julianday(valid_to) > julianday(valid_from)),
              first_seen_at text not null,
              last_seen_at text not null,
              foreign key(contract_id) references contracts(id)
            );
            insert into fee_versions_with_official_source
              select id, contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
                     open_fee_json, close_yesterday_fee_json, close_today_fee_json,
                     trading_status, is_main_contract, source_kind, source_updated_at,
                     valid_from, valid_to, first_seen_at, last_seen_at
              from fee_versions;
            drop table fee_versions;
            alter table fee_versions_with_official_source rename to fee_versions;
            create unique index idx_fee_versions_open_contract
              on fee_versions(contract_id) where valid_to is null;
            create unique index idx_fee_versions_contract_valid_from
              on fee_versions(contract_id, valid_from);
            commit;
            ",
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
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
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
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
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

/// Persist a broker document and an immutable selected-body snapshot.
///
/// Returns `true` only when the document body introduces a new snapshot.
/// This never changes fee history.
///
/// # Errors
///
/// Returns an error when schema creation, serialization, or persistence fails.
pub fn record_announcement_document(
    conn: &Connection,
    document: &AnnouncementDocument,
    fetched_at: &str,
) -> Result<bool> {
    ensure_schema(conn)?;
    let source = document.item.source.as_str();
    conn.execute(
        "insert into announcement_documents(
            source, article_id, title, published_at, broker_url, first_seen_at, last_seen_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         on conflict(source, article_id) do update set
           title = excluded.title,
           published_at = excluded.published_at,
           broker_url = excluded.broker_url,
           last_seen_at = excluded.last_seen_at",
        params![
            source,
            document.item.article_id,
            document.item.title,
            document.item.published_at,
            document.item.url,
            fetched_at,
        ],
    )?;
    let official_urls = serde_json::to_string(&document.official_urls)?;
    let body_sha256 = content_sha256(&document.body_html);
    let inserted = conn.execute(
        "insert or ignore into announcement_document_snapshots(
            source, article_id, body_sha256, body_html, body_text, official_urls_json, fetched_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            source,
            document.item.article_id,
            body_sha256,
            document.body_html,
            document.body_text,
            official_urls,
            fetched_at,
        ],
    )?;
    Ok(inserted == 1)
}

/// Persist a candidate detected from a selected broker body.
///
/// # Errors
///
/// Returns an error when the corresponding announcement document is absent or
/// candidate metadata cannot be serialized.
pub fn record_announcement_candidate(
    conn: &Connection,
    candidate: &AnnouncementCandidate,
    detected_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let inherited_resolution = candidate
        .official_urls
        .iter()
        .map(|url| {
            conn.query_row(
                "select 1
                   from announcement_candidates candidate,
                        json_each(candidate.official_urls_json) official_url
                  where candidate.resolved_at is not null
                    and official_url.value = ?1
                  limit 1",
                params![url],
                |_| Ok(()),
            )
            .optional()
        })
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .any(|match_| match_.is_some())
        .then_some(detected_at);
    conn.execute(
        "insert into announcement_candidates(
            source, article_id, keywords_json, official_urls_json, detected_at, resolved_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6)
         on conflict(source, article_id) do update set
           keywords_json = excluded.keywords_json,
           official_urls_json = excluded.official_urls_json,
           resolved_at = coalesce(announcement_candidates.resolved_at, excluded.resolved_at)",
        params![
            candidate.source.as_str(),
            candidate.article_id,
            serde_json::to_string(&candidate.keywords)?,
            serde_json::to_string(&candidate.official_urls)?,
            detected_at,
            inherited_resolution,
        ],
    )?;
    Ok(())
}

/// Record a successful source scan without clearing persisted evidence.
///
/// # Errors
///
/// Returns an error when the source state cannot be persisted.
pub fn record_announcement_source_success(
    conn: &Connection,
    source: AnnouncementSource,
    last_published_at: Option<&str>,
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "insert into announcement_source_state(source, last_success_at, last_published_at)
         values (?1, ?2, ?3)
         on conflict(source) do update set
           last_success_at = excluded.last_success_at,
           last_published_at = case
             when excluded.last_published_at is null then announcement_source_state.last_published_at
             when announcement_source_state.last_published_at is null then excluded.last_published_at
             when excluded.last_published_at > announcement_source_state.last_published_at
               then excluded.last_published_at
             else announcement_source_state.last_published_at
           end,
           last_error_at = null,
           last_error_message = null",
        params![source.as_str(), observed_at, last_published_at],
    )?;
    Ok(())
}

/// Record an announcement source failure without masking the previous watermark.
///
/// # Errors
///
/// Returns an error when the source state cannot be persisted.
pub fn record_announcement_source_error(
    conn: &Connection,
    source: AnnouncementSource,
    message: &str,
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    conn.execute(
        "insert into announcement_source_state(source, last_error_at, last_error_message)
         values (?1, ?2, ?3)
         on conflict(source) do update set
           last_error_at = excluded.last_error_at,
           last_error_message = excluded.last_error_message",
        params![source.as_str(), observed_at, message],
    )?;
    Ok(())
}

/// Require a current successful broker-announcement scan and no stale candidate.
///
/// A source error supersedes an earlier success from that source. At least one
/// source must have completed successfully within one hour: CITIC normally,
/// or HTFC during same-run fallback. A potential fee-adjustment candidate may
/// remain queued while its exchange original is examined, but it blocks live
/// source refresh once it has remained unresolved for 24 hours.
///
/// # Errors
///
/// Returns an error when scan state is missing, stale, superseded by a newer
/// source error, or when an unresolved candidate has exceeded 24 hours.
pub fn announcement_health(conn: &Connection, observed_at: &str) -> Result<AnnouncementHealth> {
    ensure_schema(conn)?;
    let observed = parse_timestamp("announcement health timestamp", observed_at)?;
    let fresh_after = observed - MAX_ANNOUNCEMENT_SCAN_AGE;
    let mut statement = conn.prepare(
        "select source, last_success_at, last_error_at
         from announcement_source_state",
    )?;
    let mut rows = statement.query([])?;
    let mut fresh_sources = Vec::new();
    while let Some(row) = rows.next()? {
        let source: String = row.get(0)?;
        let success_at: Option<String> = row.get(1)?;
        let error_at: Option<String> = row.get(2)?;
        let Some(success_at) = success_at else {
            continue;
        };
        let success = parse_timestamp("announcement source success", &success_at)?;
        let superseded_by_error = error_at
            .as_deref()
            .map(|value| parse_timestamp("announcement source error", value))
            .transpose()?
            .is_some_and(|error| error > success);
        if success >= fresh_after && !superseded_by_error {
            fresh_sources.push(source);
        }
    }
    if fresh_sources.is_empty() {
        return Err(anyhow!(
            "no fresh successful announcement scan within {} minutes",
            MAX_ANNOUNCEMENT_SCAN_AGE.whole_minutes()
        ));
    }

    let mut statement = conn.prepare(
        "select source, article_id, detected_at
         from announcement_candidates
         where resolved_at is null
         order by detected_at, source, article_id",
    )?;
    let mut rows = statement.query([])?;
    let mut pending_candidates = 0usize;
    let mut stale_candidates = Vec::new();
    while let Some(row) = rows.next()? {
        let source: String = row.get(0)?;
        let article_id: String = row.get(1)?;
        let detected_at: String = row.get(2)?;
        let detected = parse_timestamp("announcement candidate detected_at", &detected_at)?;
        if detected + MAX_UNRESOLVED_CANDIDATE_AGE <= observed {
            stale_candidates.push(format!("{source}:{article_id}"));
        } else {
            pending_candidates += 1;
        }
    }
    if !stale_candidates.is_empty() {
        return Err(anyhow!(
            "unresolved fee candidate older than {} hours blocks live refresh: {}",
            MAX_UNRESOLVED_CANDIDATE_AGE.whole_hours(),
            stale_candidates.join(",")
        ));
    }

    Ok(AnnouncementHealth {
        fresh_sources,
        pending_candidates,
    })
}

/// Persist one immutable exchange-original document snapshot.
///
/// Returns `true` when the body hash has not previously been retained for the
/// canonical URL. A fetch failure is intentionally handled by the caller as
/// unresolved candidate evidence, not as permission to alter fee history.
///
/// # Errors
///
/// Returns an error when schema creation or snapshot persistence fails.
pub fn record_official_document_snapshot(
    conn: &Connection,
    canonical_url: &str,
    body: &str,
    fetched_at: &str,
) -> Result<bool> {
    ensure_schema(conn)?;
    let inserted = conn.execute(
        "insert or ignore into official_document_snapshots(
            canonical_url, body_sha256, body, fetched_at
         ) values (?1, ?2, ?3, ?4)",
        params![canonical_url, content_sha256(body), body, fetched_at],
    )?;
    Ok(inserted == 1)
}

/// Resolve broker candidates linked to official documents applied to history.
///
/// # Errors
///
/// Returns an error when candidate state cannot be updated.
pub fn resolve_announcement_candidates_for_official_urls(
    conn: &Connection,
    official_urls: &[String],
    resolved_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    let mut resolved = 0usize;
    for url in official_urls {
        resolved += conn.execute(
            "update announcement_candidates set resolved_at = ?1
             where resolved_at is null and exists (
               select 1 from json_each(official_urls_json) where value = ?2
             )",
            params![resolved_at, url],
        )?;
    }
    Ok(resolved)
}

/// Return whether a broker article has already had its selected body persisted.
///
/// # Errors
///
/// Returns an error when the announcement state cannot be queried.
pub fn announcement_document_exists(
    conn: &Connection,
    source: AnnouncementSource,
    article_id: &str,
) -> Result<bool> {
    let exists = conn
        .query_row(
            "select 1 from announcement_documents where source = ?1 and article_id = ?2",
            params![source.as_str(), article_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(exists)
}

/// Return exchange-original URLs for a candidate that do not yet have a
/// persisted immutable snapshot.
///
/// # Errors
///
/// Returns an error when candidate JSON cannot be read or decoded.
pub fn pending_candidate_official_urls(
    conn: &Connection,
    source: AnnouncementSource,
    article_id: &str,
) -> Result<Vec<String>> {
    let urls = conn
        .query_row(
            "select official_urls_json from announcement_candidates
              where source = ?1 and article_id = ?2",
            params![source.as_str(), article_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    let Some(urls) = urls else {
        return Ok(Vec::new());
    };
    let urls = serde_json::from_str::<Vec<String>>(&urls)?;
    let mut pending = Vec::new();
    for url in urls {
        let retained = conn
            .query_row(
                "select 1 from official_document_snapshots where canonical_url = ?1 limit 1",
                [&url],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !retained {
            pending.push(url);
        }
    }
    Ok(pending)
}

/// Return the most recent successful publication-date watermark for one broker source.
///
/// # Errors
///
/// Returns an error when the announcement state cannot be queried.
pub fn announcement_source_watermark(
    conn: &Connection,
    source: AnnouncementSource,
) -> Result<Option<String>> {
    conn.query_row(
        "select last_published_at from announcement_source_state where source = ?1",
        [source.as_str()],
        |row| row.get::<_, Option<String>>(0),
    )
    .optional()
    .map(Option::flatten)
    .map_err(Into::into)
}

/// Return the newest fee-history effective timestamp, if history exists.
///
/// # Errors
///
/// Returns an error when fee history cannot be queried.
pub fn latest_fee_effective_at(conn: &Connection) -> Result<Option<String>> {
    conn.query_row("select max(valid_from) from fee_versions", [], |row| {
        row.get::<_, Option<String>>(0)
    })
    .map_err(Into::into)
}

fn content_sha256(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
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

/// Confidence assigned to one official fee-history input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfficialEvidenceLevel {
    PairedOfficial,
    OfficialParameter,
}

impl OfficialEvidenceLevel {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PairedOfficial => "paired_official",
            Self::OfficialParameter => "official_parameter",
        }
    }
}

/// Retained official document linked to one fee version.
#[derive(Debug, Clone)]
pub struct OfficialEvidenceReference {
    pub canonical_url: String,
    pub body_sha256: String,
}

/// Complete fee tuple parsed from retained exchange bytes.
#[derive(Debug, Clone)]
pub struct OfficialHistoryRow {
    pub row: AllowedRow,
    /// First timestamp not covered by retained parameter sequence.
    pub coverage_end_exclusive: String,
    pub evidence_level: OfficialEvidenceLevel,
    pub evidence: Vec<OfficialEvidenceReference>,
}

/// Atomically replace contradicted lower-confidence history with retained
/// exchange parameters.
///
/// Existing paired-official versions remain immutable. Third-party and
/// baseline versions inside retained observation interval are removed.
///
/// # Errors
///
/// Returns an error for malformed timestamps, conflicting same-instant rules,
/// or database failures.
#[allow(clippy::too_many_lines)]
pub fn replace_with_official_parameter_history(
    conn: &mut Connection,
    rows: &[OfficialHistoryRow],
    observed_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    parse_timestamp("observed_at", observed_at)?;
    let prepared = rows
        .iter()
        .map(|item| {
            let mut prepared = prepare_rows(std::slice::from_ref(&item.row), observed_at)?;
            let prepared = prepared
                .pop()
                .ok_or_else(|| anyhow!("official parameter row disappeared during preparation"))?;
            let coverage_end_at =
                parse_timestamp("coverage_end_exclusive", &item.coverage_end_exclusive)?;
            if coverage_end_at <= prepared.valid_from_at {
                return Err(anyhow!(
                    "official parameter coverage ends before observation for {}",
                    item.row.symbol
                ));
            }
            Ok((item, prepared, coverage_end_at))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut by_symbol = BTreeMap::<String, Vec<_>>::new();
    for item in prepared {
        by_symbol
            .entry(item.1.row.symbol.clone())
            .or_default()
            .push(item);
    }

    let tx = conn.transaction()?;
    let mut materialized = 0usize;
    for items in by_symbol.values_mut() {
        items.sort_by_key(|item| item.1.valid_from_at);
        let latest_row = items
            .last()
            .map(|item| item.1.row.clone())
            .ok_or_else(|| anyhow!("empty official parameter contract group"))?;
        let contract_id = tx
            .query_row(
                "select id from contracts where symbol = ?1",
                [&latest_row.symbol],
                |record| record.get::<_, i64>(0),
            )
            .optional()?
            .map_or_else(
                || upsert_contract(&tx, &latest_row, observed_at, IngestMode::V11Baseline),
                |contract_id| {
                    tx.execute(
                        "update contracts
                         set listing_date = coalesce(listing_date, ?2),
                             expiry_date = coalesce(expiry_date, ?3)
                         where id = ?1",
                        params![contract_id, latest_row.listing_date, latest_row.expiry_date,],
                    )?;
                    Ok(contract_id)
                },
            )?;
        let first_at = items[0].1.valid_from_at;
        let coverage_end_at = items
            .iter()
            .map(|item| item.2)
            .max()
            .ok_or_else(|| anyhow!("empty official parameter contract group"))?;

        let existing_parameter_keys = {
            let mut statement = tx.prepare(
                "select valid_from, rule_hash from fee_version_evidence
                 where contract_id = ?1 and evidence_level = 'official_parameter'",
            )?;
            statement
                .query_map([contract_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        let existing = load_existing_versions(&tx, contract_id)?;
        let paired_times = existing
            .iter()
            .filter(|version| {
                version.source_kind == FeeSource::Official
                    && !existing_parameter_keys
                        .contains(&(version.valid_from.clone(), version.rule_hash.clone()))
            })
            .map(|version| version.valid_from_at)
            .collect::<BTreeSet<_>>();

        let mut versions = existing
            .into_iter()
            .filter(|version| {
                let is_parameter = existing_parameter_keys
                    .contains(&(version.valid_from.clone(), version.rule_hash.clone()));
                if is_parameter {
                    return false;
                }
                version.source_kind == FeeSource::Official
                    || version.valid_from_at < first_at
                    || version.valid_from_at >= coverage_end_at
            })
            .collect::<Vec<_>>();
        let mut parameter_keys = BTreeSet::new();
        let mut evidence = Vec::new();
        for (source, prepared, _) in items.iter() {
            if paired_times.contains(&prepared.valid_from_at) {
                continue;
            }
            let key = (prepared.valid_from.clone(), prepared.rule_hash.clone());
            parameter_keys.insert(key.clone());
            evidence.push((key, source.evidence_level, source.evidence.clone()));
            versions.push(VersionRecord {
                row: prepared.row.clone(),
                rule_hash: prepared.rule_hash.clone(),
                valid_from: prepared.valid_from.clone(),
                valid_from_at: prepared.valid_from_at,
                first_seen_at: observed_at.to_owned(),
                last_seen_at: observed_at.to_owned(),
                source_kind: FeeSource::Official,
            });
        }
        versions.sort_by(|left, right| {
            left.valid_from_at
                .cmp(&right.valid_from_at)
                .then_with(|| left.rule_hash.cmp(&right.rule_hash))
        });

        let mut rebuilt = Vec::<VersionRecord>::new();
        for version in versions {
            let Some(previous) = rebuilt.last_mut() else {
                rebuilt.push(version);
                continue;
            };
            if previous.valid_from_at == version.valid_from_at {
                if previous.rule_hash != version.rule_hash {
                    return Err(anyhow!(
                        "conflicting paired official rules for {} at {}",
                        version.row.symbol,
                        version.valid_from
                    ));
                }
                merge_equivalent_version(previous, version)?;
                continue;
            }
            if previous.rule_hash == version.rule_hash {
                let previous_is_parameter = parameter_keys
                    .contains(&(previous.valid_from.clone(), previous.rule_hash.clone()));
                let current_is_parameter = parameter_keys
                    .contains(&(version.valid_from.clone(), version.rule_hash.clone()));
                if previous_is_parameter && !current_is_parameter {
                    merge_equivalent_version(previous, version)?;
                    continue;
                }
                if !current_is_parameter {
                    merge_equivalent_version(previous, version)?;
                    continue;
                }
            }
            rebuilt.push(version);
        }

        tx.execute(
            "delete from fee_version_evidence
             where contract_id = ?1 and evidence_level = 'official_parameter'",
            [contract_id],
        )?;
        replace_fee_versions(&tx, contract_id, &rebuilt)?;
        for ((valid_from, rule_hash), evidence_level, references) in evidence {
            if !rebuilt
                .iter()
                .any(|version| version.valid_from == valid_from && version.rule_hash == rule_hash)
            {
                continue;
            }
            for reference in references {
                tx.execute(
                    "insert into fee_version_evidence(
                       contract_id, valid_from, rule_hash, evidence_level,
                       canonical_url, body_sha256, recorded_at
                     ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        contract_id,
                        valid_from,
                        rule_hash,
                        evidence_level.as_str(),
                        reference.canonical_url,
                        reference.body_sha256,
                        observed_at
                    ],
                )?;
            }
            materialized += 1;
        }
    }
    tx.commit()?;
    Ok(materialized)
}

/// Apply one complete, verified official fee tuple as a forward SCD2 version.
///
/// The target contract must already exist in the reviewed baseline. Static
/// metadata and non-fee state are copied from its current approved version;
/// only the fee tuple and provenance change.
///
/// # Errors
///
/// Returns an error when the contract is absent, the effective timestamp is
/// not later than its current version, or the official version cannot be
/// persisted.
pub fn apply_official_fee_tuple(
    conn: &mut Connection,
    symbol: &str,
    effective_at: &str,
    fees: &[FeeSpec; 3],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let effective = parse_timestamp("official effective_at", effective_at)?;
    let base = conn
        .query_row(
            "select c.listing_date, c.expiry_date, c.lot_size, c.tick_size,
                    v.buy_margin_rate, v.sell_margin_rate, v.trading_status,
                    v.is_main_contract, v.valid_from
             from contracts c
             join fee_versions v on v.contract_id = c.id
             where c.symbol = ?1 and v.valid_to is null
             order by v.valid_from desc, v.id desc
             limit 1",
            [symbol],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, f64>(2)?,
                    row.get::<_, f64>(3)?,
                    row.get::<_, Option<f64>>(4)?,
                    row.get::<_, Option<f64>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            anyhow!("official adjustment contract missing approved baseline: {symbol}")
        })?;
    if effective <= parse_timestamp("current fee valid_from", &base.8)? {
        return Err(anyhow!(
            "official effective_at must be later than current fee version for {symbol}"
        ));
    }
    let trading_status = match base.6.as_str() {
        "Trading" => TradingStatus::Trading,
        "NotTrading" => TradingStatus::NotTrading,
        "Unknown" => TradingStatus::Unknown,
        other => return Err(anyhow!("unknown persisted trading status: {other}")),
    };
    let row = AllowedRow {
        symbol: symbol.to_owned(),
        listing_date: base.0,
        expiry_date: base.1,
        trading_status,
        buy_margin_rate: base.4,
        sell_margin_rate: base.5,
        open_fee: fees[0].clone(),
        close_yesterday_fee: fees[1].clone(),
        close_today_fee: fees[2].clone(),
        lot_size: base.2,
        tick_size: base.3,
        source_updated_at: Some(effective_at.to_owned()),
        is_main_contract: base.7 != 0,
    };
    upsert_rows(conn, &[row], observed_at, IngestMode::Official)
}

/// Apply a verified before/after official transition atomically.
///
/// In addition to the normal forward case, this narrowly repairs one known
/// baseline failure mode: a contract whose only version already contains the
/// post-change tuple but starts before the official effective day. The paired
/// official parameters must supply both complete tuples, so the premature row
/// can be split without product-level inference.
///
/// # Errors
///
/// Returns an error when the open rule matches neither official tuple, when a
/// premature post-change rule has established predecessors, or when the
/// transition boundary cannot be persisted atomically.
#[allow(clippy::too_many_lines)]
pub fn apply_official_fee_transition(
    conn: &mut Connection,
    symbol: &str,
    effective_at: &str,
    previous_fees: &[FeeSpec; 3],
    fees: &[FeeSpec; 3],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let effective = parse_timestamp("official transition effective_at", effective_at)?;
    parse_timestamp("official transition observed_at", observed_at)?;
    let tx = conn.transaction()?;
    let base = tx
        .query_row(
            "select v.id, c.id, c.listing_date, c.expiry_date, c.lot_size, c.tick_size,
                    v.buy_margin_rate, v.sell_margin_rate, v.open_fee_json,
                    v.close_yesterday_fee_json, v.close_today_fee_json,
                    v.trading_status, v.is_main_contract, v.valid_from
             from contracts c
             join fee_versions v on v.contract_id = c.id
             where c.symbol = ?1 and v.valid_to is null
             order by v.valid_from desc, v.id desc
             limit 1",
            [symbol],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                    row.get::<_, Option<f64>>(6)?,
                    row.get::<_, Option<f64>>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, String>(13)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| {
            anyhow!("official transition contract missing approved baseline: {symbol}")
        })?;
    let valid_from = parse_timestamp("current fee valid_from", &base.13)?;
    let current_fees = [
        parse_fee_json(&base.8)?,
        parse_fee_json(&base.9)?,
        parse_fee_json(&base.10)?,
    ];
    if effective == valid_from && same_fee_rules(&current_fees, fees) {
        tx.execute(
            "update fee_versions
             set source_kind = 'official', source_updated_at = ?1, last_seen_at = ?2
             where id = ?3",
            params![effective_at, observed_at, base.0],
        )?;
        tx.commit()?;
        return Ok(());
    }
    if effective <= valid_from {
        return Err(anyhow!(
            "official transition must be later than current fee version for {symbol}"
        ));
    }

    let trading_status = match base.11.as_str() {
        "Trading" => TradingStatus::Trading,
        "NotTrading" => TradingStatus::NotTrading,
        "Unknown" => TradingStatus::Unknown,
        other => return Err(anyhow!("unknown persisted trading status: {other}")),
    };
    let row_with_fees = |fee_tuple: &[FeeSpec; 3]| AllowedRow {
        symbol: symbol.to_owned(),
        listing_date: base.2.clone(),
        expiry_date: base.3.clone(),
        trading_status: trading_status.clone(),
        buy_margin_rate: base.6,
        sell_margin_rate: base.7,
        open_fee: fee_tuple[0].clone(),
        close_yesterday_fee: fee_tuple[1].clone(),
        close_today_fee: fee_tuple[2].clone(),
        lot_size: base.4,
        tick_size: base.5,
        source_updated_at: Some(effective_at.to_owned()),
        is_main_contract: base.12 != 0,
    };

    if same_fee_rules(&current_fees, fees) {
        let version_count: i64 = tx.query_row(
            "select count(*) from fee_versions where contract_id = ?1",
            [base.1],
            |row| row.get(0),
        )?;
        if version_count != 1 {
            return Err(anyhow!(
                "premature official transition repair requires one baseline version: {symbol}"
            ));
        }
        let previous = row_with_fees(previous_fees);
        tx.execute(
            "update fee_versions
             set rule_hash = ?1, open_fee_json = ?2,
                 close_yesterday_fee_json = ?3, close_today_fee_json = ?4,
                 source_kind = 'official', last_seen_at = ?5
             where id = ?6",
            params![
                row_rule_hash(&previous),
                serde_json::to_string(&previous.open_fee)?,
                serde_json::to_string(&previous.close_yesterday_fee)?,
                serde_json::to_string(&previous.close_today_fee)?,
                observed_at,
                base.0,
            ],
        )?;
    } else if !same_fee_rules(&current_fees, previous_fees) {
        return Err(anyhow!(
            "open fee rule matches neither side of official transition: {symbol}"
        ));
    }

    tx.execute(
        "update fee_versions set valid_to = ?1, last_seen_at = ?2 where id = ?3",
        params![effective_at, observed_at, base.0],
    )?;
    let target = row_with_fees(fees);
    tx.execute(
        "insert into fee_versions(
            contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
            open_fee_json, close_yesterday_fee_json, close_today_fee_json,
            trading_status, is_main_contract, source_kind, source_updated_at,
            valid_from, valid_to, first_seen_at, last_seen_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'official', ?10, ?10, null, ?11, ?11)",
        params![
            base.1,
            row_rule_hash(&target),
            target.buy_margin_rate,
            target.sell_margin_rate,
            serde_json::to_string(&target.open_fee)?,
            serde_json::to_string(&target.close_yesterday_fee)?,
            serde_json::to_string(&target.close_today_fee)?,
            base.11,
            base.12,
            effective_at,
            observed_at,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

/// Apply an official first-listing fee tuple. An existing contract may only be
/// retimed when its current rule already equals the official tuple and it has
/// no earlier fee version. A missing contract inherits static metadata only
/// from a product whose existing metadata is unambiguous.
///
/// # Errors
///
/// Returns an error when the requested retime would rewrite established
/// history, or no unambiguous product metadata is available for an insertion.
pub fn apply_official_listed_contract_fee_tuple(
    conn: &mut Connection,
    symbol: &str,
    effective_at: &str,
    fees: &[FeeSpec; 3],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    let effective = parse_timestamp("official listing effective_at", effective_at)?;

    if let Some(current) = current_fee_rule(conn, symbol)? {
        if !same_fee_rules(&current.fees, fees) {
            return Err(anyhow!(
                "official listed contract fee tuple conflicts with approved rule: {symbol}"
            ));
        }
        if effective > current.valid_from_at {
            return apply_official_fee_tuple(conn, symbol, effective_at, fees, observed_at);
        }
        if effective < current.valid_from_at {
            let has_predecessor: bool = conn.query_row(
                "select exists(
                    select 1 from fee_versions v
                    join contracts c on c.id = v.contract_id
                    where c.symbol = ?1 and julianday(v.valid_from) < julianday(?2)
                )",
                params![symbol, current.valid_from_at.format(&Rfc3339)?],
                |row| row.get(0),
            )?;
            if has_predecessor {
                return Err(anyhow!(
                    "official listing may only retime the first fee version: {symbol}"
                ));
            }
        }
        let updated = conn.execute(
            "update fee_versions
             set valid_from = ?1, source_kind = 'official', source_updated_at = ?1,
                 last_seen_at = ?2
             where id = (
                 select v.id from fee_versions v
                 join contracts c on c.id = v.contract_id
                 where c.symbol = ?3 and julianday(v.valid_from) = julianday(?4)
                 order by v.id desc limit 1
             )",
            params![
                effective_at,
                observed_at,
                symbol,
                current.valid_from_at.format(&Rfc3339)?,
            ],
        )?;
        if updated != 1 {
            return Err(anyhow!(
                "official listing target disappeared while applying: {symbol}"
            ));
        }
        return Ok(());
    }

    let product = derive_underlying_symbol(symbol)?;
    let mut statement = conn.prepare("select symbol, lot_size, tick_size from contracts")?;
    let candidates = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, f64>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter_map(|(candidate, lot_size, tick_size)| {
            (derive_underlying_symbol(&candidate).ok().as_deref() == Some(product.as_str()))
                .then_some((lot_size, tick_size))
        })
        .collect::<Vec<_>>();
    drop(statement);
    let Some((lot_size, tick_size)) = candidates.first().copied() else {
        return Err(anyhow!(
            "official listed contract has no product metadata: {symbol}"
        ));
    };
    if candidates.iter().any(|candidate| {
        candidate.0.to_bits() != lot_size.to_bits() || candidate.1.to_bits() != tick_size.to_bits()
    }) {
        return Err(anyhow!(
            "official listed contract has ambiguous product metadata: {symbol}"
        ));
    }

    let row = AllowedRow {
        symbol: symbol.to_owned(),
        listing_date: Some(effective.date().to_string().replace('-', "")),
        expiry_date: None,
        trading_status: TradingStatus::Unknown,
        buy_margin_rate: None,
        sell_margin_rate: None,
        open_fee: fees[0].clone(),
        close_yesterday_fee: fees[1].clone(),
        close_today_fee: fees[2].clone(),
        lot_size,
        tick_size,
        source_updated_at: Some(effective_at.to_owned()),
        is_main_contract: false,
    };
    upsert_rows(conn, &[row], observed_at, IngestMode::Official)
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
    Official,
}

impl IngestMode {
    const fn source_kind(self) -> FeeSource {
        match self {
            Self::Live => FeeSource::NineQihuo,
            Self::Historical => FeeSource::Jin10,
            Self::V11Baseline => FeeSource::V11Baseline,
            Self::Official => FeeSource::Official,
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

fn fee_rule_as_of(
    conn: &Connection,
    symbol: &str,
    effective_at: &str,
) -> Result<Option<CurrentFeeRule>> {
    let raw = conn
        .query_row(
            "select v.valid_from, v.open_fee_json, v.close_yesterday_fee_json,
                    v.close_today_fee_json, v.source_updated_at
               from fee_versions v
               join contracts c on c.id = v.contract_id
              where c.symbol = ?1
                and julianday(v.valid_from) <= julianday(?2)
                and (v.valid_to is null or julianday(v.valid_to) > julianday(?2))
              order by v.valid_from desc, v.id desc
              limit 1",
            params![symbol, effective_at],
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
        valid_from_at: parse_timestamp("as-of valid_from", &valid_from)?,
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
    if !same_fee_rules(&candidate_fees, &current.fees)
        && live_candidate_safety_rejection(&current.fees, &candidate_fees).is_some()
    {
        return Ok(None);
    }
    Ok(Some(row.clone()))
}

fn same_fee_rules(left: &[FeeSpec; 3], right: &[FeeSpec; 3]) -> bool {
    left.iter()
        .zip(right)
        .all(|(left, right)| same_fee(left, right))
}

fn same_fee(left: &FeeSpec, right: &FeeSpec) -> bool {
    if is_semantically_zero_fee(left) && is_semantically_zero_fee(right) {
        return true;
    }
    left.kind == right.kind && left.value.map(f64::to_bits) == right.value.map(f64::to_bits)
}

fn is_semantically_zero_fee(fee: &FeeSpec) -> bool {
    fee.value == Some(0.0)
        && matches!(
            fee.kind,
            FeeKind::CnyPerLot | FeeKind::TurnoverRatePerTenThousand | FeeKind::Zero
        )
}

/// Return the reason an automatically ingested latest-table fee change needs
/// exchange-original evidence. 9qihuo and Jin10 are useful corroborators for
/// ordinary, same-type adjustments, but both are secondary sources and have
/// previously propagated the same display/type/column defects.
fn live_candidate_safety_rejection(
    incumbent: &[FeeSpec; 3],
    candidate: &[FeeSpec; 3],
) -> Option<&'static str> {
    if candidate
        .iter()
        .any(|fee| fee.kind == future_meta::model::FeeKind::Unknown || fee.value.is_none())
    {
        return Some("candidate has unknown or missing fee value");
    }
    if is_non_identity_fee_permutation(incumbent, candidate) {
        return Some("fee-field permutation requires official evidence");
    }
    if has_known_fixed_offset(candidate, incumbent) {
        return Some("fixed-fee offset requires official evidence");
    }

    for (incumbent, candidate) in incumbent.iter().zip(candidate) {
        let (Some(incumbent_value), Some(candidate_value)) = (incumbent.value, candidate.value)
        else {
            return Some("candidate has unknown or missing fee value");
        };
        if !incumbent_value.is_finite()
            || !candidate_value.is_finite()
            || incumbent_value < 0.0
            || candidate_value < 0.0
        {
            return Some("candidate has invalid fee value");
        }
        if is_zero_fee(incumbent) != is_zero_fee(candidate) {
            return Some("zero-fee transition requires official evidence");
        }
        if incumbent.kind != candidate.kind {
            return Some("fee type transition requires official evidence");
        }
        if !is_zero_fee(incumbent)
            && (candidate_value > incumbent_value * 2.0 || candidate_value * 2.0 < incumbent_value)
        {
            return Some("multi-fold fee change requires official evidence");
        }
    }
    None
}

fn is_non_identity_fee_permutation(incumbent: &[FeeSpec; 3], candidate: &[FeeSpec; 3]) -> bool {
    const NON_IDENTITY_PERMUTATIONS: [[usize; 3]; 5] =
        [[0, 2, 1], [1, 0, 2], [1, 2, 0], [2, 0, 1], [2, 1, 0]];
    NON_IDENTITY_PERMUTATIONS.into_iter().any(|permutation| {
        candidate
            .iter()
            .enumerate()
            .all(|(index, fee)| same_fee(fee, &incumbent[permutation[index]]))
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
    let mut missing_metadata_symbols = Vec::new();

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
            missing_metadata_symbols.push(row.symbol.clone());
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
        missing_metadata_symbols,
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

    // CZCE's legacy contract specification pages retain the corresponding
    // 20/1, 50/1, and 100/0.2 lot/tick pairs. These products no longer occur
    // in the current 9qihuo seed but are present in the reviewed official
    // CZCE daily-parameter history.
    for (product, historical) in [
        (
            "CZCE.JR",
            ContractStaticMetadata {
                lot_size: 20.0,
                tick_size: 1.0,
            },
        ),
        (
            "CZCE.LR",
            ContractStaticMetadata {
                lot_size: 20.0,
                tick_size: 1.0,
            },
        ),
        (
            "CZCE.PM",
            ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 1.0,
            },
        ),
        (
            "CZCE.RI",
            ContractStaticMetadata {
                lot_size: 20.0,
                tick_size: 1.0,
            },
        ),
        (
            "CZCE.ZC",
            ContractStaticMetadata {
                lot_size: 100.0,
                tick_size: 0.2,
            },
        ),
    ] {
        let candidates = metadata.entry(product.to_owned()).or_default();
        if !candidates.contains(&historical) {
            candidates.push(historical);
        }
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
    Official,
}

impl FeeSource {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NineQihuo => "9qihuo",
            Self::Jin10 => "jin10",
            Self::V11Baseline => "v11_baseline",
            Self::Official => "official",
        }
    }
}

fn parse_fee_source(value: &str) -> Result<FeeSource> {
    match value {
        "9qihuo" => Ok(FeeSource::NineQihuo),
        "jin10" => Ok(FeeSource::Jin10),
        "v11_baseline" => Ok(FeeSource::V11Baseline),
        "official" => Ok(FeeSource::Official),
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
            "select fv.id, fv.contract_id, fv.rule_hash, fv.valid_from, fv.valid_to,
                    c.listing_date
             from fee_versions fv
             join contracts c on c.id = fv.contract_id
             where c.listing_date is not null
             order by fv.contract_id, fv.valid_from",
        )?;
        statement
            .query_map([], |record| {
                Ok((
                    record.get::<_, i64>(0)?,
                    record.get::<_, i64>(1)?,
                    record.get::<_, String>(2)?,
                    record.get::<_, String>(3)?,
                    record.get::<_, Option<String>>(4)?,
                    record.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut deletions = Vec::new();
    let mut repairs = Vec::new();
    for (version_id, contract_id, rule_hash, valid_from, valid_to, listing_date) in candidates {
        let valid_from_at = parse_timestamp("valid_from", &valid_from)?;
        let listing_day_start = contract_listing_day_start(&listing_date)?;
        if valid_from_at >= listing_day_start {
            continue;
        }

        if let Some(valid_to) = valid_to.as_deref() {
            let valid_to_at = parse_timestamp("valid_to", valid_to)?;
            if valid_to_at <= listing_day_start {
                deletions.push((version_id, contract_id, valid_from, rule_hash));
                continue;
            }
        }

        let is_initial: bool = conn.query_row(
            "select not exists(
                select 1 from fee_versions
                where contract_id = ?1 and valid_from < ?2
                  and (valid_to is null or valid_to > ?3)
            )",
            params![contract_id, valid_from, listing_day_start.format(&Rfc3339)?],
            |record| record.get(0),
        )?;
        if !is_initial {
            return Err(anyhow!(
                "cannot safely repair fee version {version_id}: it is not the initial contract version"
            ));
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

        repairs.push((
            version_id,
            contract_id,
            valid_from,
            repaired_valid_from,
            rule_hash,
        ));
    }

    apply_fee_listing_repairs(conn, &deletions, &repairs)
}

type FeeVersionDeletion = (i64, i64, String, String);
type FeeVersionClamp = (i64, i64, String, String, String);

fn apply_fee_listing_repairs(
    conn: &Connection,
    deletions: &[FeeVersionDeletion],
    repairs: &[FeeVersionClamp],
) -> Result<()> {
    if deletions.is_empty() && repairs.is_empty() {
        return Ok(());
    }
    conn.execute_batch("begin immediate")?;
    for (version_id, contract_id, valid_from, rule_hash) in deletions {
        if let Err(err) = conn.execute(
            "delete from fee_version_evidence
             where contract_id = ?1 and valid_from = ?2 and rule_hash = ?3",
            params![contract_id, valid_from, rule_hash],
        ) {
            conn.execute_batch("rollback")?;
            return Err(err.into());
        }
        if let Err(err) = conn.execute("delete from fee_versions where id = ?1", [*version_id]) {
            conn.execute_batch("rollback")?;
            return Err(err.into());
        }
    }
    for (version_id, contract_id, old_valid_from, valid_from, rule_hash) in repairs {
        if let Err(err) = conn.execute(
            "update fee_version_evidence set valid_from = ?1
             where contract_id = ?2 and valid_from = ?3 and rule_hash = ?4",
            params![valid_from, contract_id, old_valid_from, rule_hash],
        ) {
            conn.execute_batch("rollback")?;
            return Err(err.into());
        }
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

    let contract_id = tx.query_row(
        "select id from contracts where symbol = ?1",
        params![row.symbol.as_str()],
        |record| record.get(0),
    )?;
    upsert_contract_spec(tx, contract_id, row, observed_at, mode)?;
    Ok(contract_id)
}

fn upsert_contract_spec(
    tx: &Transaction<'_>,
    contract_id: i64,
    row: &AllowedRow,
    observed_at: &str,
    mode: IngestMode,
) -> Result<()> {
    let (valid_from, valid_from_at) = row_valid_from(row, observed_at)?;
    let current = tx
        .query_row(
            "select id, lot_size, tick_size, valid_from
             from contract_spec_versions
             where contract_id = ?1 and valid_to is null",
            params![contract_id],
            |record| {
                Ok((
                    record.get::<_, i64>(0)?,
                    record.get::<_, f64>(1)?,
                    record.get::<_, f64>(2)?,
                    record.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;

    if let Some((id, lot_size, tick_size, current_valid_from)) = current {
        if same_number(lot_size, row.lot_size) && same_number(tick_size, row.tick_size) {
            tx.execute(
                "update contract_spec_versions
                 set last_seen_at = case
                   when julianday(?1) > julianday(last_seen_at) then ?1 else last_seen_at end
                 where id = ?2",
                params![observed_at, id],
            )?;
            return Ok(());
        }
        let current_from_at = parse_timestamp("contract spec valid_from", &current_valid_from)?;
        if valid_from_at < current_from_at {
            return Err(anyhow!(
                "out-of-order contract spec change for {} at {} before current {}",
                row.symbol,
                valid_from,
                current_valid_from
            ));
        }
        if valid_from_at == current_from_at {
            tx.execute(
                "update contract_spec_versions
                 set lot_size = ?1, tick_size = ?2, source_kind = ?3,
                     last_seen_at = ?4
                 where id = ?5",
                params![
                    row.lot_size,
                    row.tick_size,
                    mode.source_kind().as_str(),
                    observed_at,
                    id
                ],
            )?;
            return Ok(());
        }
        tx.execute(
            "update contract_spec_versions set valid_to = ?1, last_seen_at = ?2 where id = ?3",
            params![valid_from, observed_at, id],
        )?;
    }

    tx.execute(
        "insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (?1, ?2, ?3, ?4, null, ?5, null, ?6, ?6)",
        params![
            contract_id,
            row.lot_size,
            row.tick_size,
            valid_from,
            mode.source_kind().as_str(),
            observed_at
        ],
    )?;
    Ok(())
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
