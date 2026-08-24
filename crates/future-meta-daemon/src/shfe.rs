//! Import retained SHFE monthly parameters paired with close-today notices.

use crate::db;
use crate::parse::AllowedRow;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, UtcOffset};

/// Inputs for an offline, hash-verified SHFE parameter import.
#[derive(Debug, Clone)]
pub struct ShfeParameterImportOptions {
    pub history_db: PathBuf,
    /// `*-raw-fee-observations.tsv` produced from retained SHFE monthly tables.
    pub parameter_manifest: PathBuf,
    /// Reviewed concrete-contract close-today intervals backed by SHFE notices.
    pub close_today_rules: PathBuf,
    pub snapshot_dir: PathBuf,
    pub from: Date,
    pub through: Date,
    pub observed_at: String,
}

/// Aggregate result from one complete SHFE import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShfeParameterImportResult {
    pub snapshots: usize,
    pub contracts: usize,
    pub versions: usize,
}

#[derive(Debug, Deserialize)]
struct ParameterRow {
    reported_month: String,
    source_kind: String,
    parent_url: String,
    source_url: String,
    sha256: String,
    product: String,
    contract: String,
    adjustment_date_text: String,
    fee_context: String,
    raw_fee_value: String,
    source_cell: String,
}

#[derive(Debug, Deserialize)]
struct CloseTodayRuleRow {
    symbol: String,
    valid_from: String,
    valid_to: String,
    close_today_kind: String,
    close_today_value: Option<f64>,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct CloseTodayRule {
    symbol: String,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
    fee: FeeSpec,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct Observation {
    symbol: String,
    valid_from: String,
    fees: [FeeSpec; 3],
    evidence: Vec<db::OfficialEvidenceReference>,
}

type ExistingMetadata = (Option<String>, Option<String>, f64, f64);

/// Materialize SHFE general fees and verified close-today rules as complete
/// official fee tuples.
///
/// The monthly table only establishes `open` and `close_yesterday`; every
/// materialized row must additionally match one concrete-contract notice rule.
///
/// # Errors
///
/// Returns an error for malformed retained inputs, unverified evidence,
/// incomplete close-today rules, missing contract metadata, or DB failures.
pub fn import_monthly_parameters(
    options: &ShfeParameterImportOptions,
) -> Result<ShfeParameterImportResult> {
    if options.through < options.from {
        bail!("SHFE import through date precedes from date");
    }
    let rules = load_close_today_rules(options)?;
    let (mut observations, snapshots) = load_observations(options, &rules)?;
    if observations.is_empty() {
        bail!(
            "SHFE parameter import has no observations through {}",
            options.through
        );
    }
    observations.sort_by(|left, right| {
        left.symbol
            .cmp(&right.symbol)
            .then_with(|| left.valid_from.cmp(&right.valid_from))
    });

    let mut by_symbol = BTreeMap::<String, Vec<Observation>>::new();
    for observation in observations {
        let rows = by_symbol.entry(observation.symbol.clone()).or_default();
        if let Some(previous) = rows.last() {
            if previous.valid_from == observation.valid_from {
                if previous.fees != observation.fees {
                    bail!(
                        "conflicting SHFE parameter values for {} at {}",
                        observation.symbol,
                        observation.valid_from
                    );
                }
                continue;
            }
            if previous.fees == observation.fees {
                continue;
            }
        }
        rows.push(observation);
    }

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let coverage_end = options
        .through
        .next_day()
        .ok_or_else(|| anyhow!("SHFE coverage end cannot advance"))?
        .midnight()
        .assume_offset(UtcOffset::from_hms(8, 0, 0)?)
        .format(&Rfc3339)?;
    let mut history = Vec::new();
    for observations in by_symbol.values() {
        let first = observations
            .first()
            .ok_or_else(|| anyhow!("empty SHFE observation group"))?;
        let metadata = load_existing_metadata(&connection, &first.symbol)?
            .ok_or_else(|| anyhow!("official SHFE contract metadata missing {}", first.symbol))?;
        for observation in observations {
            history.push(db::OfficialHistoryRow {
                row: AllowedRow {
                    symbol: observation.symbol.clone(),
                    listing_date: metadata.0.clone(),
                    expiry_date: metadata.1.clone(),
                    trading_status: TradingStatus::Unknown,
                    buy_margin_rate: None,
                    sell_margin_rate: None,
                    open_fee: observation.fees[0].clone(),
                    close_yesterday_fee: observation.fees[1].clone(),
                    close_today_fee: observation.fees[2].clone(),
                    lot_size: metadata.2,
                    tick_size: metadata.3,
                    source_updated_at: Some(observation.valid_from.clone()),
                    is_main_contract: false,
                },
                coverage_end_exclusive: coverage_end.clone(),
                evidence_level: db::OfficialEvidenceLevel::PairedOfficial,
                evidence: observation.evidence.clone(),
            });
        }
    }
    let versions = db::replace_with_official_parameter_history(
        &mut connection,
        &history,
        &options.observed_at,
    )?;
    Ok(ShfeParameterImportResult {
        snapshots,
        contracts: by_symbol.len(),
        versions,
    })
}

fn load_close_today_rules(options: &ShfeParameterImportOptions) -> Result<Vec<CloseTodayRule>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.close_today_rules)?;
    let mut rules = Vec::new();
    for record in reader.deserialize::<CloseTodayRuleRow>() {
        let record = record?;
        validate_symbol(&record.symbol)?;
        validate_notice_url(&record.canonical_url)?;
        verify_retained_evidence(&options.snapshot_dir, &record.sha256)?;
        let valid_from =
            OffsetDateTime::parse(&record.valid_from, &Rfc3339).with_context(|| {
                format!("invalid SHFE close-today valid_from {}", record.valid_from)
            })?;
        let valid_to = (!record.valid_to.trim().is_empty())
            .then(|| OffsetDateTime::parse(record.valid_to.trim(), &Rfc3339))
            .transpose()
            .with_context(|| format!("invalid SHFE close-today valid_to {}", record.valid_to))?;
        if valid_to.is_some_and(|value| value <= valid_from) {
            bail!("invalid SHFE close-today interval for {}", record.symbol);
        }
        rules.push(CloseTodayRule {
            symbol: record.symbol,
            valid_from,
            valid_to,
            fee: parse_explicit_fee(&record.close_today_kind, record.close_today_value)?,
            canonical_url: record.canonical_url,
            sha256: record.sha256,
        });
    }
    if rules.is_empty() {
        bail!("SHFE close-today rule manifest is empty");
    }
    Ok(rules)
}

