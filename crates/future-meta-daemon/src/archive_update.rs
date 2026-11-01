//! Business-history updates using the previous client archive as the baseline.

// Exact normalized source values identify changes; an epsilon would hide changes.
#![allow(clippy::float_cmp)]

use crate::{hash::row_rule_hash, model::AllowedRow};
use anyhow::{Context, Result, ensure};
use future_meta::{
    FutureMeta,
    archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex},
    model::{
        Contract, ContractFee, ContractSpecVersion, FeeArchiveV3, FeeKind, FeeSpec, Manifest,
        SCHEMA_VERSION,
    },
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};
use time::{
    Date, OffsetDateTime, UtcOffset,
    format_description::{self, well_known::Rfc3339},
};

const EXCHANGES: [&str; 6] = ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"];

#[derive(Deserialize)]
struct Batch {
    baseline_sha256: String,
    previous_through: String,
    trading_days: Vec<String>,
    generated_at: String,
    snapshots: Vec<Snapshot>,
}

#[derive(Deserialize)]
struct Snapshot {
    exchange: String,
    trade_date: String,
    rows: Vec<Parameter>,
}

#[derive(Deserialize)]
struct Parameter {
    #[serde(flatten)]
    row: AllowedRow,
    spec_effective_at: Option<String>,
    spec_trade_date: Option<String>,
}

/// Validate a complete interval, append business changes, and stage publication.
///
/// # Errors
/// Rejects an invalid baseline, missing snapshots/contracts, incomplete fields,
/// historical rewrites, or an existing destination. No output is created until
/// the entire batch and resulting query index have passed validation.
#[allow(clippy::too_many_lines)]
pub fn update(archive: &Path, input: &Path, out: &Path) -> Result<Value> {
    ensure!(!out.exists(), "output already exists: {}", out.display());
    let original = std::fs::read(archive)?;
    let batch: Batch = serde_json::from_slice(&std::fs::read(input)?)?;
    ensure!(
        sha256_hex(&original) == batch.baseline_sha256,
        "archive/checkpoint SHA mismatch"
    );
    let mut payload = decode_archive_bytes(&original)?;
    FutureMeta::from_archive(payload.clone()).context("validate baseline")?;
    ensure!(
        payload.schema_version == SCHEMA_VERSION,
        "updates require current archive schema"
    );
    OffsetDateTime::parse(&batch.generated_at, &Rfc3339)?;
    let previous = day(&batch.previous_through)?;
    let mut prior = previous;
    for value in &batch.trading_days {
        let current = day(value)?;
        ensure!(
            current > prior && current.weekday().number_from_monday() <= 5,
            "unordered/non-trading batch day"
        );
        prior = current;
    }
    let through = batch
        .trading_days
        .last()
        .context("empty trading interval")?;
    let expected: BTreeSet<_> = batch
        .trading_days
        .iter()
        .flat_map(|day| EXCHANGES.map(|exchange| (day.as_str(), exchange)))
        .collect();
    let actual: BTreeSet<_> = batch
        .snapshots
        .iter()
        .map(|s| (s.trade_date.as_str(), s.exchange.as_str()))
        .collect();
    ensure!(
        actual == expected && actual.len() == batch.snapshots.len(),
        "missing or duplicate official daily snapshots"
    );
    let mut snapshots: Vec<_> = batch.snapshots.iter().collect();
    snapshots.sort_by_key(|s| (&s.trade_date, &s.exchange));
    let mut changed = false;
    for snapshot in snapshots {
        changed |= apply_snapshot(&mut payload, snapshot, previous)?;
    }
    for contract in &mut payload.contracts {
        let active = contract
            .listing_date
            .as_ref()
            .is_some_and(|v| day(v).is_ok_and(|d| d <= prior))
            && contract
                .expiry_date
                .as_ref()
                .is_some_and(|v| day(v).is_ok_and(|d| d >= prior));
        changed |= contract.active != active;
        contract.active = active;
    }
    let bytes = if changed {
        payload.generated_at.clone_from(&batch.generated_at);
        payload.history_start.clone_from(
            payload
                .fee_versions
                .iter()
                .map(|v| &v.valid_from)
                .min()
                .context("no fee history")?,
        );
        payload.history_end.clone_from(
            payload
                .fee_versions
                .iter()
                .map(|v| &v.valid_from)
                .max()
                .context("no fee history")?,
        );
        FutureMeta::from_archive(payload.clone()).context("validate updated archive")?;
        encode_archive_bytes(&payload)?
    } else {
        original
    };
    stage(&payload, &bytes, out, through, changed)
}

