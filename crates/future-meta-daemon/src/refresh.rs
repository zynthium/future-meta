//! Refresh orchestration.

use crate::db::{self, connect, ensure_schema, upsert_latest_rows};
use crate::hash::{rule_set_hash, source_probe_hash};
use crate::jin10::{parse_snapshots_with_candidates, range_url};
use crate::latest::{LATEST_TABLE_PROBE_KEY, parse_latest_html};
use crate::parse::AllowedRow;
use crate::source::{TOTAL_URL, fetch_text, http_client};
use anyhow::{Result, anyhow};
use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, ORIGIN, REFERER, USER_AGENT};
use serde::Serialize;
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
            let (compared, differences) = db::compare_fee_rows(&conn, &snapshot.snapshot.rows)?;
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
/// and version maintenance fail.
pub fn update_latest(db: &Path, _require_seed: bool) -> Result<()> {
    let mut conn = connect(db)?;
    ensure_schema(&conn)?;
    db::ensure_seeded(&conn)?;
    crate::baseline::ensure_v11_baseline(&conn)?;

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

    let observed_at = now_string()?;
    let rows_hash = rule_set_hash(&completion.rows);
    let probe_hash = source_probe_hash(TOTAL_URL, LATEST_TABLE_PROBE_KEY);
    if db::source_rule_set_hash(&conn, TOTAL_URL)?.as_deref() == Some(&rows_hash) {
        db::update_source_success(&conn, TOTAL_URL, &probe_hash, &rows_hash, &observed_at)?;
        eprintln!(
            "latest table unchanged: rows={} skipped_invalid_symbols={} skipped_missing_metadata={} url={}",
            completion.rows.len(),
            snapshot.skipped_invalid_symbols,
            completion.skipped_missing_metadata,
            TOTAL_URL
        );
        return Ok(());
    }

    let jin10_rows = recent_jin10_rows(&conn, OffsetDateTime::parse(&observed_at, &Rfc3339)?)?;
    let verified = db::cross_verify_latest_candidates(&conn, &completion.rows, &jin10_rows)?;
    if !verified.rejected.is_empty() {
        let symbols = verified
            .rejected
            .iter()
            .take(12)
            .map(|rejection| rejection.symbol.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let message = format!(
            "refused {} unconfirmed 9qihuo fee candidates; examples={symbols}",
            verified.rejected.len()
        );
        db::update_source_error(&conn, TOTAL_URL, &observed_at, &message)?;
        return Err(anyhow!(message));
    }

    let skipped_conflicting_csv_rows =
        upsert_latest_rows(&mut conn, &verified.accepted, &observed_at)?;
    db::mark_latest_contracts_seen(&mut conn, &completion.rows, &observed_at)?;
    db::update_source_success(&conn, TOTAL_URL, &probe_hash, &rows_hash, &observed_at)?;
    eprintln!(
        "latest table updated: rows={} unchanged_fee_rows={} jin10_confirmed_fee_changes={} skipped_invalid_symbols={} skipped_missing_metadata={} skipped_conflicting_csv_rows={} url={}",
        completion.rows.len(),
        verified.unchanged,
        verified.accepted.len(),
        snapshot.skipped_invalid_symbols,
        completion.skipped_missing_metadata,
        skipped_conflicting_csv_rows,
        TOTAL_URL
    );
    Ok(())
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