fn load_observations(
    options: &ShfeParameterImportOptions,
    rules: &[CloseTodayRule],
) -> Result<(Vec<Observation>, usize)> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(&options.parameter_manifest)?;
    let mut observations = Vec::new();
    let mut snapshots = BTreeSet::new();
    for record in reader.deserialize::<ParameterRow>() {
        let record = record?;
        validate_parameter_record(&record)?;
        if snapshots.insert(record.sha256.clone()) {
            verify_retained_evidence(&options.snapshot_dir, &record.sha256)?;
        }
        let date = parse_observation_date(
            &record.reported_month,
            &record.adjustment_date_text,
            &record.parent_url,
        )?;
        if date > options.through {
            continue;
        }
        if record.fee_context.contains("套保") || !record.fee_context.contains("交易手续费")
        {
            continue;
        }
        let symbol = format!("SHFE.{}", record.contract.trim().to_ascii_lowercase());
        validate_symbol(&symbol)?;
        if record.product.trim().to_ascii_lowercase()
            != symbol[5..]
                .bytes()
                .take_while(u8::is_ascii_alphabetic)
                .map(char::from)
                .collect::<String>()
        {
            bail!("SHFE product/contract mismatch for {}", record.contract);
        }
        let general = parse_general_fee(&record.fee_context, &record.raw_fee_value)?;
        let valid_from = date
            .midnight()
            .assume_offset(UtcOffset::from_hms(8, 0, 0)?)
            .format(&Rfc3339)?;
        let observed_at = OffsetDateTime::parse(&valid_from, &Rfc3339)?;
        let rule = select_rule(rules, &symbol, observed_at)?;
        observations.push(Observation {
            symbol,
            valid_from,
            fees: [general.clone(), general, rule.fee.clone()],
            evidence: vec![
                db::OfficialEvidenceReference {
                    canonical_url: record.source_url,
                    body_sha256: record.sha256,
                },
                db::OfficialEvidenceReference {
                    canonical_url: rule.canonical_url.clone(),
                    body_sha256: rule.sha256.clone(),
                },
            ],
        });
    }
    Ok((observations, snapshots.len()))
}