/// Append reviewed, disjoint contract histories without rewriting the baseline.
/// This local maintenance operation never collects or publishes data.
///
/// # Errors
/// Rejects a changed baseline, overlapping symbols, invalid supplemental history,
/// observations after the checkpoint, or an existing destination.
pub fn extend(
    archive: &Path,
    supplement: &Path,
    expected_sha256: &str,
    checked_through: &str,
    out: &Path,
) -> Result<Value> {
    ensure!(!out.exists(), "output already exists: {}", out.display());
    let original = std::fs::read(archive)?;
    ensure!(
        sha256_hex(&original) == expected_sha256,
        "archive/checkpoint SHA mismatch"
    );
    let mut payload = decode_archive_bytes(&original)?;
    let mut additions = decode_archive_bytes(&std::fs::read(supplement)?)?;
    ensure!(
        payload.schema_version == SCHEMA_VERSION && additions.schema_version == SCHEMA_VERSION,
        "extensions require current archive schema"
    );
    FutureMeta::from_archive(payload.clone()).context("validate baseline")?;
    FutureMeta::from_archive(additions.clone()).context("validate supplemental archive")?;
    ensure!(
        !additions.contracts.is_empty(),
        "empty supplemental archive"
    );
    let through = day(checked_through)?;
    validate_supplement(&additions, through)?;
    let symbols: BTreeSet<_> = payload
        .contracts
        .iter()
        .map(|c| c.symbol.as_str())
        .collect();
    let mut next_id = payload.contracts.iter().map(|c| c.id).max().unwrap_or(0);
    let mut ids = BTreeMap::new();
    for contract in &mut additions.contracts {
        ensure!(
            !symbols.contains(contract.symbol.as_str()),
            "supplement overlaps existing contract: {}",
            contract.symbol
        );
        let listing = day(contract
            .listing_date
            .as_deref()
            .context("missing supplemental listing")?)?;
        let expiry = day(contract
            .expiry_date
            .as_deref()
            .context("missing supplemental expiry")?)?;
        ensure!(
            listing <= through && listing <= expiry,
            "supplemental lifecycle outside checkpoint"
        );
        next_id = next_id.checked_add(1).context("contract id overflow")?;
        ids.insert(contract.id, next_id);
        contract.id = next_id;
        contract.active = listing <= through && through <= expiry;
    }
    for version in &mut additions.fee_versions {
        version.contract_id = ids[&version.contract_id];
    }
    for version in &mut additions.contract_spec_versions {
        version.contract_id = ids[&version.contract_id];
    }
    payload.contracts.extend(additions.contracts);
    payload.fee_versions.extend(additions.fee_versions);
    payload
        .contract_spec_versions
        .extend(additions.contract_spec_versions);
    payload.generated_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    payload.history_start = payload
        .fee_versions
        .iter()
        .map(|v| v.valid_from.clone())
        .min()
        .context("no fee history")?;
    payload.history_end = payload
        .fee_versions
        .iter()
        .map(|v| v.valid_from.clone())
        .max()
        .context("no fee history")?;
    FutureMeta::from_archive(payload.clone()).context("validate extended archive")?;
    let bytes = encode_archive_bytes(&payload)?;
    stage(&payload, &bytes, out, checked_through, true)
}

