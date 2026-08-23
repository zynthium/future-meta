//! Isolated staging store for manually obtained exchange evidence.

use crate::db;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::model::{FeeKind, FeeSpec};
use future_meta::symbol::{SymbolKind, parse_symbol};
use reqwest::Url;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// The type of an official document used to verify a fee adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    /// A dated exchange announcement that establishes an effective time.
    Notice,
    /// An exchange fee schedule or an attachment enumerating fee values.
    FeeSchedule,
    /// An exchange settlement or business-parameter file for an effective day.
    SettlementParameter,
}

impl EvidenceKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::FeeSchedule => "fee_schedule",
            Self::SettlementParameter => "settlement_parameter",
        }
    }
}

/// The cross-check state of a staged adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OfficialVerification {
    /// The adjustment has not yet been corroborated by two complementary documents.
    Provisional,
    /// A notice and a fee schedule or parameter file corroborate the adjustment.
    Verified,
}

impl OfficialVerification {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Provisional => "provisional",
            Self::Verified => "verified",
        }
    }
}

/// Immutable identity and provenance metadata for one official document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OfficialEvidence {
    /// The original exchange-hosted URL.  Only first-party exchange domains are accepted.
    pub canonical_url: String,
    /// Optional retrieval mirror, which is never treated as the canonical source.
    pub mirror_url: Option<String>,
    /// Lowercase SHA-256 digest of the exact retrieved document bytes.
    pub sha256: String,
    /// Official publication timestamp in RFC 3339 format.
    pub published_at: String,
    /// Documentary role in the cross-check.
    pub kind: EvidenceKind,
}

/// A fee change supported by one or more official documents.
///
/// Fields left as `None` were not changed or not stated by the cited documents.
/// This is deliberately an adjustment, rather than a complete production fee rule.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfficialFeeAdjustment {
    /// Concrete `TqSdk` futures contract symbol affected by the adjustment.
    pub symbol: String,
    /// First effective exchange-day timestamp in RFC 3339 format.
    pub effective_at: String,
    /// Human-readable contract scope from the official document.
    pub scope: String,
    /// Adjusted open fee, when the document states one.
    pub open_fee: Option<FeeSpec>,
    /// Adjusted close-yesterday fee, when the document states one.
    pub close_yesterday_fee: Option<FeeSpec>,
    /// Adjusted close-today fee, when the document states one.
    pub close_today_fee: Option<FeeSpec>,
    /// Complete fee tuple from the retained parameter immediately before the
    /// transition. Present only when repairing a premature baseline boundary.
    pub previous_fees: Option<[FeeSpec; 3]>,
    /// First-party documents used to verify this adjustment.
    pub evidence: Vec<OfficialEvidence>,
}

/// Result returned after safely staging an official adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedAdjustment {
    /// The cross-check state computed from the supplied documents.
    pub verification: OfficialVerification,
    /// Number of distinct supporting documents linked to the adjustment.
    pub evidence_count: usize,
}

/// Aggregate result for an atomically staged collection of adjustments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedAdjustments {
    /// Number of concrete-contract adjustments written to the evidence database.
    pub adjustments: usize,
    /// Number of adjustments accompanied by both required official document types.
    pub verified: usize,
}

/// Apply count returned by [`apply_verified_adjustments`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedOfficialAdjustments {
    /// Number of complete verified adjustments written to fee history.
    pub adjustments: usize,
    /// Number of linked broker candidates resolved by the applied evidence.
    pub resolved_candidates: usize,
}

