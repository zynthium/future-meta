//! Refresh orchestration.

use crate::db::{self, connect, connect_readonly, ensure_schema};
use crate::jin10::{parse_snapshots_with_candidates, range_url};
use crate::latest::{LatestSnapshot, parse_latest_html};
use crate::parse::{AllowedRow, parse_csv};
use crate::source::{TOTAL_URL, discover_sources_from_html, fetch_text, http_client};
use anyhow::{Result, anyhow};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;
use time::format_description;
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration as TimeDuration, OffsetDateTime, UtcOffset};

const JIN10_REFERER: &str = "https://www.jin10.com/";
const JIN10_ORIGIN: &str = "https://www.jin10.com";
const JIN10_APP_ID: &str = "fiXF2nOnDycGutVA";

/// Refresh behavior controls.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefreshOptions {
    /// Re-apply source rows even when their parsed rule-set hash is unchanged.
    pub force_full: bool,
    /// Require an existing locally seeded history database before fetching.
    pub require_seed: bool,
}

/// Result of evaluating a third-party latest-table snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LatestUpdateOutcome {
    /// The snapshot introduced no eligible concrete-contract fee record.
    Noop,
    /// A snapshot requires retained official evidence before a separate review
    /// release may add an approved fee record.
    Deferred { reason: String },
}

/// Classify latest-table candidates without changing approved history.
#[must_use]
pub fn classify_latest_update(
    verification: &db::LatestCandidateVerification,
) -> LatestUpdateOutcome {
    if !verification.rejected.is_empty() {
        let symbols = verification
            .rejected
            .iter()
            .take(12)
            .map(|rejection| rejection.symbol.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return LatestUpdateOutcome::Deferred {
            reason: format!(
                "refused {} latest fee candidates before history write; candidates need Jin10 confirmation or staged official evidence; examples={symbols}",
                verification.rejected.len()
            ),
        };
    }

    if !verification.accepted.is_empty() {
        let symbols = verification
            .accepted
            .iter()
            .take(12)
            .map(|row| row.symbol.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return LatestUpdateOutcome::Deferred {
            reason: format!(
                "{} third-party fee changes require staged verified official evidence before history write; examples={symbols}",
                verification.accepted.len()
            ),
        };
    }

    if !verification.new_contracts.is_empty() {
        let symbols = verification
            .new_contracts
            .iter()
            .take(12)
            .map(|row| row.symbol.as_str())
            .collect::<Vec<_>>()
            .join(",");
        return LatestUpdateOutcome::Deferred {
            reason: format!(
                "{} new concrete contracts require staged official fee evidence before history write; examples={symbols}",
                verification.new_contracts.len()
            ),
        };
    }

    LatestUpdateOutcome::Noop
}

/// Result summary for one or more Jin10 source snapshot payloads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Jin10Backfill {
    /// Number of source dates imported.
    pub snapshots: usize,
    /// Number of completed fee rows imported.
    pub rows: usize,
    /// Number of non-futures source rows skipped with strict symbol parsing.
    pub skipped_invalid_symbols: usize,
}

/// Read-only comparison of Jin10 with the current production latest rules.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Jin10Validation {
    pub requested_from: String,
    pub requested_to: String,
    pub snapshots: usize,
    pub jin10_rows: usize,
    pub skipped_invalid_symbols: usize,
    pub skipped_missing_metadata: usize,
    pub production_contracts: i64,
    pub compared_rows: usize,
    pub mismatch_count: usize,
    pub differences: Vec<db::FeeRuleDifference>,
}

/// Read-only comparison of the 9qihuo latest table against the current
/// review baseline and same-day Jin10 observations.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LatestCandidateDiagnosis {
    pub observed_at: String,
    pub qihuo_rows: usize,
    pub skipped_invalid_symbols: usize,
    pub skipped_missing_metadata: usize,
    pub diagnostics: Vec<db::LatestCandidateDiagnostic>,
}

