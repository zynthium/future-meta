//! Materialize retained, paired official adjustment documents as fee history.

use crate::db;
use crate::official::{EvidenceKind, OfficialEvidence, OfficialFeeAdjustment, validate_adjustment};
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime};

/// Inputs for offline, hash-verified paired-official history import.
#[derive(Debug, Clone)]
pub struct OfficialHistoryImportOptions {
    pub history_db: PathBuf,
    pub inputs: Vec<PathBuf>,
    pub evidence_db: Option<PathBuf>,
    pub exchange: Option<String>,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub through: Date,
    pub observed_at: String,
}

/// Counts returned after successful import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OfficialHistoryImportResult {
    pub adjustments: usize,
    pub contracts: usize,
    pub versions: usize,
}

/// Validate retained paired documents and materialize complete adjustment
/// chains into review history.
///
/// Partial adjustments inherit only from an earlier complete tuple for the
/// same concrete contract in supplied inputs. This prevents product-level or
/// current-baseline guesses from filling fields an official notice omitted.
///
/// # Errors
///
/// Returns an error for incomplete document pairs, missing retained bytes,
/// digest mismatches, partial chains without a complete predecessor, ambiguous
/// contract metadata, or database failures.
#[allow(clippy::too_many_lines)]
pub fn import_adjustments(
    options: &OfficialHistoryImportOptions,
) -> Result<OfficialHistoryImportResult> {
    if options.inputs.is_empty() && options.evidence_db.is_none() {
        bail!("official history import requires an input or evidence database");
    }
    if options.through < options.from {
        bail!("official history through date precedes from date");
    }
    if options.exchange.as_deref().is_some_and(|exchange| {
        !matches!(exchange, "SHFE" | "INE" | "DCE" | "CZCE" | "CFFEX" | "GFEX")
    }) {
        bail!("unsupported official history exchange filter");
    }
    let mut adjustments = Vec::new();
    for input in &options.inputs {
        let json = std::fs::read_to_string(input)
            .with_context(|| format!("read official adjustment input {}", input.display()))?;
        let input_adjustments: Vec<OfficialFeeAdjustment> = serde_json::from_str(&json)
            .with_context(|| format!("decode official adjustment input {}", input.display()))?;
        for adjustment in input_adjustments {
            if !matches_exchange(&adjustment, options.exchange.as_deref()) {
                continue;
            }
            validate_adjustment(&adjustment)?;
            require_paired_evidence(&adjustment)?;
            let effective = OffsetDateTime::parse(&adjustment.effective_at, &Rfc3339)?;
            if effective.date() < options.from || effective.date() > options.through {
                continue;
            }
            for evidence in &adjustment.evidence {
                verify_retained_evidence(&options.snapshot_dir, &evidence.sha256)?;
            }
            adjustments.push(adjustment);
        }
    }
    if let Some(evidence_db) = options.evidence_db.as_deref() {
        for adjustment in load_staged_adjustments(evidence_db)? {
            if !matches_exchange(&adjustment, options.exchange.as_deref()) {
                continue;
            }
            validate_adjustment(&adjustment)?;
            require_paired_evidence(&adjustment)?;
            let effective = OffsetDateTime::parse(&adjustment.effective_at, &Rfc3339)?;
            if effective.date() < options.from || effective.date() > options.through {
                continue;
            }
            for evidence in &adjustment.evidence {
                verify_retained_evidence(&options.snapshot_dir, &evidence.sha256)?;
            }
            adjustments.push(adjustment);
        }
    }
    if adjustments.is_empty() {
        bail!("official history import has no in-range adjustments");
    }

    let adjustment_count = adjustments.len();
    let mut by_symbol = BTreeMap::<String, Vec<OfficialFeeAdjustment>>::new();
    for adjustment in adjustments {
        by_symbol
            .entry(adjustment.symbol.clone())
            .or_default()
            .push(adjustment);
    }
    let coverage_end_exclusive = (options
        .through
        .next_day()
        .ok_or_else(|| anyhow!("official history through date cannot advance"))?)
    .midnight()
    .assume_offset(time::UtcOffset::from_hms(8, 0, 0)?)
    .format(&Rfc3339)?;
    let mut conn = db::connect(&options.history_db)?;
    db::ensure_schema(&conn)?;
    let product_metadata = db::product_static_metadata_candidates(&conn)?;
    let mut rows = Vec::<db::OfficialHistoryRow>::new();
    for symbol_adjustments in by_symbol.values_mut() {
        symbol_adjustments.sort_by(|left, right| {
            left.effective_at
                .cmp(&right.effective_at)
                .then_with(|| stated_fee_count(right).cmp(&stated_fee_count(left)))
                .then_with(|| left.scope.cmp(&right.scope))
        });
        let symbol = &symbol_adjustments[0].symbol;
        let existing_metadata = conn
            .query_row(
                "select listing_date, expiry_date, lot_size, tick_size
                 from contracts where symbol = ?1",
                [symbol],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, f64>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                },
            )
            .optional()?;
        let listing = symbol_adjustments[0].effective_at[..10].replace('-', "");
        let metadata = if let Some(mut metadata) = existing_metadata {
            if metadata.0.is_none() {
                metadata.0 = Some(listing);
            }
            metadata
        } else {
            let product = derive_underlying_symbol(symbol)?;
            let candidates = product_metadata
                .get(&product)
                .ok_or_else(|| anyhow!("official contract metadata missing for {symbol}"))?;
            if candidates.len() != 1 {
                bail!(
                    "official contract metadata is ambiguous for {symbol}: {} candidates",
                    candidates.len()
                );
            }
            (
                Some(listing),
                None,
                candidates[0].lot_size,
                candidates[0].tick_size,
            )
        };
        let mut current: Option<[FeeSpec; 3]> = None;
        for adjustment in symbol_adjustments {
            let fees = complete_tuple(adjustment, current.as_ref())?;
            current = Some(fees.clone());
            let mut history_row = db::OfficialHistoryRow {
                row: AllowedRow {
                    symbol: adjustment.symbol.clone(),
                    listing_date: metadata.0.clone(),
                    expiry_date: metadata.1.clone(),
                    trading_status: TradingStatus::Unknown,
                    buy_margin_rate: None,
                    sell_margin_rate: None,
                    open_fee: fees[0].clone(),
                    close_yesterday_fee: fees[1].clone(),
                    close_today_fee: fees[2].clone(),
                    lot_size: metadata.2,
                    tick_size: metadata.3,
                    source_updated_at: Some(adjustment.effective_at.clone()),
                    is_main_contract: false,
                },
                coverage_end_exclusive: coverage_end_exclusive.clone(),
                evidence_level: db::OfficialEvidenceLevel::PairedOfficial,
                evidence: adjustment
                    .evidence
                    .iter()
                    .map(|evidence| db::OfficialEvidenceReference {
                        canonical_url: evidence.canonical_url.clone(),
                        body_sha256: evidence.sha256.clone(),
                    })
                    .collect(),
            };
            if let Some(previous) = rows.last_mut().filter(|previous| {
                previous.row.symbol == history_row.row.symbol
                    && previous.row.source_updated_at == history_row.row.source_updated_at
            }) {
                previous.row.open_fee = history_row.row.open_fee;
                previous.row.close_yesterday_fee = history_row.row.close_yesterday_fee;
                previous.row.close_today_fee = history_row.row.close_today_fee;
                previous.evidence.append(&mut history_row.evidence);
                previous.evidence.sort_by(|left, right| {
                    left.canonical_url
                        .cmp(&right.canonical_url)
                        .then_with(|| left.body_sha256.cmp(&right.body_sha256))
                });
                previous.evidence.dedup_by(|left, right| {
                    left.canonical_url == right.canonical_url
                        && left.body_sha256 == right.body_sha256
                });
            } else {
                rows.push(history_row);
            }
        }
    }
    let versions =
        db::replace_with_official_parameter_history(&mut conn, &rows, &options.observed_at)?;
    Ok(OfficialHistoryImportResult {
        adjustments: adjustment_count,
        contracts: by_symbol.len(),
        versions,
    })
}