fn validate_parameter_record(record: &ParameterRow) -> Result<()> {
    if !matches!(record.source_kind.as_str(), "html" | "attachment") {
        bail!(
            "unexpected SHFE parameter source kind {}",
            record.source_kind
        );
    }
    for value in [&record.parent_url, &record.source_url] {
        let url = reqwest::Url::parse(value)?;
        if url.scheme() != "https"
            || url.host_str() != Some("www.shfe.com.cn")
            || !url
                .path()
                .starts_with("/reports/businessdata/adjtomonthlysettlementprm/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            bail!("unexpected SHFE monthly parameter URL {value}");
        }
    }
    verify_sha256_format(&record.sha256)?;
    if record.source_cell.trim().is_empty() {
        bail!("SHFE parameter row has no source cell");
    }
    Ok(())
}

fn parse_observation_date(reported_month: &str, value: &str, parent_url: &str) -> Result<Date> {
    if value.trim().is_empty() {
        return parse_parent_publication_date(parent_url);
    }
    let (year, report_month) = reported_month
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid SHFE reported month {reported_month}"))?;
    let year = year.parse::<i32>()?;
    let report_month = report_month.parse::<u8>()?;
    let digits = value
        .split_once('月')
        .and_then(|(month, rest)| rest.split_once('日').map(|(day, _)| (month, day)))
        .ok_or_else(|| anyhow!("invalid SHFE adjustment date {value}"))?;
    let month = digits.0.trim().parse::<u8>()?;
    let day = digits.1.trim().parse::<u8>()?;
    let year = if month > report_month { year - 1 } else { year };
    Date::from_calendar_date(year, Month::try_from(month)?, day)
        .with_context(|| format!("invalid SHFE adjustment date {value}"))
}

fn parse_parent_publication_date(value: &str) -> Result<Date> {
    let url = reqwest::Url::parse(value)?;
    let filename = url.path().rsplit('/').next().unwrap_or_default();
    let digits = filename
        .strip_prefix('t')
        .and_then(|value| value.get(..8))
        .filter(|value| value.bytes().all(|byte| byte.is_ascii_digit()))
        .ok_or_else(|| anyhow!("SHFE parent URL has no publication date {value}"))?;
    let format = time::format_description::parse("[year][month][day]")?;
    Date::parse(digits, &format)
        .with_context(|| format!("invalid SHFE parent publication date {value}"))
}

fn parse_general_fee(context: &str, raw: &str) -> Result<FeeSpec> {
    let value = raw.trim().parse::<f64>()?;
    if !value.is_finite() || value < 0.0 {
        bail!("invalid SHFE parameter fee {raw}");
    }
    if context.contains('‰') || context.contains("万分之") {
        return Ok(FeeSpec {
            kind: FeeKind::TurnoverRatePerTenThousand,
            value: Some(value * 10.0),
            raw_text: Some(format!("SHFE monthly {context}={raw}")),
        });
    }
    if context.contains("元/手") {
        return Ok(FeeSpec {
            kind: FeeKind::CnyPerLot,
            value: Some(value),
            raw_text: Some(format!("SHFE monthly {context}={raw}")),
        });
    }
    bail!("SHFE monthly fee unit is missing from {context}")
}

