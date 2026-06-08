//! Refresh orchestration.

use crate::db::{self, connect, ensure_schema, upsert_allowed_rows};
use crate::hash::{rule_set_hash, source_probe_hash};
use crate::parse::parse_csv;
use crate::source::{discover_sources_from_html, fetch_text, http_client};
use anyhow::{Result, anyhow};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

const TOTAL_URL: &str = "https://www.9qihuo.com/qihuoshouxufei";

/// Refresh fee history data.
///
/// # Errors
///
/// Returns an error if refresh fails.
pub fn refresh(db: &Path, force_full: bool) -> Result<()> {
    let mut conn = connect(db)?;
    ensure_schema(&conn)?;

    let client = http_client()?;
    let html = fetch_text(&client, TOTAL_URL)?;
    let sources = discover_sources_from_html(&html)?;
    let observed_at = now_string()?;
    let mut attempted = 0usize;
    let mut succeeded = 0usize;
    let mut failed = 0usize;

    for source in sources {
        let probe_hash = source_probe_hash(&source.csv_url, &source.detail_url);
        if !force_full
            && db::source_probe_hash(&conn, &source.csv_url)?.as_deref() == Some(&probe_hash)
        {
            continue;
        }
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
            force_full,
        ) {
            Ok(RefreshedSource::UpdatedOrEmpty) => {
                succeeded += 1;
                eprintln!("source refresh succeeded for {}", source.csv_url);
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

enum RefreshedSource {
    UpdatedOrEmpty,
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

    upsert_allowed_rows(conn, &rows, observed_at)?;
    let rows_hash = rule_set_hash(&rows);
    db::update_source_success(conn, &source.csv_url, probe_hash, &rows_hash, observed_at)?;

    Ok(RefreshedSource::UpdatedOrEmpty)
}

fn now_string() -> Result<String> {
    Ok(OffsetDateTime::now_utc().format(&Rfc3339)?)
}
