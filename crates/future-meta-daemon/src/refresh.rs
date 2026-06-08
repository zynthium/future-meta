//! Refresh orchestration.

use crate::db::{self, connect, ensure_schema, upsert_allowed_rows};
use crate::hash::{rule_set_hash, source_probe_hash};
use crate::latest::{LATEST_TABLE_PROBE_KEY, parse_latest_html};
use crate::parse::parse_csv;
use crate::source::{TOTAL_URL, discover_sources_from_html, fetch_text, http_client};
use anyhow::{Result, anyhow};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Refresh behavior controls.
#[derive(Debug, Clone, Copy, Default)]
pub struct RefreshOptions {
    /// Re-apply source rows even when their parsed rule-set hash is unchanged.
    pub force_full: bool,
    /// Require an existing locally seeded history database before fetching.
    pub require_seed: bool,
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

/// Refresh fee history data with explicit safety options.
///
/// # Errors
///
/// Returns an error if refresh fails or a required seed is missing.
pub fn refresh_with_options(db: &Path, options: RefreshOptions) -> Result<()> {
    let mut conn = connect(db)?;
    ensure_schema(&conn)?;
    if options.require_seed {
        db::ensure_seeded(&conn)?;
    }

    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let sources = discover_sources_from_html(&html)?;
    let observed_at = now_string()?;
    let mut attempted = 0usize;
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for source in sources {
        let probe_hash = source_probe_hash(&source.csv_url, &source.detail_url);
        attempted += 1;
        eprintln!(
            "refreshing source {attempted}: heyue={} url={}",
            source.heyue, source.csv_url
        );

        match refresh_source(
            &mut conn,
            &client,
            &source,
            &probe_hash,
            &observed_at,
            options.force_full,
        ) {
            Ok(RefreshedSource::UpdatedOrEmpty) => {
                succeeded += 1;
                eprintln!("source refresh succeeded for {}", source.csv_url);
            }
            Ok(RefreshedSource::Unchanged) => {
                succeeded += 1;
                eprintln!("source refresh unchanged for {}", source.csv_url);
            }
            Ok(RefreshedSource::SkippedEmpty) => {}
            Err(err) => {
                failed += 1;
                eprintln!("source refresh failed for {}: {err}", source.csv_url);
                db::update_source_error(&conn, &source.csv_url, &observed_at, &err.to_string())?;
            }
        }
    }

    if attempted > 0 && succeeded == 0 && failed > 0 {
        return Err(anyhow!("all attempted source refreshes failed"));
    }

    Ok(())
}

/// Update from the latest all-contract table on the total page.
///
/// # Errors
///
/// Returns an error if the seed is missing, latest page fetch fails, or parsing
/// and version maintenance fail.
pub fn update_latest(db: &Path, require_seed: bool) -> Result<()> {
    let mut conn = connect(db)?;
    ensure_schema(&conn)?;
    if require_seed {
        db::ensure_seeded(&conn)?;
    }

    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let snapshot = parse_latest_html(&html)?;
    if snapshot.rows.is_empty() {
        return Err(anyhow!(
            "latest total-page table returned no allowed rows: {}",
            TOTAL_URL
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

    upsert_allowed_rows(&mut conn, &completion.rows, &observed_at)?;
    db::update_source_success(&conn, TOTAL_URL, &probe_hash, &rows_hash, &observed_at)?;
    eprintln!(
        "latest table updated: rows={} skipped_invalid_symbols={} skipped_missing_metadata={} url={}",
        completion.rows.len(),
        snapshot.skipped_invalid_symbols,
        completion.skipped_missing_metadata,
        TOTAL_URL
    );
    Ok(())
}

enum RefreshedSource {
    UpdatedOrEmpty,
    Unchanged,
    SkippedEmpty,
}

fn refresh_source(
    conn: &mut rusqlite::Connection,
    client: &reqwest::blocking::Client,
    source: &crate::source::SourceEntry,
    probe_hash: &str,
    observed_at: &str,
    force_full: bool,
) -> Result<RefreshedSource> {
    let csv = fetch_text(client, source.csv_url.as_str())?;
    let rows = parse_csv(&csv)?;
    if rows.is_empty() && !force_full {
        return Ok(RefreshedSource::SkippedEmpty);
    }

    let rows_hash = rule_set_hash(&rows);
    if !force_full
        && db::source_rule_set_hash(conn, &source.csv_url)?.as_deref() == Some(&rows_hash)
    {
        db::update_source_success(conn, &source.csv_url, probe_hash, &rows_hash, observed_at)?;
        return Ok(RefreshedSource::Unchanged);
    }

    upsert_allowed_rows(conn, &rows, observed_at)?;
    db::update_source_success(conn, &source.csv_url, probe_hash, &rows_hash, observed_at)?;

    Ok(RefreshedSource::UpdatedOrEmpty)
}

fn now_string() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}
