//! `SQLite` schema and version maintenance.

use crate::announcement::{AnnouncementCandidate, AnnouncementDocument, AnnouncementSource};
use crate::hash::row_rule_hash;
use crate::model::{AllowedRow, ContractStaticMetadata};
use anyhow::{Result, anyhow};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};

/// Minimal persisted history counts for inspection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HistoryCounts {
    pub contracts: i64,
    pub fee_versions: i64,
}

/// Open a writable history database.
///
/// # Errors
/// Returns an error when the database cannot be opened.
pub fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = non_empty_parent(path) {
        std::fs::create_dir_all(parent)?;
    }

    let conn = Connection::open(path)?;
    conn.execute_batch("pragma foreign_keys = on;")?;
    Ok(conn)
}

/// Open an existing daemon history database without permitting mutations.
///
/// # Errors
///
/// Returns an error when the database cannot be opened or configured.
pub fn connect_readonly(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.execute_batch("pragma foreign_keys = on;")?;
    Ok(conn)
}

/// Validate fee-history invariants required for a publishable archive.
///
/// # Errors
///
/// Returns an error when fee evidence cannot be linked to a fee version, an
/// observation timestamp is reversed, or persisted intervals overlap.
pub fn validate_fee_history_integrity(conn: &Connection) -> Result<()> {
    let orphan_evidence: i64 = conn.query_row(
        "select count(*)
         from fee_version_evidence evidence
         where not exists (
           select 1 from fee_versions version
           where version.contract_id = evidence.contract_id
             and version.valid_from = evidence.valid_from
             and version.rule_hash = evidence.rule_hash
         )",
        [],
        |row| row.get(0),
    )?;
    if orphan_evidence > 0 {
        return Err(anyhow!("orphan fee-version evidence: {orphan_evidence}"));
    }

    let missing_evidence: i64 = conn.query_row(
        "select count(*) from fee_versions version
         where not exists (
           select 1 from fee_version_evidence evidence
           where evidence.contract_id = version.contract_id
             and evidence.valid_from = version.valid_from
             and evidence.rule_hash = version.rule_hash
         )",
        [],
        |row| row.get(0),
    )?;
    if missing_evidence > 0 {
        return Err(anyhow!(
            "fee versions missing official evidence: {missing_evidence}"
        ));
    }

    let mut intervals = collect_fee_history_intervals(conn)?;

    if intervals.reverse_observations > 0 {
        return Err(anyhow!(
            "reverse fee observation times: {}",
            intervals.reverse_observations
        ));
    }

    if intervals.prelisting_versions > 0 {
        return Err(anyhow!(
            "fee versions before contract listing: {}",
            intervals.prelisting_versions
        ));
    }
    if intervals.invalid_intervals > 0 {
        return Err(anyhow!(
            "invalid fee intervals: {}",
            intervals.invalid_intervals
        ));
    }

    let mut open_before_later = 0usize;
    let mut overlaps = 0usize;
    for contract_intervals in intervals.by_contract.values_mut() {
        contract_intervals.sort_unstable_by_key(|(valid_from, _)| *valid_from);
        for pair in contract_intervals.windows(2) {
            let (_, earlier_to) = &pair[0];
            let (later_from, _) = &pair[1];
            match earlier_to {
                None => open_before_later += 1,
                Some(valid_to) if *valid_to > *later_from => overlaps += 1,
                Some(_) => {}
            }
        }
    }
    if open_before_later > 0 {
        return Err(anyhow!(
            "open fee intervals before later versions: {open_before_later}"
        ));
    }
    if overlaps > 0 {
        return Err(anyhow!("overlapping fee intervals: {overlaps}"));
    }

    Ok(())
}

#[derive(Default)]
struct FeeHistoryIntervals {
    by_contract: BTreeMap<i64, Vec<(OffsetDateTime, Option<OffsetDateTime>)>>,
    reverse_observations: usize,
    prelisting_versions: usize,
    invalid_intervals: usize,
}

