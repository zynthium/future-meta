//! Strict historical coverage auditing for release review.

use anyhow::{Context, Result, bail};
use future_meta::model::{FeeKind, FeeSpec};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::json;
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

/// Inclusive exchange-date boundary for a coverage audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoverageBoundary {
    /// First date that must be covered.
    pub from: Date,
    /// Last date that must be covered.
    pub through: Date,
}

impl CoverageBoundary {
    /// Parse an inclusive ISO-date audit boundary.
    ///
    /// # Errors
    ///
    /// Returns an error when either date is malformed or the range is inverted.
    pub fn parse(from: &str, through: &str) -> Result<Self> {
        let format = time::format_description::parse("[year]-[month]-[day]")?;
        let boundary = Self {
            from: Date::parse(from, &format).context("invalid coverage start date")?,
            through: Date::parse(through, &format).context("invalid coverage end date")?,
        };
        if boundary.from > boundary.through {
            bail!("coverage start must not be after coverage end");
        }
        Ok(boundary)
    }
}

/// Machine-readable reason a contract cannot support a strict coverage claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageFindingKind {
    /// Official listing date is unavailable.
    MissingListingDate,
    /// Official expiry date is unavailable.
    MissingExpiryDate,
    /// Contract has no fee history.
    MissingFeeHistory,
    /// Contract has no lot-size and tick-size history.
    MissingSpecificationHistory,
    /// Official fee row has no retained evidence link.
    MissingFeeEvidence,
    /// Official specification row has no retained evidence link.
    MissingSpecificationEvidence,
    /// Lifecycle boundaries have no retained official evidence link.
    MissingLifecycleEvidence,
    /// A fee interval leaves part of the listed lifetime uncovered.
    FeeCoverageGap,
    /// A specification interval leaves part of the listed lifetime uncovered.
    SpecificationCoverageGap,
    /// Fee intervals overlap and therefore have ambiguous precedence.
    FeeIntervalOverlap,
    /// Specification intervals overlap and therefore have ambiguous precedence.
    SpecificationIntervalOverlap,
    /// A fee interval used by the claim lacks official provenance.
    NonOfficialFeeSource,
    /// A specification interval used by the claim lacks official provenance.
    NonOfficialSpecificationSource,
    /// A fee interval contains an unknown or invalid numeric value.
    InvalidFeeValue,
    /// A specification interval contains an invalid lot size or tick size.
    InvalidSpecificationValue,
    /// Listing date exists but is malformed.
    InvalidListingDate,
    /// Expiry date exists but is malformed.
    InvalidExpiryDate,
    /// Fee validity timestamps are malformed.
    InvalidFeeInterval,
    /// Specification validity timestamps are malformed.
    InvalidSpecificationInterval,
}

impl CoverageFindingKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::MissingListingDate => "missing_listing_date",
            Self::MissingExpiryDate => "missing_expiry_date",
            Self::MissingFeeHistory => "missing_fee_history",
            Self::MissingSpecificationHistory => "missing_specification_history",
            Self::MissingFeeEvidence => "missing_fee_evidence",
            Self::MissingSpecificationEvidence => "missing_specification_evidence",
            Self::MissingLifecycleEvidence => "missing_lifecycle_evidence",
            Self::FeeCoverageGap => "fee_coverage_gap",
            Self::SpecificationCoverageGap => "specification_coverage_gap",
            Self::FeeIntervalOverlap => "fee_interval_overlap",
            Self::SpecificationIntervalOverlap => "specification_interval_overlap",
            Self::NonOfficialFeeSource => "non_official_fee_source",
            Self::NonOfficialSpecificationSource => "non_official_specification_source",
            Self::InvalidFeeValue => "invalid_fee_value",
            Self::InvalidSpecificationValue => "invalid_specification_value",
            Self::InvalidListingDate => "invalid_listing_date",
            Self::InvalidExpiryDate => "invalid_expiry_date",
            Self::InvalidFeeInterval => "invalid_fee_interval",
            Self::InvalidSpecificationInterval => "invalid_specification_interval",
        }
    }
}

