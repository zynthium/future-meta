//! Import retained INE daily parameters paired with reviewed close-today rules.

use crate::db;
use crate::jin10::ContractStaticMetadata;
use crate::official::validate_official_canonical_url;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use future_meta::symbol::derive_underlying_symbol;
use reqwest::Url;
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, Duration, OffsetDateTime};

/// Inputs for one offline, hash-verified INE parameter import.
#[derive(Debug, Clone)]
pub struct IneParameterImportOptions {
    /// Review-copy history database.
    pub history_db: PathBuf,
    /// Retained INE dailydata manifest.
    pub manifest: PathBuf,
    /// Reviewed close-today rule manifest.
    pub close_today_rules: PathBuf,
    /// Directory containing content-addressed retained bytes.
    pub snapshot_dir: PathBuf,
    /// First report date to import.
    pub from: Date,
    /// Audit timestamp attached to persisted rows.
    pub observed_at: String,
}

/// Counts returned after a successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IneParameterImportResult {
    /// Daily parameter snapshots checked.
    pub snapshots: usize,
    /// Concrete contracts materialized.
    pub contracts: usize,
    /// Distinct fee versions materialized.
    pub versions: usize,
}

#[derive(Debug, Deserialize)]
struct ManifestRow {
    requested_date: String,
    status: String,
    report_date: String,
    sha256: String,
    url: String,
    record_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct ParameterDocument {
    o_code: serde_json::Value,
    report_date: String,
    o_cursor: Vec<ParameterRow>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
struct ParameterRow {
    productid: String,
    instrumentid: String,
    tradefeeratio: f64,
    tradefeeunit: f64,
}

#[derive(Debug, Deserialize)]
struct CloseTodayRuleRow {
    scope: String,
    valid_from: String,
    valid_to: String,
    close_today_kind: String,
    close_today_value: Option<f64>,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct CloseTodayRule {
    scope: String,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
    fee: CloseTodayFee,
    canonical_url: String,
    body_sha256: String,
}

#[derive(Debug, Clone)]
enum CloseTodayFee {
    SameAsGeneral,
    Explicit(FeeSpec),
}

#[derive(Debug, Clone)]
struct Observation {
    symbol: String,
    valid_from: String,
    fees: [FeeSpec; 3],
    evidence: Vec<db::OfficialEvidenceReference>,
}

type ExistingMetadata = (Option<String>, Option<String>, f64, f64);

/// Validate retained INE dailydata and paired close-today evidence, then
/// materialize complete three-leg fee tuples.
///
/// # Errors
///
/// Returns an error for invalid URLs, digests, parameter fields, uncovered or
/// ambiguous close-today rules, metadata, timestamps, or database writes.
pub fn import_daily_parameters(
    options: &IneParameterImportOptions,
) -> Result<IneParameterImportResult> {
    let rules = load_close_today_rules(options)?;
    let (observations, snapshots) = load_observations(options, &rules)?;
    if observations.is_empty() {
        bail!("INE parameter import has no in-range observations");
    }

    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    let mut last_observed = BTreeMap::<String, String>::new();
    let mut corpus_last = String::new();
    for observation in observations {
        corpus_last = corpus_last.max(observation.valid_from.clone());
        last_observed.insert(observation.symbol.clone(), observation.valid_from.clone());
        let rows = by_symbol.entry(observation.symbol.clone()).or_default();
        if rows
            .last()
            .is_none_or(|previous| previous.fees != observation.fees)
        {
            rows.push(observation);
        }
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let metadata = db::product_static_metadata_candidates(&connection)?;
    let rows = materialize_rows(
        &connection,
        &metadata,
        &by_symbol,
        &last_observed,
        &corpus_last,
    )?;
    let versions =
        db::replace_with_official_parameter_history(&mut connection, &rows, &options.observed_at)?;
    Ok(IneParameterImportResult {
        snapshots,
        contracts: by_symbol.len(),
        versions,
    })
}

fn load_close_today_rules(options: &IneParameterImportOptions) -> Result<Vec<CloseTodayRule>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.close_today_rules)?;
    let mut rules = Vec::new();
    for row in reader.deserialize::<CloseTodayRuleRow>() {
        let row = row?;
        validate_rule_scope(&row.scope)?;
        validate_close_today_rule_url(&row.canonical_url)?;
        verify_retained_evidence(&options.snapshot_dir, &row.sha256)?;
        let valid_from = OffsetDateTime::parse(&row.valid_from, &Rfc3339)
            .with_context(|| format!("invalid INE close-today valid_from {}", row.valid_from))?;
        let valid_to = empty_to_none(&row.valid_to)
            .map(|value| OffsetDateTime::parse(value, &Rfc3339))
            .transpose()
            .with_context(|| format!("invalid INE close-today valid_to {}", row.valid_to))?;
        if valid_to.is_some_and(|value| value <= valid_from) {
            bail!("invalid INE close-today interval for {}", row.scope);
        }
        let fee = parse_close_today_fee(&row)?;
        rules.push(CloseTodayRule {
            scope: row.scope,
            valid_from,
            valid_to,
            fee,
            canonical_url: row.canonical_url,
            body_sha256: row.sha256,
        });
    }
    if rules.is_empty() {
        bail!("INE close-today rule manifest is empty");
    }
    Ok(rules)
}

fn load_observations(
    options: &IneParameterImportOptions,
    rules: &[CloseTodayRule],
) -> Result<(Vec<Observation>, usize)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.manifest)?;
    let mut observations = Vec::new();
    let mut snapshots = 0;
    for row in reader.deserialize::<ManifestRow>() {
        let row = row?;
        if row.status != "ok" {
            continue;
        }
        let date = parse_compact_date(&row.report_date)?;
        if date < options.from {
            continue;
        }
        validate_manifest_row(&row)?;
        let path = options.snapshot_dir.join(format!("{}.dat", row.sha256));
        let bytes = std::fs::read(&path)
            .with_context(|| format!("read retained INE snapshot {}", path.display()))?;
        verify_sha256(&bytes, &row.sha256, &row.url)?;
        let document: ParameterDocument = serde_json::from_slice(&bytes)?;
        if !successful_code(&document.o_code) || document.report_date.trim() != row.report_date {
            bail!(
                "INE dailydata document is unsuccessful for {}",
                row.report_date
            );
        }
        if row
            .record_count
            .is_some_and(|expected| expected != document.o_cursor.len())
        {
            bail!(
                "INE dailydata record count mismatch for {}",
                row.report_date
            );
        }
        snapshots += 1;
        let valid_from = format!("{date}T00:00:00+08:00");
        let observed_at = OffsetDateTime::parse(&valid_from, &Rfc3339)?;
        for parameter in document.o_cursor {
            let product = parameter.productid.trim().to_ascii_lowercase();
            let instrument = parameter.instrumentid.trim().to_ascii_lowercase();
            if !product.ends_with("_f") || !is_contract_id(&instrument) {
                continue;
            }
            let symbol = format!("INE.{instrument}");
            let product_scope = format!("INE.{}", product.trim_end_matches("_f"));
            let rule = select_rule(rules, &symbol, &product_scope, observed_at)?;
            let general = parse_general_fee(parameter.tradefeeratio, parameter.tradefeeunit)?;
            let close_today = match &rule.fee {
                CloseTodayFee::SameAsGeneral => general.clone(),
                CloseTodayFee::Explicit(fee) => fee.clone(),
            };
            observations.push(Observation {
                symbol,
                valid_from: valid_from.clone(),
                fees: [general.clone(), general, close_today],
                evidence: vec![
                    db::OfficialEvidenceReference {
                        canonical_url: row.url.clone(),
                        body_sha256: row.sha256.clone(),
                    },
                    db::OfficialEvidenceReference {
                        canonical_url: rule.canonical_url.clone(),
                        body_sha256: rule.body_sha256.clone(),
                    },
                ],
            });
        }
    }
    Ok((observations, snapshots))
}