fn collect_fee_history_intervals(conn: &Connection) -> Result<FeeHistoryIntervals> {
    let mut intervals = FeeHistoryIntervals::default();
    let mut statement = conn.prepare(
        "select version.contract_id, version.valid_from, version.valid_to,
                version.first_seen_at, version.last_seen_at, contract.listing_date
         from fee_versions version
         join contracts contract on contract.id = version.contract_id",
    )?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let contract_id: i64 = row.get(0)?;
        let valid_from: String = row.get(1)?;
        let valid_from_at = parse_timestamp("fee valid_from", &valid_from)?;
        let valid_to: Option<String> = row.get(2)?;
        let valid_to_at = valid_to
            .as_deref()
            .map(|value| parse_timestamp("fee valid_to", value))
            .transpose()?;
        let first_seen_at: String = row.get(3)?;
        let first_seen_at = parse_timestamp("fee first_seen_at", &first_seen_at)?;
        let last_seen_at: String = row.get(4)?;
        let last_seen_at = parse_timestamp("fee last_seen_at", &last_seen_at)?;
        let listing_date: Option<String> = row.get(5)?;

        if first_seen_at > last_seen_at {
            intervals.reverse_observations += 1;
        }
        if valid_to_at.is_some_and(|valid_to| valid_to <= valid_from_at) {
            intervals.invalid_intervals += 1;
        }
        if listing_date
            .as_deref()
            .map(contract_listing_day_start)
            .transpose()?
            .is_some_and(|listing_at| valid_from_at < listing_at)
        {
            intervals.prelisting_versions += 1;
        }
        intervals
            .by_contract
            .entry(contract_id)
            .or_default()
            .push((valid_from_at, valid_to_at));
    }
    Ok(intervals)
}

/// Explicit, human-reviewed correction for one reversed observation interval.
///
/// This type is accepted only by the review-copy repair command. `rationale`
/// becomes part of the persistent repair audit trail.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewObservationTimeRepair {
    pub symbol: String,
    pub valid_from: String,
    pub last_seen_at: String,
    pub rationale: String,
}

/// Counts changed by one review-only fee-hash migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewFeeHistoryRekey {
    pub fee_versions: usize,
    pub evidence: usize,
}

#[derive(Debug)]
struct FeeHashRekey {
    version_id: i64,
    contract_id: i64,
    valid_from: String,
    old_rule_hash: String,
    new_rule_hash: String,
}