/// Reject third-party fee differences until an exact official adjustment has
/// already been applied to the local history. `9qihuo` and Jin10 agreement is
/// useful corroboration, but it is not authority to create a fee version.
///
/// # Errors
///
/// Returns an error whenever two third-party sources agree on at least one
/// changed fee tuple that is not yet represented by the approved history.
pub fn require_official_fee_change_admission(
    verification: &db::LatestCandidateVerification,
) -> Result<()> {
    if verification.accepted.is_empty() {
        return Ok(());
    }
    let symbols = verification
        .accepted
        .iter()
        .take(12)
        .map(|row| row.symbol.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Err(anyhow!(
        "{} third-party fee changes require staged verified official evidence before history write; examples={symbols}",
        verification.accepted.len()
    ))
}

/// Backfill one verified Jin10 JSON payload into an existing 9qihuo seed.
///
/// The operation refuses rows whose product static metadata cannot be verified
/// from the existing seed. It therefore never guesses lot size or tick size.
///
/// # Errors
///
/// Returns an error when the seed is unavailable, the payload is invalid, a
/// static-metadata check fails, or database version maintenance fails.
pub fn backfill_jin10_payload(db: &Path, payload: &str) -> Result<Jin10Backfill> {
    reject_jin10_backfill()?;
    let mut conn = connect(db)?;
    ensure_schema(&conn)?;
    db::ensure_seeded(&conn)?;
    let metadata = db::product_static_metadata_candidates(&conn)?;
    let snapshots = parse_snapshots_with_candidates(payload, &metadata)?;
    let skipped_missing_metadata = snapshots
        .iter()
        .map(|snapshot| snapshot.snapshot.skipped_missing_metadata)
        .sum::<usize>();
    let skipped_invalid_symbols = snapshots
        .iter()
        .map(|snapshot| snapshot.snapshot.skipped_invalid_symbols)
        .sum::<usize>();
    if skipped_missing_metadata > 0 {
        return Err(anyhow!(
            "Jin10 backfill refused {skipped_missing_metadata} rows without verified static metadata"
        ));
    }
    let rows = snapshots
        .iter()
        .map(|snapshot| snapshot.snapshot.rows.len())
        .sum::<usize>();
    if rows == 0 {
        return Err(anyhow!("Jin10 payload contains no completed fee rows"));
    }

    let mut last_observed_at_by_symbol = std::collections::BTreeMap::new();
    for snapshot in &snapshots {
        for row in &snapshot.snapshot.rows {
            last_observed_at_by_symbol
                .entry(row.symbol.clone())
                .and_modify(|last_observed_at: &mut String| {
                    if snapshot.observed_at > *last_observed_at {
                        last_observed_at.clone_from(&snapshot.observed_at);
                    }
                })
                .or_insert_with(|| snapshot.observed_at.clone());
        }
        db::backfill_allowed_rows(&mut conn, &snapshot.snapshot.rows, &snapshot.observed_at)?;
        db::record_jin10_source_snapshot(
            &conn,
            &snapshot.observed_at,
            snapshot.snapshot.rows.len(),
            snapshot.snapshot.skipped_invalid_symbols,
        )?;
    }
    db::close_historical_fee_versions(&mut conn, &last_observed_at_by_symbol)?;

    Ok(Jin10Backfill {
        snapshots: snapshots.len(),
        rows,
        skipped_invalid_symbols,
    })
}

/// Download and backfill an inclusive Jin10 history date range.
///
/// The range is requested in 31-day chunks to avoid unbounded source
/// responses. Every chunk is subject to the same static-metadata checks as
/// [`backfill_jin10_payload`].
///
/// # Errors
///
/// Returns an error when dates are invalid, the source cannot be fetched, a
/// response fails validation, or database version maintenance fails.
pub fn backfill_jin10(db: &Path, from: &str, to: &str) -> Result<Jin10Backfill> {
    reject_jin10_backfill()?;
    let client = jin10_client()?;
    let mut total = Jin10Backfill {
        snapshots: 0,
        rows: 0,
        skipped_invalid_symbols: 0,
    };

    for (range_start, range_end) in jin10_date_ranges(from, to)? {
        let url = range_url(&range_start, &range_end)?;
        let payload = fetch_text(&client, url.as_str())?;
        let result = backfill_jin10_payload(db, &payload)?;
        total.snapshots += result.snapshots;
        total.rows += result.rows;
        total.skipped_invalid_symbols += result.skipped_invalid_symbols;
        eprintln!(
            "Jin10 backfill range {range_start}..{range_end}: snapshots={} rows={} skipped_invalid_symbols={}",
            result.snapshots, result.rows, result.skipped_invalid_symbols
        );
    }

    Ok(total)
}

/// Fetch Jin10 snapshots and compare them with production without writing any
/// fee version or source state. This is the secondary-source audit path.
///
/// # Errors
///
/// Returns an error when the database, Jin10 request, or snapshot parsing fails.
pub fn validate_jin10(
    db: &Path,
    from: &str,
    to: &str,
    out: Option<&Path>,
) -> Result<Jin10Validation> {
    let conn = connect(db)?;
    ensure_schema(&conn)?;
    db::ensure_seeded(&conn)?;
    let metadata = db::product_static_metadata_candidates(&conn)?;
    let client = jin10_client()?;
    let mut validation = Jin10Validation {
        requested_from: from.to_owned(),
        requested_to: to.to_owned(),
        snapshots: 0,
        jin10_rows: 0,
        skipped_invalid_symbols: 0,
        skipped_missing_metadata: 0,
        production_contracts: db::history_counts(&conn)?.contracts,
        compared_rows: 0,
        mismatch_count: 0,
        differences: Vec::new(),
    };

    for (range_start, range_end) in jin10_date_ranges(from, to)? {
        let url = range_url(&range_start, &range_end)?;
        let payload = fetch_text(&client, url.as_str())?;
        for snapshot in parse_snapshots_with_candidates(&payload, &metadata)? {
            validation.snapshots += 1;
            validation.jin10_rows += snapshot.snapshot.rows.len();
            validation.skipped_invalid_symbols += snapshot.snapshot.skipped_invalid_symbols;
            validation.skipped_missing_metadata += snapshot.snapshot.skipped_missing_metadata;
            let (compared, differences) =
                db::compare_fee_rows_as_of(&conn, &snapshot.snapshot.rows, &snapshot.observed_at)?;
            validation.compared_rows += compared;
            validation.differences.extend(differences);
        }
    }
    validation.mismatch_count = validation.differences.len();
    if let Some(out) = out {
        std::fs::write(out, serde_json::to_vec_pretty(&validation)?)?;
    }
    Ok(validation)
}

/// Fetch latest third-party observations and write a read-only diagnostic
/// report. This command deliberately does not update source state, fee
/// history, or announcement state.
///
/// # Errors
///
/// Returns an error when the database is not a V11 seed, either source cannot
/// be fetched or parsed, candidate verification fails, or the report cannot be
/// written.
pub fn diagnose_latest(db_path: &Path, out: &Path) -> Result<LatestCandidateDiagnosis> {
    let conn = connect_readonly(db_path)?;
    db::ensure_seeded(&conn)?;
    crate::baseline::ensure_v11_baseline(&conn)?;

    let observed_at = now_string()?;
    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let snapshot = parse_latest_html(&html)?;
    if snapshot.rows.is_empty() {
        return Err(anyhow!(
            "latest total-page table returned no allowed rows: {TOTAL_URL}"
        ));
    }
    let completion = db::complete_latest_rows(&conn, &snapshot.rows)?;
    if completion.rows.is_empty() {
        return Err(anyhow!(
            "latest total-page rows could not be completed from seed metadata: parsed={} skipped_invalid_symbols={} skipped_missing_metadata={}",
            snapshot.rows.len(),
            snapshot.skipped_invalid_symbols,
            completion.skipped_missing_metadata
        ));
    }
    let observed_at_parsed = OffsetDateTime::parse(&observed_at, &Rfc3339)?;
    let jin10_rows = recent_jin10_rows(&conn, observed_at_parsed)?;
    let diagnostics =
        db::diagnose_rejected_latest_candidates(&conn, &completion.rows, &jin10_rows)?;
    let result = LatestCandidateDiagnosis {
        observed_at,
        qihuo_rows: completion.rows.len(),
        skipped_invalid_symbols: snapshot.skipped_invalid_symbols,
        skipped_missing_metadata: completion.skipped_missing_metadata,
        diagnostics,
    };
    std::fs::write(out, serde_json::to_vec_pretty(&result)?)?;
    Ok(result)
}

fn reject_jin10_backfill() -> Result<()> {
    Err(anyhow!(
        "Jin10 historical backfill is retired and cannot be used"
    ))
}

fn jin10_client() -> Result<reqwest::blocking::Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        ACCEPT,
        HeaderValue::from_static("application/json, text/plain, */*"),
    );
    headers.insert(ORIGIN, HeaderValue::from_static(JIN10_ORIGIN));
    headers.insert(REFERER, HeaderValue::from_static(JIN10_REFERER));
    headers.insert(
        USER_AGENT,
        HeaderValue::from_static(
            "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/125.0 Safari/537.36",
        ),
    );
    headers.insert(
        HeaderName::from_static("x-app-id"),
        HeaderValue::from_static(JIN10_APP_ID),
    );
    headers.insert(
        HeaderName::from_static("x-version"),
        HeaderValue::from_static("1.0"),
    );

    reqwest::blocking::Client::builder()
        .default_headers(headers)
        .timeout(Duration::from_secs(90))
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(Into::into)
}