/// Materialize complete, verified staged adjustments into a history database.
///
/// This command intentionally accepts no third-party source rows. Every
/// applied adjustment originates in the isolated official-evidence store and
/// must state the full open/close-yesterday/close-today tuple.
///
/// # Errors
///
/// Returns an error when a staged adjustment is incomplete, provisional, or
/// cannot be applied as a forward official version.
pub fn apply_verified_adjustments(
    history_db: &Path,
    evidence_db: &Path,
    observed_at: &str,
) -> Result<AppliedOfficialAdjustments> {
    let evidence = connect(evidence_db)?;
    ensure_schema(&evidence)?;
    let mut statement = evidence.prepare(
        "select id, symbol, effective_at, open_fee_json, close_yesterday_fee_json,
                close_today_fee_json, previous_fees_json
         from official_fee_adjustments where verification = 'verified'
         order by effective_at, symbol, scope",
    )?;
    let mut records = statement.query([])?;
    let mut staged = Vec::new();
    while let Some(record) = records.next()? {
        let adjustment_id: i64 = record.get(0)?;
        let symbol: String = record.get(1)?;
        let effective_at: String = record.get(2)?;
        let values = [
            record.get::<_, Option<String>>(3)?,
            record.get::<_, Option<String>>(4)?,
            record.get::<_, Option<String>>(5)?,
        ];
        let mut fees = Vec::with_capacity(3);
        for value in values {
            let Some(value) = value else {
                bail!("verified official adjustment requires complete fee tuple: {symbol}");
            };
            fees.push(serde_json::from_str::<FeeSpec>(&value)?);
        }
        let previous_fees = record
            .get::<_, Option<String>>(6)?
            .map(|value| serde_json::from_str::<[FeeSpec; 3]>(&value))
            .transpose()?;
        let mut evidence_statement = evidence.prepare(
            "select e.canonical_url, e.sha256 from official_evidence e
             join official_adjustment_evidence link on link.evidence_id = e.id
             where link.adjustment_id = ?1 order by e.canonical_url",
        )?;
        let evidence_urls = evidence_statement
            .query_map([adjustment_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
        staged.push((
            symbol,
            effective_at,
            [fees.remove(0), fees.remove(0), fees.remove(0)],
            previous_fees,
            evidence_urls,
        ));
    }
    if staged.is_empty() {
        bail!("no verified official adjustments staged");
    }
    let mut history = db::connect(history_db)?;
    db::ensure_schema(&history)?;
    for (symbol, _, _, _, evidence_urls) in &staged {
        for (url, sha256) in evidence_urls {
            let retained = history
                .query_row(
                    "select 1 from official_document_snapshots
                     where canonical_url = ?1 and body_sha256 = ?2",
                    rusqlite::params![url, sha256],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if !retained {
                bail!(
                    "official evidence snapshot is missing or hash-mismatched for {symbol}: {url}"
                );
            }
        }
    }
    for (symbol, effective_at, fees, previous_fees, _) in &staged {
        if let Some(previous_fees) = previous_fees {
            db::apply_official_fee_transition(
                &mut history,
                symbol,
                effective_at,
                previous_fees,
                fees,
                observed_at,
            )?;
        } else {
            db::apply_official_fee_tuple(&mut history, symbol, effective_at, fees, observed_at)?;
        }
    }
    let official_urls = staged
        .iter()
        .flat_map(|(_, _, _, _, evidence_urls)| evidence_urls.iter().map(|(url, _)| url.clone()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let resolved_candidates = db::resolve_announcement_candidates_for_official_urls(
        &history,
        &official_urls,
        observed_at,
    )?;
    Ok(AppliedOfficialAdjustments {
        adjustments: staged.len(),
        resolved_candidates,
    })
}

/// Insert or update a manually verified adjustment in the isolated evidence database.
///
/// This function never opens, migrates, or writes the production fee-history database.
/// It does not materialize an adjustment into `fee_versions`; that requires a later,
/// explicit review process after complete rule reconstruction.
///
/// # Errors
///
/// Returns an error if symbols, timestamps, fee units, source URLs, document digests,
/// or cross-document references are invalid.
pub fn stage_adjustment(
    path: &Path,
    adjustment: &OfficialFeeAdjustment,
) -> Result<StagedAdjustment> {
    validate_adjustment(adjustment)?;
    let mut conn = connect(path)?;
    ensure_schema(&conn)?;
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let tx = conn.transaction()?;
    let staged = stage_adjustment_in_transaction(&tx, adjustment, &now)?;
    tx.commit()?;
    Ok(staged)
}

/// Decode and atomically stage an array of official-evidence JSON documents.
///
/// Every object must have the [`OfficialFeeAdjustment`] shape.  All input is
/// validated before the transaction begins, so an invalid member cannot leave
/// a partial group of concrete contracts in the evidence database.
///
/// # Errors
///
/// Returns an error if the JSON is not a non-empty array, any adjustment is
/// invalid, or staging any member fails.
pub fn stage_adjustments_json(path: &Path, json: &str) -> Result<StagedAdjustments> {
    let adjustments: Vec<OfficialFeeAdjustment> =
        serde_json::from_str(json).context("invalid official adjustment batch JSON")?;
    if adjustments.is_empty() {
        bail!("official adjustment batch cannot be empty");
    }
    for adjustment in &adjustments {
        validate_adjustment(adjustment)?;
    }

    let mut conn = connect(path)?;
    ensure_schema(&conn)?;
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let tx = conn.transaction()?;
    let mut verified = 0usize;
    for adjustment in &adjustments {
        let staged = stage_adjustment_in_transaction(&tx, adjustment, &now)?;
        if staged.verification == OfficialVerification::Verified {
            verified += 1;
        }
    }
    tx.commit()?;

    Ok(StagedAdjustments {
        adjustments: adjustments.len(),
        verified,
    })
}

fn stage_adjustment_in_transaction(
    tx: &Transaction<'_>,
    adjustment: &OfficialFeeAdjustment,
    now: &str,
) -> Result<StagedAdjustment> {
    let verification = verification_for(adjustment);
    let evidence_count = adjustment.evidence.len();

    for evidence in &adjustment.evidence {
        let existing_sha = tx
            .query_row(
                "select sha256 from official_evidence where canonical_url = ?1",
                [&evidence.canonical_url],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(existing_sha) = existing_sha {
            if existing_sha != evidence.sha256 {
                bail!(
                    "official document changed at {}; expected immutable evidence",
                    evidence.canonical_url
                );
            }
        } else {
            tx.execute(
                "insert into official_evidence(
                    canonical_url, mirror_url, sha256, published_at, evidence_kind, recorded_at
                 ) values (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    evidence.canonical_url,
                    evidence.mirror_url,
                    evidence.sha256,
                    evidence.published_at,
                    evidence.kind.as_str(),
                    now,
                ],
            )?;
        }
    }

    tx.execute(
        "insert into official_fee_adjustments(
            symbol, effective_at, scope,
            open_fee_json, close_yesterday_fee_json, close_today_fee_json, previous_fees_json,
            verification, recorded_at
         ) values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         on conflict(symbol, effective_at, scope) do update set
            open_fee_json = excluded.open_fee_json,
            close_yesterday_fee_json = excluded.close_yesterday_fee_json,
            close_today_fee_json = excluded.close_today_fee_json,
            previous_fees_json = excluded.previous_fees_json,
            verification = excluded.verification,
            recorded_at = excluded.recorded_at",
        params![
            adjustment.symbol,
            adjustment.effective_at,
            adjustment.scope,
            optional_fee_json(adjustment.open_fee.as_ref())?,
            optional_fee_json(adjustment.close_yesterday_fee.as_ref())?,
            optional_fee_json(adjustment.close_today_fee.as_ref())?,
            adjustment
                .previous_fees
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
            verification.as_str(),
            now,
        ],
    )?;
    let adjustment_id = tx.query_row(
        "select id from official_fee_adjustments
         where symbol = ?1 and effective_at = ?2 and scope = ?3",
        params![adjustment.symbol, adjustment.effective_at, adjustment.scope],
        |row| row.get::<_, i64>(0),
    )?;

    tx.execute(
        "delete from official_adjustment_evidence where adjustment_id = ?1",
        [adjustment_id],
    )?;
    for evidence in &adjustment.evidence {
        let evidence_id = tx.query_row(
            "select id from official_evidence where canonical_url = ?1",
            [&evidence.canonical_url],
            |row| row.get::<_, i64>(0),
        )?;
        tx.execute(
            "insert or ignore into official_adjustment_evidence(adjustment_id, evidence_id)
             values (?1, ?2)",
            params![adjustment_id, evidence_id],
        )?;
    }
    Ok(StagedAdjustment {
        verification,
        evidence_count,
    })
}

/// Decode and stage one manually prepared official-evidence JSON document.
///
/// The JSON shape is [`OfficialFeeAdjustment`].  This entry point intentionally
/// performs no HTTP request: source files must be obtained through an approved
/// human-accessible channel, hashed, and reviewed before staging.
///
/// # Errors
///
/// Returns an error if the JSON cannot be decoded or the decoded adjustment
/// fails the same validation as [`stage_adjustment`].
pub fn stage_adjustment_json(path: &Path, json: &str) -> Result<StagedAdjustment> {
    let adjustment: OfficialFeeAdjustment =
        serde_json::from_str(json).context("invalid official adjustment JSON")?;
    stage_adjustment(path, &adjustment)
}

fn connect(path: &Path) -> Result<Connection> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let conn = Connection::open(path)?;
    conn.execute_batch("pragma foreign_keys = on;")?;
    Ok(conn)
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        pragma foreign_keys = on;

        create table if not exists official_evidence(
          id integer primary key,
          canonical_url text not null unique,
          mirror_url text,
          sha256 text not null check(length(sha256) = 64),
          published_at text not null,
          evidence_kind text not null check(evidence_kind in (
            'notice', 'fee_schedule', 'settlement_parameter'
          )),
          recorded_at text not null
        );

        create table if not exists official_fee_adjustments(
          id integer primary key,
          symbol text not null,
          effective_at text not null,
          scope text not null check(length(trim(scope)) > 0),
          open_fee_json text check(open_fee_json is null or json_valid(open_fee_json)),
          close_yesterday_fee_json text check(
            close_yesterday_fee_json is null or json_valid(close_yesterday_fee_json)
          ),
          close_today_fee_json text check(
            close_today_fee_json is null or json_valid(close_today_fee_json)
          ),
          previous_fees_json text check(
            previous_fees_json is null or json_valid(previous_fees_json)
          ),
          verification text not null check(verification in ('provisional', 'verified')),
          recorded_at text not null,
          unique(symbol, effective_at, scope)
        );

        create table if not exists official_adjustment_evidence(
          adjustment_id integer not null,
          evidence_id integer not null,
          primary key(adjustment_id, evidence_id),
          foreign key(adjustment_id) references official_fee_adjustments(id),
          foreign key(evidence_id) references official_evidence(id)
        );
        ",
    )?;
    let has_previous_fees = conn
        .prepare("pragma table_info(official_fee_adjustments)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|column| column == "previous_fees_json");
    if !has_previous_fees {
        conn.execute(
            "alter table official_fee_adjustments add column previous_fees_json text
             check(previous_fees_json is null or json_valid(previous_fees_json))",
            [],
        )?;
    }
    Ok(())
}

fn validate_adjustment(adjustment: &OfficialFeeAdjustment) -> Result<()> {
    let parsed = parse_symbol(&adjustment.symbol)?;
    if parsed.kind != SymbolKind::Futures {
        bail!("official adjustment requires a concrete futures symbol");
    }
    OffsetDateTime::parse(&adjustment.effective_at, &Rfc3339)?;
    if adjustment.scope.trim().is_empty() {
        bail!("official adjustment scope cannot be empty");
    }
    let fees = [
        adjustment.open_fee.as_ref(),
        adjustment.close_yesterday_fee.as_ref(),
        adjustment.close_today_fee.as_ref(),
    ];
    if fees.iter().all(Option::is_none) {
        bail!("official adjustment must state at least one fee");
    }
    for fee in fees.into_iter().flatten() {
        validate_fee(fee)?;
    }
    if let Some(previous_fees) = adjustment.previous_fees.as_ref() {
        for fee in previous_fees {
            validate_fee(fee)?;
        }
    }

    let allowed_domain = official_domain_for_exchange(&parsed.exchange)?;
    let mut urls = BTreeSet::new();
    for evidence in &adjustment.evidence {
        if !urls.insert(&evidence.canonical_url) {
            bail!("official adjustment contains duplicate evidence URLs");
        }
        validate_canonical_url(&evidence.canonical_url, allowed_domain)?;
        if let Some(mirror_url) = evidence.mirror_url.as_deref() {
            validate_https_url(mirror_url)?;
        }
        validate_sha256(&evidence.sha256)?;
        OffsetDateTime::parse(&evidence.published_at, &Rfc3339)?;
    }
    if adjustment.evidence.is_empty() {
        bail!("official adjustment requires at least one document");
    }
    if adjustment.previous_fees.is_some()
        && !paired_settlement_parameters_verify_complete_tuple(adjustment)
    {
        bail!("official transition repair requires paired settlement parameters");
    }
    Ok(())
}

fn official_domain_for_exchange(exchange: &str) -> Result<&'static str> {
    match exchange {
        "SHFE" => Ok("shfe.com.cn"),
        "INE" => Ok("ine.cn"),
        "DCE" => Ok("dce.com.cn"),
        "CZCE" => Ok("czce.com.cn"),
        "CFFEX" => Ok("cffex.com.cn"),
        "GFEX" => Ok("gfex.com.cn"),
        _ => Err(anyhow!("unsupported exchange {exchange}")),
    }
}

fn validate_canonical_url(value: &str, allowed_domain: &str) -> Result<()> {
    let url = Url::parse(value)?;
    // Some exchanges retain historical originals only on HTTP. Preserve their
    // canonical URL rather than inventing an HTTPS variant that cannot be
    // authenticated or retrieved.
    let allows_http = matches!(
        allowed_domain,
        "cffex.com.cn" | "dce.com.cn" | "gfex.com.cn"
    );
    if (url.scheme() != "https" && !(allows_http && url.scheme() == "http"))
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!(
            "evidence URL must be an absolute HTTPS URL without credentials, except for CFFEX/DCE/GFEX HTTP originals"
        );
    }
    let Some(host) = url.host_str() else {
        bail!("official evidence URL has no host");
    };
    let host = host.to_ascii_lowercase();
    if host != allowed_domain && host != format!("www.{allowed_domain}") {
        bail!("official evidence URL must use the {allowed_domain} primary domain");
    }
    Ok(())
}

fn validate_https_url(value: &str) -> Result<Url> {
    let url = Url::parse(value)?;
    if url.scheme() != "https"
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        bail!("evidence URL must be an absolute HTTPS URL without credentials");
    }
    Ok(url)
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        bail!("evidence SHA-256 must be 64 lowercase hexadecimal characters");
    }
    Ok(())
}