/// Rekey every fee version and its retained evidence using the fee-only rule
/// hash. This migration is review-copy-only: it records each changed version
/// and never drops retained evidence bytes.
///
/// # Errors
///
/// Returns an error if an evidence record cannot be associated with exactly
/// one concrete contract version on its effective timestamp, if timestamps are
/// malformed, or if the resulting history fails publication integrity checks.
#[allow(clippy::too_many_lines)]
pub fn rekey_fee_history_for_review(
    conn: &mut Connection,
    repaired_at: &str,
) -> Result<ReviewFeeHistoryRekey> {
    ensure_schema(conn)?;
    parse_timestamp("repaired_at", repaired_at)?;
    let tx = conn.transaction()?;
    let rekeys = {
        let mut statement = tx.prepare(
            "select v.id, v.contract_id, v.rule_hash, v.valid_from,
                    c.symbol, c.listing_date, c.expiry_date, c.lot_size, c.tick_size,
                    v.buy_margin_rate, v.sell_margin_rate,
                    v.open_fee_json, v.close_yesterday_fee_json, v.close_today_fee_json,
                    v.trading_status, v.source_updated_at, v.is_main_contract, v.intraday_open_fee_json
             from fee_versions v
             join contracts c on c.id = v.contract_id
             order by v.id",
        )?;
        let mut rows = statement.query([])?;
        let mut rekeys = Vec::new();
        while let Some(record) = rows.next()? {
            let row = AllowedRow {
                symbol: record.get(4)?,
                listing_date: record.get(5)?,
                expiry_date: record.get(6)?,
                trading_status: parse_trading_status_text(&record.get::<_, String>(14)?)?,
                buy_margin_rate: record.get(9)?,
                sell_margin_rate: record.get(10)?,
                open_fee: parse_fee_json(&record.get::<_, String>(11)?)?,
                close_yesterday_fee: parse_fee_json(&record.get::<_, String>(12)?)?,
                close_today_fee: parse_fee_json(&record.get::<_, String>(13)?)?,
                intraday_open_fee: record
                    .get::<_, Option<String>>(17)?
                    .map(|s| parse_fee_json(&s))
                    .transpose()?,
                lot_size: record.get(7)?,
                tick_size: record.get(8)?,
                source_updated_at: record.get(15)?,
                is_main_contract: record.get::<_, i64>(16)? != 0,
            };
            rekeys.push(FeeHashRekey {
                version_id: record.get(0)?,
                contract_id: record.get(1)?,
                valid_from: record.get(3)?,
                old_rule_hash: record.get(2)?,
                new_rule_hash: row_rule_hash(&row),
            });
        }
        rekeys
    };
    let rekeys_by_version = rekeys
        .iter()
        .map(|rekey| ((rekey.contract_id, rekey.valid_from.clone()), rekey))
        .collect::<BTreeMap<_, _>>();
    if rekeys_by_version.len() != rekeys.len() {
        return Err(anyhow!(
            "multiple fee versions share a contract effective timestamp"
        ));
    }

    let evidence = {
        let mut statement = tx.prepare(
            "select contract_id, valid_from, rule_hash, evidence_level,
                    canonical_url, body_sha256, recorded_at
             from fee_version_evidence
             order by contract_id, valid_from, rule_hash, canonical_url, body_sha256",
        )?;
        statement
            .query_map([], |record| {
                Ok((
                    record.get::<_, i64>(0)?,
                    record.get::<_, String>(1)?,
                    record.get::<_, String>(2)?,
                    record.get::<_, String>(3)?,
                    record.get::<_, String>(4)?,
                    record.get::<_, String>(5)?,
                    record.get::<_, String>(6)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut changed_evidence = 0usize;
    for (
        contract_id,
        valid_from,
        old_rule_hash,
        evidence_level,
        canonical_url,
        body_sha256,
        recorded_at,
    ) in evidence
    {
        let rekey = rekeys_by_version
            .get(&(contract_id, valid_from.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "review fee-hash rekey cannot associate evidence with a version: contract_id={contract_id} valid_from={valid_from}"
                )
            })?;
        if old_rule_hash == rekey.new_rule_hash {
            continue;
        }
        tx.execute(
            "insert or ignore into fee_version_evidence(
               contract_id, valid_from, rule_hash, evidence_level,
               canonical_url, body_sha256, recorded_at
             ) values(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                contract_id,
                valid_from,
                rekey.new_rule_hash,
                evidence_level,
                canonical_url,
                body_sha256,
                recorded_at,
            ],
        )?;
        // SQLite UPSERT cannot target views; this transaction also supports
        // the compact seed's writable evidence interface.
        if evidence_level == "paired_official" {
            tx.execute(
                "update fee_version_evidence set evidence_level='paired_official'
                 where contract_id=?1 and valid_from=?2 and rule_hash=?3
                   and canonical_url=?4 and body_sha256=?5",
                params![
                    contract_id,
                    valid_from,
                    rekey.new_rule_hash,
                    canonical_url,
                    body_sha256
                ],
            )?;
        }
        tx.execute(
            "delete from fee_version_evidence
             where contract_id = ?1 and valid_from = ?2 and rule_hash = ?3
               and canonical_url = ?4 and body_sha256 = ?5",
            params![
                contract_id,
                valid_from,
                old_rule_hash,
                canonical_url,
                body_sha256
            ],
        )?;
        changed_evidence += 1;
    }

    let mut changed_versions = 0usize;
    for rekey in &rekeys {
        if rekey.old_rule_hash == rekey.new_rule_hash {
            continue;
        }
        tx.execute(
            "update fee_versions set rule_hash = ?1 where id = ?2",
            params![rekey.new_rule_hash, rekey.version_id],
        )?;
        tx.execute(
            "insert into review_fee_history_repairs(
               repair_kind, contract_id, valid_from, old_rule_hash, new_rule_hash,
               old_first_seen_at, old_last_seen_at, new_last_seen_at, rationale, repaired_at
             ) values(
               'fee_hash_rekey', ?1, ?2, ?3, ?4, null, null, null, ?5, ?6
             )",
            params![
                rekey.contract_id,
                rekey.valid_from,
                rekey.old_rule_hash,
                rekey.new_rule_hash,
                "Canonical fee hash now contains only fee kind and numeric value.",
                repaired_at,
            ],
        )?;
        changed_versions += 1;
    }
    validate_fee_history_integrity(&tx)?;
    tx.commit()?;
    Ok(ReviewFeeHistoryRekey {
        fee_versions: changed_versions,
        evidence: changed_evidence,
    })
}

/// Apply explicitly reviewed corrections for all reversed observation times.
///
/// The input must cover every reversed record and may not contain unrelated
/// records. This prevents a timestamp repair from becoming a silent bulk edit.
///
/// # Errors
///
/// Returns an error when a correction is missing, duplicated, unrelated,
/// malformed, or predates its version's first observation.
#[allow(clippy::too_many_lines)]
pub fn repair_review_observation_times(
    conn: &mut Connection,
    repairs: &[ReviewObservationTimeRepair],
    repaired_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    parse_timestamp("repaired_at", repaired_at)?;
    let tx = conn.transaction()?;
    let reversed = {
        let mut statement = tx.prepare(
            "select v.id, v.contract_id, c.symbol, v.valid_from,
                    v.first_seen_at, v.last_seen_at
             from fee_versions v
             join contracts c on c.id = v.contract_id
             where julianday(v.first_seen_at) > julianday(v.last_seen_at)
             order by c.symbol, v.valid_from, v.id",
        )?;
        statement
            .query_map([], |record| {
                Ok((
                    record.get::<_, i64>(0)?,
                    record.get::<_, i64>(1)?,
                    record.get::<_, String>(2)?,
                    record.get::<_, String>(3)?,
                    record.get::<_, String>(4)?,
                    record.get::<_, String>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut repairs_by_key = BTreeMap::new();
    for repair in repairs {
        if repair.rationale.trim().is_empty() {
            return Err(anyhow!(
                "review observation-time repair requires a rationale: {} {}",
                repair.symbol,
                repair.valid_from
            ));
        }
        parse_timestamp("review repair last_seen_at", &repair.last_seen_at)?;
        let key = (repair.symbol.clone(), repair.valid_from.clone());
        if repairs_by_key.insert(key.clone(), repair).is_some() {
            return Err(anyhow!(
                "duplicate review observation-time repair: {} {}",
                key.0,
                key.1
            ));
        }
    }

    let repair_keys = repairs_by_key.keys().cloned().collect::<Vec<_>>();
    for (symbol, valid_from) in repair_keys {
        let repair = repairs_by_key
            .get(&(symbol.clone(), valid_from.clone()))
            .ok_or_else(|| {
                anyhow!(
                    "review observation-time repair disappeared before validation: {symbol} {valid_from}"
                )
            })?;
        let already_applied: bool = tx.query_row(
            "select exists(
               select 1
               from fee_versions version
               join contracts contract on contract.id = version.contract_id
               join review_fee_history_repairs audit
                 on audit.contract_id = version.contract_id
                and audit.valid_from = version.valid_from
              where audit.repair_kind = 'observation_time'
                and contract.symbol = ?1
                and version.valid_from = ?2
                and version.last_seen_at = ?3
                and audit.new_last_seen_at = ?3
                and audit.rationale = ?4
             )",
            params![symbol, valid_from, repair.last_seen_at, repair.rationale],
            |record| record.get(0),
        )?;
        if already_applied {
            repairs_by_key.remove(&(symbol, valid_from));
        }
    }

    let mut repaired = 0usize;
    for (version_id, contract_id, symbol, valid_from, first_seen_at, old_last_seen_at) in reversed {
        let repair = repairs_by_key
            .remove(&(symbol.clone(), valid_from.clone()))
            .ok_or_else(|| {
                anyhow!("missing review observation-time repair: {symbol} {valid_from}")
            })?;
        let first_seen = parse_timestamp("first_seen_at", &first_seen_at)?;
        let new_last_seen = parse_timestamp("review repair last_seen_at", &repair.last_seen_at)?;
        if new_last_seen < first_seen {
            return Err(anyhow!(
                "review observation-time repair remains reversed: {symbol} {valid_from}"
            ));
        }
        tx.execute(
            "update fee_versions set last_seen_at = ?1 where id = ?2",
            params![repair.last_seen_at, version_id],
        )?;
        tx.execute(
            "insert into review_fee_history_repairs(
               repair_kind, contract_id, valid_from, old_rule_hash, new_rule_hash,
               old_first_seen_at, old_last_seen_at, new_last_seen_at, rationale, repaired_at
             ) values(
               'observation_time', ?1, ?2, null, null, ?3, ?4, ?5, ?6, ?7
             )",
            params![
                contract_id,
                valid_from,
                first_seen_at,
                old_last_seen_at,
                repair.last_seen_at,
                repair.rationale,
                repaired_at,
            ],
        )?;
        repaired += 1;
    }
    if let Some(((symbol, valid_from), _)) = repairs_by_key.into_iter().next() {
        return Err(anyhow!(
            "review observation-time repair does not match a reversed record: {symbol} {valid_from}"
        ));
    }
    tx.commit()?;
    Ok(repaired)
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
        ) without rowid;
        create table if not exists review_fee_history_repairs(
            id integer primary key,
            repair_kind text not null check(repair_kind in ('fee_hash_rekey', 'observation_time')),
            contract_id integer not null,
            valid_from text not null,
            old_rule_hash text,
            new_rule_hash text,
            old_first_seen_at text,
            old_last_seen_at text,
            new_last_seen_at text,
            rationale text not null,
            repaired_at text not null,
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
        ) without rowid;
        create table if not exists contract_lifecycle_evidence(
            contract_id integer not null,
            listing_date text not null,
            expiry_date text not null,
            canonical_url text not null,
            body_sha256 text not null check(length(body_sha256) = 64),
            recorded_at text not null,
            primary key(contract_id, canonical_url, body_sha256),
            foreign key(contract_id) references contracts(id)
        ) without rowid;
        ",
    )?;

    conn.execute_batch("drop index if exists idx_fee_versions_contract;")?;

    ensure_fee_version_source_kind_column(conn)?;
    if !conn.query_row("select exists(select 1 from pragma_table_info('contract_lifecycle_evidence') where name='provenance_kind')", [], |row| row.get::<_, bool>(0))? {
        conn.execute("alter table contract_lifecycle_evidence add column provenance_kind text not null default 'official'", [])?;
    }

    if !conn
        .prepare("pragma table_info(fee_versions)")?
        .query_map([], |r| r.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "intraday_open_fee_json")
    {
        conn.execute("alter table fee_versions add column intraday_open_fee_json text check(intraday_open_fee_json is null or json_valid(intraday_open_fee_json))", [])?;
    }

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

/// Resolve pending option-fee-only notices, which are outside this archive.
///
/// # Errors
///
/// Returns an error when candidate state cannot be updated.
pub fn resolve_out_of_scope_option_fee_candidates(
    conn: &Connection,
    resolved_at: &str,
) -> Result<usize> {
    ensure_schema(conn)?;
    let resolved = conn.execute(
        "update announcement_candidates
         set resolved_at = ?1
         where resolved_at is null
           and exists (
             select 1 from announcement_documents document
             where document.source = announcement_candidates.source
               and document.article_id = announcement_candidates.article_id
               and document.title like '%期权%'
               and document.title not like '%期货手续费%'
           )
           and exists (
             select 1 from announcement_document_snapshots snapshot
             where snapshot.source = announcement_candidates.source
               and snapshot.article_id = announcement_candidates.article_id
               and snapshot.fetched_at = (
                 select max(latest.fetched_at)
                 from announcement_document_snapshots latest
                 where latest.source = snapshot.source
                   and latest.article_id = snapshot.article_id
               )
               and (snapshot.body_text like '%期权交易手续费%'
                 or snapshot.body_text like '%期权手续费%')
               and snapshot.body_text not like '%期货交易手续费%'
               and snapshot.body_text not like '%期货手续费%'
           )",
        params![resolved_at],
    )?;
    Ok(resolved)
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
                || upsert_official_contract(&tx, &latest_row, observed_at),
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
        let existing_paired_keys = {
            let mut statement = tx.prepare(
                "select evidence.valid_from, evidence.rule_hash
                 from fee_version_evidence evidence
                 join fee_versions version
                   on version.contract_id = evidence.contract_id
                  and version.valid_from = evidence.valid_from
                  and version.rule_hash = evidence.rule_hash
                 where evidence.contract_id = ?1
                   and evidence.evidence_level = 'paired_official'",
            )?;
            statement
                .query_map([contract_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        let existing_evidence_keys = {
            let mut statement = tx.prepare(
                "select valid_from, rule_hash from fee_version_evidence where contract_id = ?1",
            )?;
            statement
                .query_map([contract_id], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<BTreeSet<_>>>()?
        };
        let existing = load_existing_versions(&tx, contract_id)?;
        let paired_rules = existing
            .iter()
            .filter(|version| {
                existing_paired_keys
                    .contains(&(version.valid_from.clone(), version.rule_hash.clone()))
            })
            .map(|version| {
                (
                    version.valid_from_at,
                    (version.valid_from.clone(), version.rule_hash.clone()),
                )
            })
            .collect::<BTreeMap<_, _>>();

        let mut versions = existing
            .into_iter()
            .filter(|version| {
                let is_parameter = existing_parameter_keys
                    .contains(&(version.valid_from.clone(), version.rule_hash.clone()));
                if is_parameter {
                    return false;
                }
                existing_paired_keys
                    .contains(&(version.valid_from.clone(), version.rule_hash.clone()))
                    || version.valid_from_at < first_at
                    || version.valid_from_at >= coverage_end_at
            })
            .collect::<Vec<_>>();
        let mut parameter_keys = BTreeSet::new();
        let mut evidence = Vec::new();
        for (source, prepared, _) in items.iter() {
            if let Some((existing_valid_from, existing_rule_hash)) =
                paired_rules.get(&prepared.valid_from_at)
            {
                if existing_rule_hash != &prepared.rule_hash {
                    return Err(anyhow!(
                        "paired official fee tuple conflicts with retained version {} at {}",
                        prepared.row.symbol,
                        prepared.valid_from
                    ));
                }
                evidence.push((
                    (existing_valid_from.clone(), existing_rule_hash.clone()),
                    source.evidence_level,
                    source.evidence.clone(),
                ));
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
            let current_key = (version.valid_from.clone(), version.rule_hash.clone());
            if existing_evidence_keys.contains(&current_key) {
                rebuilt.push(version);
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
                    "insert or ignore into fee_version_evidence(
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
                    v.is_main_contract, v.valid_from, v.intraday_open_fee_json
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
                    row.get::<_, Option<String>>(9)?,
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
        intraday_open_fee: base.9.as_deref().map(parse_fee_json).transpose()?,
        lot_size: base.2,
        tick_size: base.3,
        source_updated_at: Some(effective_at.to_owned()),
        is_main_contract: base.7 != 0,
    };
    upsert_official_rows(conn, &[row], observed_at)
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
                    v.trading_status, v.is_main_contract, v.valid_from, v.intraday_open_fee_json
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
                    row.get::<_, Option<String>>(14)?,
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
    let intraday_open_fee = base.14.as_deref().map(parse_fee_json).transpose()?;
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
        intraday_open_fee: intraday_open_fee.clone(),
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
        intraday_open_fee: None,
        lot_size,
        tick_size,
        source_updated_at: Some(effective_at.to_owned()),
        is_main_contract: false,
    };
    upsert_official_rows(conn, &[row], observed_at)
}

fn upsert_official_rows(
    conn: &mut Connection,
    rows: &[AllowedRow],
    observed_at: &str,
) -> Result<()> {
    ensure_schema(conn)?;
    parse_timestamp("observed_at", observed_at)?;
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

        let contract_id = upsert_official_contract(&tx, &latest_row, observed_at)?;

        let mut versions = load_existing_versions(&tx, contract_id)?;
        versions.extend(rows.into_iter().map(|prepared| VersionRecord {
            row: prepared.row,
            rule_hash: prepared.rule_hash,
            valid_from: prepared.valid_from,
            valid_from_at: prepared.valid_from_at,
            first_seen_at: observed_at.to_owned(),
            last_seen_at: observed_at.to_owned(),
            source_kind: FeeSource::Official,
        }));

        let versions = merge_versions(versions)?;
        replace_fee_versions(&tx, contract_id, &versions)?;
    }

    tx.commit()?;
    Ok(())
}

#[derive(Debug)]
struct CurrentFeeRule {
    valid_from_at: OffsetDateTime,
    fees: [FeeSpec; 3],
}

fn current_fee_rule(conn: &Connection, symbol: &str) -> Result<Option<CurrentFeeRule>> {
    let raw = conn
        .query_row(
            "select v.valid_from,v.open_fee_json,v.close_yesterday_fee_json,v.close_today_fee_json
         from fee_versions v join contracts c on c.id=v.contract_id
         where c.symbol=?1 order by julianday(v.valid_from) desc,v.id desc limit 1",
            [symbol],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(from, open, close, today)| {
        Ok(CurrentFeeRule {
            valid_from_at: parse_timestamp("current valid_from", &from)?,
            fees: [
                parse_fee_json(&open)?,
                parse_fee_json(&close)?,
                parse_fee_json(&today)?,
            ],
        })
    })
    .transpose()
}

fn same_number(left: f64, right: f64) -> bool {
    (left - right).abs() <= left.abs().max(right.abs()).max(1.0) * 1e-9
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
    // on 2022-12-01: 20 tonnes/lot and 1 yuan/tonne. WH is absent from the current catalogue but occurs
    // in retained official historical parameters.
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
    // in the current catalogue but are present in the reviewed official
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
    // to 20 on 2024-12-17. Current contracts contain newer ticks while the
    // retained official history starts before both effective dates.
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
                v.valid_from, v.first_seen_at, v.last_seen_at, v.intraday_open_fee_json
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
                intraday_open_fee: record
                    .get::<_, Option<String>>(18)?
                    .map(|v| parse_fee_json(&v))
                    .transpose()?,
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
            && same_fee_terms(&last.row, &version.row)
        {
            merge_equivalent_version(last, version)?;
            continue;
        }

        collapsed.push(version);
    }

    Ok(collapsed)
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
        && left.intraday_open_fee == right.intraday_open_fee
        && left.lot_size.to_bits() == right.lot_size.to_bits()
        && left.tick_size.to_bits() == right.tick_size.to_bits()
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

fn upsert_official_contract(
    tx: &Transaction<'_>,
    row: &AllowedRow,
    observed_at: &str,
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
            0,
        ],
    )?;

    let contract_id = tx.query_row(
        "select id from contracts where symbol = ?1",
        params![row.symbol.as_str()],
        |record| record.get(0),
    )?;
    upsert_official_contract_spec(tx, contract_id, row, observed_at)?;
    Ok(contract_id)
}

fn upsert_official_contract_spec(
    tx: &Transaction<'_>,
    contract_id: i64,
    row: &AllowedRow,
    observed_at: &str,
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
                params![row.lot_size, row.tick_size, "official", observed_at, id],
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
            "official",
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
           valid_from, valid_to, first_seen_at, last_seen_at, intraday_open_fee_json
         )
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
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
            row.intraday_open_fee
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
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