fn materialize_rows(
    connection: &rusqlite::Connection,
    product_metadata: &BTreeMap<String, Vec<ContractStaticMetadata>>,
    by_symbol: &BTreeMap<String, Vec<Observation>>,
    last_observed: &BTreeMap<String, String>,
    corpus_last: &str,
) -> Result<Vec<db::OfficialHistoryRow>> {
    let mut result = Vec::new();
    for observations in by_symbol.values() {
        let first = observations
            .first()
            .ok_or_else(|| anyhow!("empty INE observation group"))?;
        let symbol_last = last_observed
            .get(&first.symbol)
            .ok_or_else(|| anyhow!("INE contract has no last observation"))?;
        let coverage_end =
            (OffsetDateTime::parse(symbol_last, &Rfc3339)? + Duration::days(1)).format(&Rfc3339)?;
        let existing = load_existing_metadata(connection, &first.symbol)?;
        let inferred_listing = first.valid_from[..10].replace('-', "");
        let inferred_expiry =
            (symbol_last.as_str() < corpus_last).then(|| symbol_last[..10].replace('-', ""));
        let (listing, expiry, lot_size, tick_size) = if let Some(mut existing) = existing {
            existing.0.get_or_insert(inferred_listing);
            if existing.1.is_none() {
                existing.1 = inferred_expiry;
            }
            existing
        } else {
            let product = derive_underlying_symbol(&first.symbol)?;
            let candidates = product_metadata
                .get(&product)
                .ok_or_else(|| anyhow!("INE contract metadata missing {}", first.symbol))?;
            if candidates.len() != 1 {
                bail!(
                    "INE contract metadata ambiguous {}: {} candidates",
                    first.symbol,
                    candidates.len()
                );
            }
            (
                Some(inferred_listing),
                inferred_expiry,
                candidates[0].lot_size,
                candidates[0].tick_size,
            )
        };
        for observation in observations {
            result.push(db::OfficialHistoryRow {
                row: AllowedRow {
                    symbol: observation.symbol.clone(),
                    listing_date: listing.clone(),
                    expiry_date: expiry.clone(),
                    trading_status: TradingStatus::Unknown,
                    buy_margin_rate: None,
                    sell_margin_rate: None,
                    open_fee: observation.fees[0].clone(),
                    close_yesterday_fee: observation.fees[1].clone(),
                    close_today_fee: observation.fees[2].clone(),
                    lot_size,
                    tick_size,
                    source_updated_at: Some(observation.valid_from.clone()),
                    is_main_contract: false,
                },
                coverage_end_exclusive: coverage_end.clone(),
                evidence_level: db::OfficialEvidenceLevel::PairedOfficial,
                evidence: observation.evidence.clone(),
            });
        }
    }
    Ok(result)
}