/// One strict-coverage failure associated with a concrete contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageFinding {
    /// TqSdk-style concrete contract symbol.
    pub symbol: String,
    /// Stable finding category.
    pub kind: CoverageFindingKind,
    /// Human-readable audit detail.
    pub detail: String,
}

/// Aggregate result of a strict history coverage audit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Requested audit boundary.
    pub boundary: CoverageBoundary,
    /// Number of contracts inspected.
    pub contracts: usize,
    /// Number of contracts without findings.
    pub complete_contracts: usize,
    /// Deterministically ordered coverage failures.
    pub findings: Vec<CoverageFinding>,
}

/// Audit lifecycle metadata and the existence of both required history tables.
///
/// # Errors
///
/// Returns an error for an inverted boundary or an unreadable history database.
pub fn audit_history_coverage(
    connection: &Connection,
    boundary: CoverageBoundary,
) -> Result<CoverageReport> {
    if boundary.from > boundary.through {
        bail!("coverage start must not be after coverage end");
    }

    let mut statement = connection.prepare(
        "select c.id, c.symbol, c.listing_date, c.expiry_date
         from contracts c order by c.symbol",
    )?;
    let contracts = statement
        .query_map([], |row| {
            Ok(ContractLifecycle {
                id: row.get(0)?,
                symbol: row.get(1)?,
                listing_date: row.get(2)?,
                expiry_date: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut findings = Vec::new();
    let mut complete_contracts = 0usize;
    let mut inspected_contracts = 0usize;
    for contract in &contracts {
        if let Some(complete) = audit_contract(connection, contract, boundary, &mut findings)? {
            inspected_contracts += 1;
            complete_contracts += usize::from(complete);
        }
    }

    Ok(CoverageReport {
        boundary,
        contracts: inspected_contracts,
        complete_contracts,
        findings,
    })
}

#[derive(Debug)]
struct ContractLifecycle {
    id: i64,
    symbol: String,
    listing_date: Option<String>,
    expiry_date: Option<String>,
}

fn audit_contract(
    connection: &Connection,
    contract: &ContractLifecycle,
    boundary: CoverageBoundary,
    findings: &mut Vec<CoverageFinding>,
) -> Result<Option<bool>> {
    let finding_start = findings.len();
    let listing = parse_lifecycle_date(
        &contract.symbol,
        contract.listing_date.as_deref(),
        CoverageFindingKind::MissingListingDate,
        CoverageFindingKind::InvalidListingDate,
        "listing",
        findings,
    );
    let expiry = parse_lifecycle_date(
        &contract.symbol,
        contract.expiry_date.as_deref(),
        CoverageFindingKind::MissingExpiryDate,
        CoverageFindingKind::InvalidExpiryDate,
        "expiry",
        findings,
    );

    if matches!((listing, expiry), (Some(listing), Some(expiry)) if expiry < boundary.from || listing > boundary.through)
    {
        findings.truncate(finding_start);
        return Ok(None);
    }

    if let (Some(listing_date), Some(expiry_date)) = (
        contract.listing_date.as_deref(),
        contract.expiry_date.as_deref(),
    ) {
        let retained = connection
            .query_row(
                "select 1 from contract_lifecycle_evidence
                 where contract_id = ?1 and listing_date = ?2 and expiry_date = ?3
                   and length(trim(canonical_url)) > 0 and length(body_sha256) = 64
                 limit 1",
                params![contract.id, listing_date, expiry_date],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !retained {
            push_finding_once(
                findings,
                &contract.symbol,
                CoverageFindingKind::MissingLifecycleEvidence,
                "lifecycle boundaries lack retained official evidence".to_owned(),
            );
        }
    }

    let scope_start = listing.map_or(boundary.from, |date| date.max(boundary.from));
    let scope_through = expiry.map_or(boundary.through, |date| date.min(boundary.through));
    let scope_start = exchange_day_start(scope_start)?;
    let scope_end = exchange_day_start(
        scope_through
            .next_day()
            .context("coverage boundary exceeds supported date range")?,
    )?;
    audit_fee_history(connection, contract, scope_start, scope_end, findings)?;
    audit_specification_history(connection, contract, scope_start, scope_end, findings)?;
    Ok(Some(findings.len() == finding_start))
}

fn audit_fee_history(
    connection: &Connection,
    contract: &ContractLifecycle,
    scope_start: OffsetDateTime,
    scope_end: OffsetDateTime,
    findings: &mut Vec<CoverageFinding>,
) -> Result<()> {
    let intervals = load_fee_intervals(connection, contract.id, &contract.symbol, findings)?;
    if intervals.is_empty() {
        findings.push(CoverageFinding {
            symbol: contract.symbol.clone(),
            kind: CoverageFindingKind::MissingFeeHistory,
            detail: "fee history is missing".to_owned(),
        });
    } else {
        audit_intervals(
            &contract.symbol,
            &intervals,
            scope_start,
            scope_end,
            CoverageFindingKind::FeeCoverageGap,
            CoverageFindingKind::FeeIntervalOverlap,
            CoverageFindingKind::NonOfficialFeeSource,
            CoverageFindingKind::MissingFeeEvidence,
            CoverageFindingKind::InvalidFeeValue,
            "fee",
            findings,
        );
    }
    Ok(())
}

fn audit_specification_history(
    connection: &Connection,
    contract: &ContractLifecycle,
    scope_start: OffsetDateTime,
    scope_end: OffsetDateTime,
    findings: &mut Vec<CoverageFinding>,
) -> Result<()> {
    let intervals =
        load_specification_intervals(connection, contract.id, &contract.symbol, findings)?;
    if intervals.is_empty() {
        findings.push(CoverageFinding {
            symbol: contract.symbol.clone(),
            kind: CoverageFindingKind::MissingSpecificationHistory,
            detail: "contract specification history is missing".to_owned(),
        });
    } else {
        audit_intervals(
            &contract.symbol,
            &intervals,
            scope_start,
            scope_end,
            CoverageFindingKind::SpecificationCoverageGap,
            CoverageFindingKind::SpecificationIntervalOverlap,
            CoverageFindingKind::NonOfficialSpecificationSource,
            CoverageFindingKind::MissingSpecificationEvidence,
            CoverageFindingKind::InvalidSpecificationValue,
            "contract specification",
            findings,
        );
    }
    Ok(())
}

/// Audit a database, persist a deterministic JSON report, and optionally fail closed.
///
/// The history database is opened read-only. The report is always written before
/// strict-mode failure so reviewers can inspect every blocking finding.
///
/// # Errors
///
/// Returns an error when the database or output cannot be read/written, or when
/// strict mode finds incomplete coverage.
pub fn audit_history_coverage_to_path(
    database: &Path,
    boundary: CoverageBoundary,
    output: &Path,
    strict: bool,
) -> Result<CoverageReport> {
    let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("open coverage database {}", database.display()))?;
    let report = audit_history_coverage(&connection, boundary)?;
    let findings = report
        .findings
        .iter()
        .map(|finding| {
            json!({
                "symbol": finding.symbol,
                "kind": finding.kind.as_str(),
                "detail": finding.detail,
            })
        })
        .collect::<Vec<_>>();
    let document = json!({
        "boundary": {
            "from": report.boundary.from.to_string(),
            "through": report.boundary.through.to_string(),
        },
        "contracts": report.contracts,
        "complete_contracts": report.complete_contracts,
        "findings": findings,
    });
    if let Some(parent) = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = serde_json::to_vec_pretty(&document)?;
    bytes.push(b'\n');
    std::fs::write(output, bytes)?;

    if strict && !report.findings.is_empty() {
        bail!(
            "strict coverage failed: contracts={} complete={} findings={}",
            report.contracts,
            report.complete_contracts,
            report.findings.len()
        );
    }
    Ok(report)
}

#[derive(Debug)]
struct AuditInterval {
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
    official: bool,
    evidence: bool,
    value_valid: bool,
}

#[allow(clippy::too_many_arguments)]
fn audit_intervals(
    symbol: &str,
    intervals: &[AuditInterval],
    scope_start: OffsetDateTime,
    scope_end: OffsetDateTime,
    gap_kind: CoverageFindingKind,
    overlap_kind: CoverageFindingKind,
    source_kind: CoverageFindingKind,
    evidence_kind: CoverageFindingKind,
    value_kind: CoverageFindingKind,
    label: &str,
    findings: &mut Vec<CoverageFinding>,
) {
    let mut expected = scope_start;
    let mut used_interval = false;
    for interval in intervals {
        let interval_end = interval.valid_to.unwrap_or(scope_end);
        if interval_end <= scope_start || interval.valid_from >= scope_end {
            continue;
        }

        if used_interval && interval.valid_from < expected {
            push_finding_once(
                findings,
                symbol,
                overlap_kind,
                format!("{label} intervals overlap at {}", interval.valid_from),
            );
        } else if interval.valid_from > expected {
            push_finding_once(
                findings,
                symbol,
                gap_kind,
                format!("{label} history is uncovered at {expected}"),
            );
        }
        if !interval.official {
            push_finding_once(
                findings,
                symbol,
                source_kind,
                format!("{label} interval lacks official provenance"),
            );
        }
        if interval.official && !interval.evidence {
            push_finding_once(
                findings,
                symbol,
                evidence_kind,
                format!("{label} interval lacks retained official evidence"),
            );
        }
        if !interval.value_valid {
            push_finding_once(
                findings,
                symbol,
                value_kind,
                format!("{label} interval contains an invalid value"),
            );
        }

        expected = expected.max(interval_end.min(scope_end));
        used_interval = true;
    }
    if !used_interval || expected < scope_end {
        push_finding_once(
            findings,
            symbol,
            gap_kind,
            format!("{label} history does not reach {scope_end}"),
        );
    }
}

fn load_fee_intervals(
    connection: &Connection,
    contract_id: i64,
    symbol: &str,
    findings: &mut Vec<CoverageFinding>,
) -> Result<Vec<AuditInterval>> {
    let mut statement = connection.prepare(
        "select v.valid_from, v.valid_to, v.source_kind,
                v.open_fee_json, v.close_yesterday_fee_json, v.close_today_fee_json,
                exists(
                  select 1 from fee_version_evidence e
                  where e.contract_id = v.contract_id
                    and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
                    and length(trim(e.canonical_url)) > 0 and length(e.body_sha256) = 64
                )
         from fee_versions v where v.contract_id = ?1 order by v.valid_from",
    )?;
    let records = statement
        .query_map([contract_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut intervals = Vec::with_capacity(records.len());
    for (valid_from, valid_to, source, open, close_yesterday, close_today, evidence) in records {
        let Some((valid_from, valid_to)) = parse_interval(
            symbol,
            &valid_from,
            valid_to.as_deref(),
            CoverageFindingKind::InvalidFeeInterval,
            "fee",
            findings,
        ) else {
            continue;
        };
        let value_valid = [&open, &close_yesterday, &close_today]
            .into_iter()
            .all(|json| valid_fee_json(json));
        intervals.push(AuditInterval {
            valid_from,
            valid_to,
            official: source == "official",
            evidence,
            value_valid,
        });
    }
    intervals.sort_by_key(|interval| interval.valid_from);
    Ok(intervals)
}

fn load_specification_intervals(
    connection: &Connection,
    contract_id: i64,
    symbol: &str,
    findings: &mut Vec<CoverageFinding>,
) -> Result<Vec<AuditInterval>> {
    let mut statement = connection.prepare(
        "select s.valid_from, s.valid_to, s.source_kind, s.source_url,
                s.lot_size, s.tick_size,
                exists(
                  select 1 from contract_spec_evidence e
                  where e.contract_id = s.contract_id and e.valid_from = s.valid_from
                    and length(trim(e.canonical_url)) > 0 and length(e.body_sha256) = 64
                )
         from contract_spec_versions s where s.contract_id = ?1 order by s.valid_from",
    )?;
    let records = statement
        .query_map(params![contract_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, f64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, bool>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut intervals = Vec::with_capacity(records.len());
    for (valid_from, valid_to, source, source_url, lot_size, tick_size, evidence) in records {
        let Some((valid_from, valid_to)) = parse_interval(
            symbol,
            &valid_from,
            valid_to.as_deref(),
            CoverageFindingKind::InvalidSpecificationInterval,
            "contract specification",
            findings,
        ) else {
            continue;
        };
        intervals.push(AuditInterval {
            valid_from,
            valid_to,
            official: source == "official"
                && source_url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty()),
            evidence,
            value_valid: lot_size.is_finite()
                && lot_size > 0.0
                && tick_size.is_finite()
                && tick_size > 0.0,
        });
    }
    intervals.sort_by_key(|interval| interval.valid_from);
    Ok(intervals)
}

fn parse_interval(
    symbol: &str,
    valid_from: &str,
    valid_to: Option<&str>,
    finding_kind: CoverageFindingKind,
    label: &str,
    findings: &mut Vec<CoverageFinding>,
) -> Option<(OffsetDateTime, Option<OffsetDateTime>)> {
    let Ok(valid_from) = OffsetDateTime::parse(valid_from, &Rfc3339) else {
        push_finding_once(
            findings,
            symbol,
            finding_kind,
            format!("{label} valid_from is not RFC 3339"),
        );
        return None;
    };
    let valid_to = if let Some(value) = valid_to {
        let Ok(value) = OffsetDateTime::parse(value, &Rfc3339) else {
            push_finding_once(
                findings,
                symbol,
                finding_kind,
                format!("{label} valid_to is not RFC 3339"),
            );
            return None;
        };
        Some(value)
    } else {
        None
    };
    Some((valid_from, valid_to))
}

fn valid_fee_json(json: &str) -> bool {
    let Ok(fee) = serde_json::from_str::<FeeSpec>(json) else {
        return false;
    };
    match (fee.kind, fee.value) {
        (FeeKind::CnyPerLot | FeeKind::TurnoverRatePerTenThousand, Some(value)) => {
            value.is_finite() && value >= 0.0
        }
        (FeeKind::Zero, Some(value)) => value == 0.0,
        (FeeKind::Unknown, _) | (_, None) => false,
    }
}

fn parse_lifecycle_date(
    symbol: &str,
    value: Option<&str>,
    missing_kind: CoverageFindingKind,
    invalid_kind: CoverageFindingKind,
    label: &str,
    findings: &mut Vec<CoverageFinding>,
) -> Option<Date> {
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        findings.push(CoverageFinding {
            symbol: symbol.to_owned(),
            kind: missing_kind,
            detail: format!("official {label} date is missing"),
        });
        return None;
    };
    let format = if value.len() == 8 {
        "[year][month][day]"
    } else {
        "[year]-[month]-[day]"
    };
    let Ok(parsed_format) = time::format_description::parse(format) else {
        return None;
    };
    if let Ok(date) = Date::parse(value, &parsed_format) {
        Some(date)
    } else {
        findings.push(CoverageFinding {
            symbol: symbol.to_owned(),
            kind: invalid_kind,
            detail: format!("official {label} date is invalid: {value}"),
        });
        None
    }
}

fn exchange_day_start(date: Date) -> Result<OffsetDateTime> {
    Ok(date.midnight().assume_offset(UtcOffset::from_hms(8, 0, 0)?))
}

fn push_finding_once(
    findings: &mut Vec<CoverageFinding>,
    symbol: &str,
    kind: CoverageFindingKind,
    detail: String,
) {
    if findings
        .iter()
        .any(|finding| finding.symbol == symbol && finding.kind == kind)
    {
        return;
    }
    findings.push(CoverageFinding {
        symbol: symbol.to_owned(),
        kind,
        detail,
    });
}