fn jin10_date_ranges(from: &str, to: &str) -> Result<Vec<(String, String)>> {
    let format = format_description::parse("[year]-[month]-[day]")?;
    let mut start = Date::parse(from, &format)
        .map_err(|err| anyhow!("invalid Jin10 start date {from}: {err}"))?;
    let end =
        Date::parse(to, &format).map_err(|err| anyhow!("invalid Jin10 end date {to}: {err}"))?;
    if start > end {
        return Err(anyhow!("Jin10 start date must not be after end date"));
    }

    let mut ranges = Vec::new();
    while start <= end {
        let range_end = (start + TimeDuration::days(30)).min(end);
        ranges.push((start.format(&format)?, range_end.format(&format)?));
        start = range_end + TimeDuration::days(1);
    }
    Ok(ranges)
}

/// Refresh fee history data.
///
/// # Errors
///
/// Returns an error if refresh fails.
pub fn refresh(db: &Path, force_full: bool) -> Result<()> {
    refresh_with_options(
        db,
        RefreshOptions {
            force_full,
            require_seed: false,
        },
    )
}

/// Reject the retired 9qihuo single-variety CSV history path.
///
/// # Errors
///
///
/// Always returns an error. Historical facts must be staged from exchange
/// originals in the isolated official-evidence database instead.
pub fn refresh_with_options(_db: &Path, _options: RefreshOptions) -> Result<()> {
    Err(anyhow!(
        "9qihuo single-variety CSV history refresh is retired; stage exchange originals with stage-official"
    ))
}