fn validate_fee(fee: &FeeSpec) -> Result<()> {
    match fee.kind {
        FeeKind::CnyPerLot | FeeKind::TurnoverRatePerTenThousand => {
            let Some(value) = fee.value else {
                bail!("official fee with a known unit must have a value");
            };
            if !value.is_finite() || value < 0.0 {
                bail!("official fee value must be finite and non-negative");
            }
        }
        FeeKind::Zero => {
            if fee.value != Some(0.0) {
                bail!("zero official fee must have numeric value zero");
            }
        }
        FeeKind::Unknown => bail!("unknown fee units cannot be staged as official evidence"),
    }
    if matches!(fee.raw_text.as_deref(), Some(text) if text.trim().is_empty()) {
        bail!("official fee raw text cannot be blank");
    }
    Ok(())
}

fn optional_fee_json(fee: Option<&FeeSpec>) -> Result<Option<String>> {
    fee.map(serde_json::to_string)
        .transpose()
        .map_err(Into::into)
}

fn verification_for(adjustment: &OfficialFeeAdjustment) -> OfficialVerification {
    let has_notice = adjustment
        .evidence
        .iter()
        .any(|item| item.kind == EvidenceKind::Notice);
    let has_schedule = adjustment.evidence.iter().any(|item| {
        matches!(
            item.kind,
            EvidenceKind::FeeSchedule | EvidenceKind::SettlementParameter
        )
    });
    if has_notice && has_schedule {
        return OfficialVerification::Verified;
    }

    if paired_settlement_parameters_verify_complete_tuple(adjustment) {
        return OfficialVerification::Verified;
    }

    OfficialVerification::Provisional
}