fn select_rule<'a>(
    rules: &'a [CloseTodayRule],
    symbol: &str,
    product: &str,
    observed_at: OffsetDateTime,
) -> Result<&'a CloseTodayRule> {
    let mut matching = rules
        .iter()
        .filter(|rule| {
            (rule.scope == symbol || rule.scope == product)
                && rule.valid_from <= observed_at
                && rule.valid_to.is_none_or(|valid_to| observed_at < valid_to)
        })
        .collect::<Vec<_>>();
    matching.sort_by_key(|rule| usize::from(rule.scope == symbol));
    let selected = matching
        .last()
        .ok_or_else(|| anyhow!("INE close-today rule missing for {symbol} at {observed_at}"))?;
    let selected_specificity = selected.scope == symbol;
    if matching
        .iter()
        .filter(|rule| (rule.scope == symbol) == selected_specificity)
        .count()
        != 1
    {
        bail!("INE close-today rule is ambiguous for {symbol} at {observed_at}");
    }
    Ok(selected)
}

fn parse_general_fee(ratio: f64, unit: f64) -> Result<FeeSpec> {
    if !ratio.is_finite() || !unit.is_finite() || ratio < 0.0 || unit < 0.0 {
        bail!("INE general fee fields must be finite and non-negative");
    }
    if ratio > 0.0 && unit > 0.0 {
        bail!("INE general fee cannot contain both ratio and unit");
    }
    if ratio > 0.0 {
        let value = ratio * 10.0;
        return Ok(FeeSpec {
            kind: FeeKind::TurnoverRatePerTenThousand,
            value: Some(value),
            raw_text: Some(format!("TRADEFEERATIO={ratio}")),
        });
    }
    if unit > 0.0 {
        return Ok(FeeSpec {
            kind: FeeKind::CnyPerLot,
            value: Some(unit),
            raw_text: Some(format!("TRADEFEEUNIT={unit}")),
        });
    }
    Ok(FeeSpec {
        kind: FeeKind::Zero,
        value: Some(0.0),
        raw_text: Some("TRADEFEERATIO=0;TRADEFEEUNIT=0".to_owned()),
    })
}