/// Update from the latest all-contract table on the total page.
///
/// # Errors
///
/// Returns an error if the seed is missing, latest page fetch fails, or parsing
/// and version maintenance fail. Unconfirmed 9qihuo fee candidates are retained
/// as a source error and omitted without preventing a no-change baseline export.
pub fn update_latest(db: &Path, _require_seed: bool) -> Result<LatestUpdateOutcome> {
    if !db.is_file() {
        return Err(anyhow!("seeded daemon database required: {}", db.display()));
    }
    let conn = connect_readonly(db)?;
    db::ensure_seeded(&conn)?;
    crate::baseline::ensure_v11_baseline(&conn)?;

    let observed_at = now_string()?;
    let announcement_health = db::announcement_health(&conn, &observed_at)?;
    eprintln!(
        "announcement health accepted: fresh_sources={} pending_candidates={}",
        announcement_health.fresh_sources.join(","),
        announcement_health.pending_candidates,
    );

    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let snapshot = parse_latest_html(&html)?;
    if snapshot.rows.is_empty() {
        return Err(anyhow!(
            "latest total-page table returned no allowed rows: {TOTAL_URL}"
        ));
    }
    let (completion, cached_jin10_rows) =
        complete_latest_snapshot(&conn, &client, &html, &snapshot, &observed_at)?;
    db::require_complete_latest_metadata(&completion)?;
    if completion.rows.is_empty() {
        return Err(anyhow!(
            "latest total-page rows could not be completed from seed metadata: parsed={} skipped_invalid_symbols={} skipped_missing_metadata={}",
            snapshot.rows.len(),
            snapshot.skipped_invalid_symbols,
            completion.skipped_missing_metadata
        ));
    }

    let jin10_rows = match cached_jin10_rows {
        Some(rows) => rows,
        None => recent_jin10_rows(&conn, OffsetDateTime::parse(&observed_at, &Rfc3339)?)?,
    };
    let verified = db::cross_verify_latest_candidates(&conn, &completion.rows, &jin10_rows)?;
    let outcome = classify_latest_update(&verified);
    match &outcome {
        LatestUpdateOutcome::Noop => eprintln!(
            "latest table has no eligible concrete-contract fee records: rows={} unchanged_fee_rows={} skipped_invalid_symbols={} skipped_missing_metadata={} url={}",
            completion.rows.len(),
            verified.unchanged,
            snapshot.skipped_invalid_symbols,
            completion.skipped_missing_metadata,
            TOTAL_URL
        ),
        LatestUpdateOutcome::Deferred { reason } => {
            return Err(anyhow!("{reason}; no fee history changes were applied"));
        }
    }
    Ok(outcome)
}