fn validate_supplement(archive: &FeeArchiveV3, through: Date) -> Result<()> {
    for contract in &archive.contracts {
        let expiry = day(contract.expiry_date.as_deref().context("missing expiry")?)?;
        let cap = OffsetDateTime::parse(&after_expiry(&expiry.to_string())?, &Rfc3339)?;
        let exchange = contract.symbol.split_once('.').context("invalid symbol")?.0;
        let mut intervals = Vec::new();
        for fee in archive
            .fee_versions
            .iter()
            .filter(|v| v.contract_id == contract.id)
        {
            let start = OffsetDateTime::parse(&fee.valid_from, &Rfc3339)?;
            let observed = start.to_offset(UtcOffset::from_hms(8, 0, 0)?).date();
            ensure!(observed <= through, "supplemental fee after checkpoint");
            validate(
                &AllowedRow {
                    symbol: contract.symbol.clone(),
                    listing_date: contract.listing_date.clone(),
                    expiry_date: contract.expiry_date.clone(),
                    lot_size: contract.lot_size,
                    tick_size: contract.tick_size,
                    buy_margin_rate: fee.buy_margin_rate,
                    sell_margin_rate: fee.sell_margin_rate,
                    open_fee: fee.open_fee.clone(),
                    close_yesterday_fee: fee.close_yesterday_fee.clone(),
                    close_today_fee: fee.close_today_fee.clone(),
                    intraday_open_fee: fee.intraday_open_fee.clone(),
                    trading_status: fee.trading_status.clone(),
                    is_main_contract: fee.is_main_contract,
                    source_updated_at: fee.source_updated_at.clone(),
                },
                exchange,
                &observed.to_string(),
            )?;
            let end = fee
                .valid_to
                .as_deref()
                .map(|v| OffsetDateTime::parse(v, &Rfc3339))
                .transpose()?;
            ensure!(
                end.is_none_or(|end| start < end && end <= cap),
                "supplemental fee exceeds lifecycle"
            );
            ensure!(
                expiry > through || end.is_some(),
                "expired supplemental fee has no cap"
            );
            intervals.push((start, end));
        }
        ensure!(
            !intervals.is_empty(),
            "supplemental contract has no observed fees"
        );
        ensure!(
            intervals
                .windows(2)
                .all(|p| p[0].0 < p[1].0 && p[0].1.is_some_and(|end| end <= p[1].0)),
            "unordered/overlapping supplemental fees"
        );
        let mut specs = Vec::new();
        for spec in archive
            .contract_spec_versions
            .iter()
            .filter(|v| v.contract_id == contract.id)
        {
            let start = OffsetDateTime::parse(&spec.valid_from, &Rfc3339)?;
            let end = OffsetDateTime::parse(
                spec.valid_to
                    .as_deref()
                    .context("uncapped supplemental specification")?,
                &Rfc3339,
            )?;
            ensure!(
                start < end
                    && end <= cap
                    && spec.lot_size.is_finite()
                    && spec.lot_size > 0.0
                    && spec.tick_size.is_finite()
                    && spec.tick_size > 0.0,
                "invalid supplemental specification"
            );
            specs.push((start, end));
        }
        let listing = OffsetDateTime::parse(
            &format!(
                "{}T00:00:00+08:00",
                day(contract
                    .listing_date
                    .as_deref()
                    .context("missing listing")?)?
            ),
            &Rfc3339,
        )?;
        ensure!(
            specs.first().is_some_and(|v| v.0 <= listing)
                && specs.last().is_some_and(|v| v.1 == cap)
                && specs.windows(2).all(|p| p[0].1 == p[1].0),
            "incomplete supplemental specifications"
        );
    }
    Ok(())
}