fn parse_close_today_fee(row: &CloseTodayRuleRow) -> Result<CloseTodayFee> {
    match row.close_today_kind.as_str() {
        "same_as_general" => {
            if row.close_today_value.is_some() {
                bail!("INE same_as_general close-today rule must not have a value");
            }
            Ok(CloseTodayFee::SameAsGeneral)
        }
        "Zero" => {
            if row.close_today_value.is_some_and(|value| value != 0.0) {
                bail!("INE zero close-today rule has a non-zero value");
            }
            Ok(CloseTodayFee::Explicit(FeeSpec {
                kind: FeeKind::Zero,
                value: Some(0.0),
                raw_text: Some("reviewed official close-today zero".to_owned()),
            }))
        }
        "CnyPerLot" | "TurnoverRatePerTenThousand" => {
            let value = row
                .close_today_value
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| anyhow!("INE explicit close-today rule needs a positive value"))?;
            Ok(CloseTodayFee::Explicit(FeeSpec {
                kind: if row.close_today_kind == "CnyPerLot" {
                    FeeKind::CnyPerLot
                } else {
                    FeeKind::TurnoverRatePerTenThousand
                },
                value: Some(value),
                raw_text: Some("reviewed official close-today rule".to_owned()),
            }))
        }
        other => bail!("unknown INE close-today rule kind {other}"),
    }
}

fn validate_manifest_row(row: &ManifestRow) -> Result<()> {
    if row.requested_date != row.report_date {
        bail!("INE requested/report date mismatch");
    }
    validate_sha256(&row.sha256)?;
    let url = Url::parse(&row.url)?;
    let expected_path = format!("/data/tradedata/future/dailydata/js{}.dat", row.report_date);
    if url.scheme() != "https"
        || url.host_str() != Some("www.ine.cn")
        || url.path() != expected_path
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("unexpected INE dailydata URL {}", row.url);
    }
    Ok(())
}

fn validate_rule_scope(scope: &str) -> Result<()> {
    let Some(value) = scope.strip_prefix("INE.") else {
        bail!("invalid INE close-today scope {scope}");
    };
    let letters = value.bytes().take_while(u8::is_ascii_lowercase).count();
    if !(1..=3).contains(&letters)
        || (letters != value.len()
            && (value.len() != letters + 4
                || !value.as_bytes()[letters..].iter().all(u8::is_ascii_digit)))
    {
        bail!("invalid INE close-today scope {scope}");
    }
    Ok(())
}

fn validate_close_today_rule_url(value: &str) -> Result<()> {
    validate_official_canonical_url("INE", value)?;
    let url = Url::parse(value)?;
    if !url.path().starts_with("/publicnotice/notice/")
        || !Path::new(url.path())
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("close-today rule must be an INE notice: {value}");
    }
    Ok(())
}

fn verify_retained_evidence(snapshot_dir: &Path, sha256: &str) -> Result<()> {
    validate_sha256(sha256)?;
    let prefix = format!("{sha256}.");
    let matches = std::fs::read_dir(snapshot_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!(
            "INE evidence digest {sha256} resolved {} files",
            matches.len()
        );
    }
    let bytes = std::fs::read(&matches[0])?;
    verify_sha256(&bytes, sha256, &matches[0].display().to_string())
}

fn verify_sha256(bytes: &[u8], expected: &str, label: &str) -> Result<()> {
    validate_sha256(expected)?;
    if hex::encode(Sha256::digest(bytes)) != expected {
        bail!("retained INE SHA-256 mismatch for {label}");
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid INE SHA-256 {value}");
    }
    Ok(())
}

fn load_existing_metadata(
    connection: &rusqlite::Connection,
    symbol: &str,
) -> Result<Option<ExistingMetadata>> {
    Ok(connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size
             from contracts where symbol = ?1",
            [symbol],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

fn successful_code(value: &serde_json::Value) -> bool {
    value.as_str().is_some_and(|value| value == "0")
        || value.as_i64().is_some_and(|value| value == 0)
}

fn parse_compact_date(value: &str) -> Result<Date> {
    if value.len() != 8 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid compact INE date {value}");
    }
    let format = time::format_description::parse("[year][month][day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid INE date {value}"))
}

fn is_contract_id(value: &str) -> bool {
    let letters = value.bytes().take_while(u8::is_ascii_lowercase).count();
    (1..=3).contains(&letters)
        && value.len() == letters + 4
        && value.as_bytes()[letters..].iter().all(u8::is_ascii_digit)
}

fn empty_to_none(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::validate_close_today_rule_url;

    #[test]
    fn close_today_rule_rejects_non_notice_official_page() {
        let error =
            validate_close_today_rule_url("https://www.ine.cn/products/futures/index_f/lu_f/")
                .unwrap_err();

        assert!(error.to_string().contains("must be an INE notice"));
    }
}