fn parse_explicit_fee(kind: &str, value: Option<f64>) -> Result<FeeSpec> {
    match kind {
        "Zero" => {
            if value.is_some_and(|value| value != 0.0) {
                bail!("SHFE zero close-today rule has a non-zero value");
            }
            Ok(FeeSpec {
                kind: FeeKind::Zero,
                value: Some(0.0),
                raw_text: None,
            })
        }
        "CnyPerLot" | "TurnoverRatePerTenThousand" => {
            let value = value
                .filter(|value| value.is_finite() && *value > 0.0)
                .ok_or_else(|| anyhow!("SHFE close-today rule needs a positive value"))?;
            Ok(FeeSpec {
                kind: if kind == "CnyPerLot" {
                    FeeKind::CnyPerLot
                } else {
                    FeeKind::TurnoverRatePerTenThousand
                },
                value: Some(value),
                raw_text: None,
            })
        }
        other => bail!("unknown SHFE close-today fee kind {other}"),
    }
}

fn select_rule<'a>(
    rules: &'a [CloseTodayRule],
    symbol: &str,
    observed_at: OffsetDateTime,
) -> Result<&'a CloseTodayRule> {
    let matches = rules
        .iter()
        .filter(|rule| {
            rule.symbol == symbol
                && rule.valid_from <= observed_at
                && rule.valid_to.is_none_or(|valid_to| observed_at < valid_to)
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [rule] => Ok(*rule),
        [] => bail!("SHFE close-today rule missing for {symbol} at {observed_at}"),
        _ => bail!("SHFE close-today rule is ambiguous for {symbol} at {observed_at}"),
    }
}

fn validate_symbol(value: &str) -> Result<()> {
    let Some(contract) = value.strip_prefix("SHFE.") else {
        bail!("invalid SHFE symbol {value}");
    };
    let letters = contract.bytes().take_while(u8::is_ascii_lowercase).count();
    if !(1..=2).contains(&letters)
        || contract.len() != letters + 4
        || !contract.as_bytes()[letters..]
            .iter()
            .all(u8::is_ascii_digit)
    {
        bail!("invalid SHFE symbol {value}");
    }
    Ok(())
}

fn validate_notice_url(value: &str) -> Result<()> {
    let url = reqwest::Url::parse(value)?;
    if url.scheme() != "https"
        || url.host_str() != Some("www.shfe.com.cn")
        || !url.path().starts_with("/publicnotice/notice/")
        || !Path::new(url.path())
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("html"))
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("SHFE close-today rule must reference a notice: {value}");
    }
    Ok(())
}

fn load_existing_metadata(
    connection: &rusqlite::Connection,
    symbol: &str,
) -> Result<Option<ExistingMetadata>> {
    Ok(connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size from contracts where symbol = ?1",
            [symbol],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?)
}

fn verify_retained_evidence(snapshot_dir: &Path, sha256: &str) -> Result<()> {
    verify_sha256_format(sha256)?;
    let prefix = format!("{sha256}.");
    let paths = std::fs::read_dir(snapshot_dir)?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
        })
        .collect::<Vec<_>>();
    if paths.len() != 1 {
        bail!(
            "SHFE evidence digest {sha256} resolved {} files",
            paths.len()
        );
    }
    let bytes = std::fs::read(&paths[0])?;
    if hex::encode(Sha256::digest(bytes)) != sha256 {
        bail!("retained SHFE SHA-256 mismatch for {}", paths[0].display());
    }
    Ok(())
}

fn verify_sha256_format(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid SHFE SHA-256 {value}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_observation_date;

    #[test]
    fn uses_parent_publication_date_when_monthly_cell_has_no_date() {
        let date = parse_observation_date(
            "2020-10",
            "",
            "https://www.shfe.com.cn/reports/businessdata/adjtomonthlysettlementprm/202009/t20200928_796480.html",
        )
        .unwrap();
        assert_eq!(date.to_string(), "2020-09-28");
    }
}