fn stage(
    payload: &FeeArchiveV3,
    bytes: &[u8],
    out: &Path,
    through: &str,
    changed: bool,
) -> Result<Value> {
    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        data_version: payload.generated_at.clone(),
        generated_at: payload.generated_at.clone(),
        history_start: payload.history_start.clone(),
        history_end: payload.history_end.clone(),
        fee_effective_from: Some(payload.history_end.clone()),
        artifact: "latest.fmeta.zst".into(),
        sha256: sha256_hex(bytes),
        size: u64::try_from(bytes.len())?,
        mirrors: Vec::new(),
    };
    let report = json!({"changed": changed, "checked_through": through, "sha256": manifest.sha256,
        "contracts": payload.contracts.len(), "specification_versions": payload.contract_spec_versions.len(), "fee_versions": payload.fee_versions.len()});
    std::fs::create_dir_all(out)?;
    std::fs::write(out.join("latest.fmeta.zst"), bytes)?;
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    std::fs::write(
        out.join("update-report.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}

fn day(value: &str) -> Result<Date> {
    let format = if value.len() == 8 {
        "[year][month][day]"
    } else {
        "[year]-[month]-[day]"
    };
    Ok(Date::parse(value, &format_description::parse(format)?)?)
}

fn timestamp_day(value: &str) -> Result<Date> {
    Ok(OffsetDateTime::parse(value, &Rfc3339)?.date())
}

fn validate(row: &AllowedRow, exchange: &str, observed: &str) -> Result<()> {
    let parsed = future_meta::symbol::parse_symbol(&row.symbol)?;
    ensure!(
        parsed.kind == future_meta::symbol::SymbolKind::Futures
            && row.symbol.starts_with(&format!("{exchange}.")),
        "invalid exchange/symbol: {}",
        row.symbol
    );
    let listing = row
        .listing_date
        .as_deref()
        .context("missing listing date")?;
    let expiry = row.expiry_date.as_deref().context("missing expiry date")?;
    day(listing)?;
    day(expiry)?;
    ensure!(
        day(listing)? <= day(observed)? && day(observed)? <= day(expiry)?,
        "observation outside official lifecycle: {} {observed}",
        row.symbol
    );
    ensure!(
        row.lot_size.is_finite()
            && row.lot_size > 0.0
            && row.tick_size.is_finite()
            && row.tick_size > 0.0,
        "invalid specification"
    );
    for margin in [row.buy_margin_rate, row.sell_margin_rate] {
        ensure!(
            margin.is_some_and(|v| v.is_finite() && (0.0..=100.0).contains(&v)),
            "missing/invalid official margin"
        );
    }
    for fee in [
        &row.open_fee,
        &row.close_yesterday_fee,
        &row.close_today_fee,
    ]
    .into_iter()
    .chain(row.intraday_open_fee.iter())
    {
        ensure!(
            fee.kind != FeeKind::Unknown && fee.value.is_some_and(|v| v.is_finite() && v >= 0.0),
            "missing/invalid official fee"
        );
    }
    Ok(())
}

fn equal_fee(left: &FeeSpec, right: &FeeSpec) -> bool {
    let zero = |v: &FeeSpec| v.value == Some(0.0) && v.kind != FeeKind::Unknown;
    (zero(left) && zero(right)) || (left.kind == right.kind && left.value == right.value)
}

fn equal_state(current: &ContractFee, row: &AllowedRow) -> bool {
    equal_fee(&current.open_fee, &row.open_fee)
        && equal_fee(&current.close_yesterday_fee, &row.close_yesterday_fee)
        && equal_fee(&current.close_today_fee, &row.close_today_fee)
        && match (&current.intraday_open_fee, &row.intraday_open_fee) {
            (None, None) => true,
            (Some(a), Some(b)) => equal_fee(a, b),
            _ => false,
        }
        && current.buy_margin_rate == row.buy_margin_rate
        && current.sell_margin_rate == row.sell_margin_rate
        && current.trading_status == row.trading_status
        && current.is_main_contract == row.is_main_contract
}

#[allow(clippy::too_many_lines)]
fn apply_snapshot(archive: &mut FeeArchiveV3, snapshot: &Snapshot, previous: Date) -> Result<bool> {
    let rows: BTreeMap<_, _> = snapshot
        .rows
        .iter()
        .map(|r| (r.row.symbol.as_str(), r))
        .collect();
    ensure!(
        !rows.is_empty() && rows.len() == snapshot.rows.len(),
        "empty/duplicate parameter rows"
    );
    for contract in &archive.contracts {
        if contract
            .symbol
            .starts_with(&format!("{}.", snapshot.exchange))
            && contract.listing_date.as_ref().is_some_and(|v| {
                day(v).is_ok_and(|d| d <= day(&snapshot.trade_date).expect("validated day"))
            })
            && contract.expiry_date.as_ref().is_some_and(|v| {
                day(v).is_ok_and(|d| d >= day(&snapshot.trade_date).expect("validated day"))
            })
        {
            ensure!(
                rows.contains_key(contract.symbol.as_str()),
                "missing active contract: {} {}",
                contract.symbol,
                snapshot.trade_date
            );
        }
    }
    let mut changed = false;
    for parameter in rows.values() {
        let row = &parameter.row;
        validate(row, &snapshot.exchange, &snapshot.trade_date)?;
        let listing = Some(
            day(row.listing_date.as_deref().context("missing listing")?)?
                .format(&format_description::parse("[year][month][day]")?)?,
        );
        let expiry = Some(
            day(row.expiry_date.as_deref().context("missing expiry")?)?
                .format(&format_description::parse("[year][month][day]")?)?,
        );
        let index = if let Some(index) = archive
            .contracts
            .iter()
            .position(|v| v.symbol == row.symbol)
        {
            index
        } else {
            let id = archive
                .contracts
                .iter()
                .map(|v| v.id)
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .context("contract id overflow")?;
            archive.contracts.push(Contract {
                id,
                symbol: row.symbol.clone(),
                listing_date: listing.clone(),
                expiry_date: expiry.clone(),
                lot_size: row.lot_size,
                tick_size: row.tick_size,
                active: true,
            });
            changed = true;
            archive.contracts.len() - 1
        };
        let contract = &mut archive.contracts[index];
        let old_expiry = contract.expiry_date.clone();
        changed |= contract.listing_date != listing || contract.expiry_date != expiry;
        contract.listing_date = listing;
        contract.expiry_date = expiry;
        let id = contract.id;
        if old_expiry != contract.expiry_date
            && let Some(old_expiry) = old_expiry
        {
            let old_cap = after_expiry(&old_expiry)?;
            let new_cap = after_expiry(contract.expiry_date.as_deref().context("missing expiry")?)?;
            for spec in archive
                .contract_spec_versions
                .iter_mut()
                .filter(|v| v.contract_id == id)
            {
                if spec.valid_to.as_deref() == Some(&old_cap) {
                    spec.valid_to = Some(new_cap.clone());
                }
            }
            for fee in archive
                .fee_versions
                .iter_mut()
                .filter(|v| v.contract_id == id)
            {
                if fee.valid_to.as_deref() == Some(&old_cap) {
                    fee.valid_to = Some(new_cap.clone());
                }
            }
        }
        let spec_index = archive
            .contract_spec_versions
            .iter()
            .rposition(|v| v.contract_id == id);
        let spec_changed = spec_index.is_none_or(|i| {
            let v = &archive.contract_spec_versions[i];
            v.lot_size != row.lot_size || v.tick_size != row.tick_size
        });
        if spec_changed {
            let boundary = parameter
                .spec_effective_at
                .clone()
                .unwrap_or_else(|| format!("{}T00:00:00+08:00", snapshot.trade_date));
            let boundary_time = OffsetDateTime::parse(&boundary, &Rfc3339)?;
            ensure!(
                timestamp_day(&boundary)? <= day(&snapshot.trade_date)?,
                "specification starts after observation"
            );
            let cap = if let Some(index) = spec_index {
                let effective_day = parameter
                    .spec_trade_date
                    .as_deref()
                    .map(day)
                    .transpose()?
                    .unwrap_or(timestamp_day(&boundary)?);
                ensure!(
                    effective_day > previous && effective_day <= day(&snapshot.trade_date)?,
                    "cannot rewrite historical specification"
                );
                let prior = &mut archive.contract_spec_versions[index];
                let cap =
                    transition_cap(&prior.valid_from, prior.valid_to.as_deref(), boundary_time)?;
                prior.valid_to = Some(boundary.clone());
                cap
            } else {
                Some(after_expiry(
                    row.expiry_date.as_deref().context("missing expiry")?,
                )?)
            };
            archive.contract_spec_versions.push(ContractSpecVersion {
                contract_id: id,
                lot_size: row.lot_size,
                tick_size: row.tick_size,
                valid_from: boundary,
                valid_to: cap,
            });
            changed = true;
        }
        contract.lot_size = row.lot_size;
        contract.tick_size = row.tick_size;
        let latest = archive
            .fee_versions
            .iter()
            .rposition(|v| v.contract_id == id);
        if latest.is_none_or(|i| !equal_state(&archive.fee_versions[i], row)) {
            let boundary = format!("{}T00:00:00+08:00", snapshot.trade_date);
            let cap = if let Some(index) = latest {
                let prior = &mut archive.fee_versions[index];
                let cap = transition_cap(
                    &prior.valid_from,
                    prior.valid_to.as_deref(),
                    OffsetDateTime::parse(&boundary, &Rfc3339)?,
                )?;
                prior.valid_to = Some(boundary.clone());
                cap
            } else {
                None
            };
            archive.fee_versions.push(ContractFee {
                contract_id: id,
                rule_hash: row_rule_hash(row),
                buy_margin_rate: row.buy_margin_rate,
                sell_margin_rate: row.sell_margin_rate,
                open_fee: row.open_fee.clone(),
                close_yesterday_fee: row.close_yesterday_fee.clone(),
                close_today_fee: row.close_today_fee.clone(),
                intraday_open_fee: row.intraday_open_fee.clone(),
                trading_status: row.trading_status.clone(),
                is_main_contract: row.is_main_contract,
                source_updated_at: None,
                valid_from: boundary,
                valid_to: cap,
            });
            changed = true;
        }
        if day(row.expiry_date.as_deref().context("missing expiry")?)? == day(&snapshot.trade_date)?
        {
            let latest = archive
                .fee_versions
                .iter_mut()
                .rev()
                .find(|v| v.contract_id == id)
                .context("missing current fee")?;
            if latest.valid_to.is_none() {
                latest.valid_to = Some(after_expiry(
                    row.expiry_date.as_deref().context("missing expiry")?,
                )?);
                changed = true;
            }
        }
    }
    Ok(changed)
}

fn after_expiry(value: &str) -> Result<String> {
    Ok(format!(
        "{}T00:00:00+08:00",
        day(value)?
            .next_day()
            .context("expiry overflow")?
            .format(&format_description::parse("[year]-[month]-[day]")?)?
    ))
}

fn transition_cap(
    from: &str,
    end: Option<&str>,
    boundary: OffsetDateTime,
) -> Result<Option<String>> {
    ensure!(
        OffsetDateTime::parse(from, &Rfc3339)? < boundary,
        "invalid version transition"
    );
    if let Some(end) = end {
        ensure!(
            OffsetDateTime::parse(end, &Rfc3339)? > boundary,
            "transition after known interval end"
        );
    }
    Ok(end.map(str::to_owned))
}