/// A retained pair of first-party daily settlement parameters can establish a
/// concrete change when the files bracket its effective exchange day. This is
/// deliberately narrower than ordinary schedule evidence: it accepts only a
/// complete fee tuple and never infers a product-wide rule for contracts that
/// are absent from either parameter file.
fn paired_settlement_parameters_verify_complete_tuple(adjustment: &OfficialFeeAdjustment) -> bool {
    if [
        adjustment.open_fee.as_ref(),
        adjustment.close_yesterday_fee.as_ref(),
        adjustment.close_today_fee.as_ref(),
    ]
    .iter()
    .any(Option::is_none)
    {
        return false;
    }

    let Ok(effective_at) = OffsetDateTime::parse(&adjustment.effective_at, &Rfc3339) else {
        return false;
    };
    let effective_date = effective_at.date();
    let mut dates = BTreeSet::new();
    let mut has_before = false;
    let mut has_on_or_after = false;

    for evidence in adjustment
        .evidence
        .iter()
        .filter(|item| item.kind == EvidenceKind::SettlementParameter)
    {
        let Ok(published_at) = OffsetDateTime::parse(&evidence.published_at, &Rfc3339) else {
            return false;
        };
        let date = published_at.date();
        dates.insert(date);
        has_before |= date < effective_date;
        has_on_or_after |= date >= effective_date;
    }

    dates.len() >= 2 && has_before && has_on_or_after
}