fn complete_latest_snapshot(
    conn: &rusqlite::Connection,
    client: &reqwest::blocking::Client,
    html: &str,
    snapshot: &LatestSnapshot,
    observed_at: &str,
) -> Result<(db::LatestCompletion, Option<Vec<AllowedRow>>)> {
    let completion = db::complete_latest_rows(conn, &snapshot.rows)?;
    if completion.missing_metadata_symbols.is_empty() {
        return Ok((completion, None));
    }

    let missing = completion
        .missing_metadata_symbols
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    let missing_latest = snapshot
        .rows
        .iter()
        .filter(|row| missing.contains(&row.symbol))
        .cloned()
        .collect::<Vec<_>>();
    let csv_rows = fetch_missing_contract_csv_rows(client, html, &missing)?;
    let jin10_rows = recent_jin10_rows(conn, OffsetDateTime::parse(observed_at, &Rfc3339)?)?;
    let corroborated =
        db::corroborate_new_contract_metadata(&missing_latest, &csv_rows, &jin10_rows)?
            .into_iter()
            .map(|row| (row.symbol.clone(), row))
            .collect::<BTreeMap<_, _>>();
    let enriched_rows = snapshot
        .rows
        .iter()
        .map(|row| {
            corroborated
                .get(&row.symbol)
                .cloned()
                .unwrap_or_else(|| row.clone())
        })
        .collect::<Vec<_>>();
    let completion = db::complete_latest_rows(conn, &enriched_rows)?;
    Ok((completion, Some(jin10_rows)))
}

fn fetch_missing_contract_csv_rows(
    client: &reqwest::blocking::Client,
    total_html: &str,
    missing_symbols: &BTreeSet<String>,
) -> Result<Vec<AllowedRow>> {
    let sources = discover_sources_from_html(total_html)?;
    let mut sources_by_code = BTreeMap::new();
    for source in sources {
        sources_by_code.insert(source.heyue.to_ascii_lowercase(), source);
    }

    let mut missing_by_product = BTreeMap::<String, BTreeSet<String>>::new();
    for symbol in missing_symbols {
        missing_by_product
            .entry(derive_underlying_symbol(symbol)?)
            .or_default()
            .insert(symbol.clone());
    }

    let mut evidence = Vec::new();
    for (product, symbols) in missing_by_product {
        let (_, local) = product
            .split_once('.')
            .ok_or_else(|| anyhow!("invalid product symbol {product}"))?;
        let source = sources_by_code
            .get(&local.to_ascii_lowercase())
            .ok_or_else(|| {
                anyhow!("no 9qihuo product CSV source for new contracts in {product}")
            })?;
        let csv = fetch_text(client, &source.csv_url)?;
        let rows = parse_csv(&csv)?;
        for symbol in symbols {
            let row = rows
                .iter()
                .find(|row| row.symbol == symbol)
                .ok_or_else(|| anyhow!("new contract {symbol} missing from {}", source.csv_url))?;
            evidence.push(row.clone());
        }
    }
    Ok(evidence)
}

/// Fetch recent Jin10 snapshots whose next-day effective dates can corroborate
/// a current 9qihuo table change. The bounded window intentionally fails
/// closed for stale 9qihuo timestamps instead of treating an old Jin10 value
/// as confirmation of a new fee rule.
fn recent_jin10_rows(
    conn: &rusqlite::Connection,
    observed_at: OffsetDateTime,
) -> Result<Vec<AllowedRow>> {
    let offset = UtcOffset::from_hms(8, 0, 0).expect("valid China Standard Time offset");
    let latest_day = observed_at.to_offset(offset).date();
    let source_start = latest_day - TimeDuration::days(8);
    let source_end = latest_day - TimeDuration::days(1);
    let format = format_description::parse("[year]-[month]-[day]")?;
    let from = source_start.format(&format)?;
    let to = source_end.format(&format)?;
    let metadata = db::product_static_metadata_candidates(conn)?;
    let client = jin10_client()?;
    let mut rows = Vec::new();

    for (range_start, range_end) in jin10_date_ranges(&from, &to)? {
        let url = range_url(&range_start, &range_end)?;
        let payload = fetch_text(&client, url.as_str())?;
        for snapshot in parse_snapshots_with_candidates(&payload, &metadata)? {
            rows.extend(snapshot.snapshot.rows);
        }
    }
    if rows.is_empty() {
        return Err(anyhow!(
            "Jin10 returned no rows for latest-source verification"
        ));
    }
    Ok(rows)
}

fn now_string() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}