fn stated_fee_count(adjustment: &OfficialFeeAdjustment) -> usize {
    [
        adjustment.open_fee.as_ref(),
        adjustment.close_yesterday_fee.as_ref(),
        adjustment.close_today_fee.as_ref(),
    ]
    .into_iter()
    .flatten()
    .count()
}

fn matches_exchange(adjustment: &OfficialFeeAdjustment, exchange: Option<&str>) -> bool {
    exchange.is_none_or(|exchange| {
        adjustment
            .symbol
            .split_once('.')
            .is_some_and(|(actual, _)| actual == exchange)
    })
}

fn require_paired_evidence(adjustment: &OfficialFeeAdjustment) -> Result<()> {
    let notice = adjustment
        .evidence
        .iter()
        .any(|evidence| evidence.kind == EvidenceKind::Notice);
    let parameter = adjustment.evidence.iter().any(|evidence| {
        matches!(
            evidence.kind,
            EvidenceKind::FeeSchedule | EvidenceKind::SettlementParameter
        )
    });
    if !notice || !parameter {
        bail!(
            "paired official import requires notice and schedule: {}",
            adjustment.symbol
        );
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
        bail!("retained official evidence must resolve uniquely: {expected_sha256}");
    }
    let bytes = std::fs::read(&matches[0])?;
    let actual = hex::encode(Sha256::digest(&bytes));
    if actual != expected_sha256 {
        bail!("retained official evidence SHA-256 mismatch: {expected_sha256}");
    }
    Ok(())
}

fn complete_tuple(
    adjustment: &OfficialFeeAdjustment,
    previous: Option<&[FeeSpec; 3]>,
) -> Result<[FeeSpec; 3]> {
    let values = [
        adjustment.open_fee.clone(),
        adjustment.close_yesterday_fee.clone(),
        adjustment.close_today_fee.clone(),
    ];
    if values.iter().all(Option::is_some) {
        return Ok(values.map(Option::unwrap));
    }
    let previous = previous.ok_or_else(|| {
        anyhow!(
            "partial official adjustment has no complete predecessor: {} {}",
            adjustment.symbol,
            adjustment.effective_at
        )
    })?;
    Ok(std::array::from_fn(|index| {
        values[index]
            .clone()
            .unwrap_or_else(|| previous[index].clone())
    }))
}

fn load_staged_adjustments(path: &Path) -> Result<Vec<OfficialFeeAdjustment>> {
    let conn = Connection::open(path)?;
    let has_previous_fees = conn
        .prepare("pragma table_info(official_fee_adjustments)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "previous_fees_json");
    let previous_fees_projection = if has_previous_fees {
        "previous_fees_json"
    } else {
        "null"
    };
    let query = format!(
        "select id, symbol, effective_at, scope, open_fee_json,
                close_yesterday_fee_json, close_today_fee_json, {previous_fees_projection}
         from official_fee_adjustments where verification = 'verified'
         order by effective_at, symbol, scope"
    );
    let mut statement = conn.prepare(&query)?;
    let mut records = statement.query([])?;
    let mut adjustments = Vec::new();
    while let Some(record) = records.next()? {
        let adjustment_id: i64 = record.get(0)?;
        let mut evidence_statement = conn.prepare(
            "select e.canonical_url, e.mirror_url, e.sha256, e.published_at,
                    e.evidence_kind
             from official_evidence e
             join official_adjustment_evidence link on link.evidence_id = e.id
             where link.adjustment_id = ?1 order by e.canonical_url",
        )?;
        let evidence = evidence_statement
            .query_map([adjustment_id], |row| {
                let kind = match row.get::<_, String>(4)?.as_str() {
                    "notice" => EvidenceKind::Notice,
                    "fee_schedule" => EvidenceKind::FeeSchedule,
                    "settlement_parameter" => EvidenceKind::SettlementParameter,
                    other => {
                        return Err(rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            anyhow!("unknown official evidence kind {other}").into(),
                        ));
                    }
                };
                Ok(OfficialEvidence {
                    canonical_url: row.get(0)?,
                    mirror_url: row.get(1)?,
                    sha256: row.get(2)?,
                    published_at: row.get(3)?,
                    kind,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        adjustments.push(OfficialFeeAdjustment {
            symbol: record.get(1)?,
            effective_at: record.get(2)?,
            scope: record.get(3)?,
            open_fee: record
                .get::<_, Option<String>>(4)?
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            close_yesterday_fee: record
                .get::<_, Option<String>>(5)?
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            close_today_fee: record
                .get::<_, Option<String>>(6)?
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            previous_fees: record
                .get::<_, Option<String>>(7)?
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            evidence,
        });
    }
    Ok(adjustments)
}

#[cfg(test)]
mod tests {
    use super::load_staged_adjustments;
    use crate::official::stage_adjustments_json;

    #[test]
    fn loads_verified_adjustments_and_document_links_from_evidence_database() {
        let directory = tempfile::tempdir().unwrap();
        let evidence_db = directory.path().join("evidence.sqlite");
        let json = r#"[{
          "symbol":"CFFEX.IF2001",
          "effective_at":"2020-01-02T00:00:00+08:00",
          "scope":"listing",
          "open_fee":{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"0.23"},
          "close_yesterday_fee":{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"0.23"},
          "close_today_fee":{"kind":"TurnoverRatePerTenThousand","value":3.45,"raw_text":"3.45"},
          "previous_fees":null,
          "evidence":[
            {"canonical_url":"http://www.cffex.com.cn/cn/jystz/20200101/1.html","mirror_url":null,"sha256":"1111111111111111111111111111111111111111111111111111111111111111","published_at":"2020-01-01T00:00:00+08:00","kind":"notice"},
            {"canonical_url":"http://www.cffex.com.cn/sj/jscs/202001/02/20200102_1.csv","mirror_url":null,"sha256":"2222222222222222222222222222222222222222222222222222222222222222","published_at":"2020-01-02T00:00:00+08:00","kind":"settlement_parameter"}
          ]
        }]"#;
        stage_adjustments_json(&evidence_db, json).unwrap();

        let loaded = load_staged_adjustments(&evidence_db).unwrap();

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].symbol, "CFFEX.IF2001");
        assert_eq!(loaded[0].evidence.len(), 2);
    }

    #[test]
    fn loads_legacy_evidence_database_without_previous_fees_column() {
        let directory = tempfile::tempdir().unwrap();
        let evidence_db = directory.path().join("legacy.sqlite");
        let conn = rusqlite::Connection::open(&evidence_db).unwrap();
        conn.execute_batch(
            "create table official_fee_adjustments(
               id integer primary key, symbol text, effective_at text, scope text,
               open_fee_json text, close_yesterday_fee_json text, close_today_fee_json text,
               verification text, recorded_at text
             );
             create table official_evidence(
               id integer primary key, canonical_url text, mirror_url text, sha256 text,
               published_at text, evidence_kind text, recorded_at text
             );
             create table official_adjustment_evidence(adjustment_id integer, evidence_id integer);
             insert into official_fee_adjustments values(
               1, 'CFFEX.IF2001', '2020-01-02T00:00:00+08:00', 'listing',
               '{\"kind\":\"CnyPerLot\",\"value\":1.0}',
               '{\"kind\":\"CnyPerLot\",\"value\":1.0}',
               '{\"kind\":\"Zero\",\"value\":0.0}', 'verified', '2020-01-02'
             );
             insert into official_evidence values(
               1, 'http://www.cffex.com.cn/notice.html', null,
               '1111111111111111111111111111111111111111111111111111111111111111',
               '2020-01-01T00:00:00+08:00', 'notice', '2020-01-02'
             );
             insert into official_adjustment_evidence values(1, 1);",
        )
        .unwrap();

        let loaded = load_staged_adjustments(&evidence_db).unwrap();

        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].previous_fees.is_none());
    }
}
