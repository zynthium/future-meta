//! Offline acceptance: remove the last interval, replay official observations,
//! and compare the resulting business history to the approved published archive.
use anyhow::{Context, Result, ensure};
use future_meta::archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex};
use serde_json::Value;
use std::path::PathBuf;

#[allow(clippy::too_many_lines)]
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    ensure!(
        args.len() == 4,
        "usage: replay_archive_delta ARCHIVE BATCH_JSON OUT"
    );
    let original = decode_archive_bytes(&std::fs::read(&args[1])?)?;
    let mut batch: Value = serde_json::from_slice(&std::fs::read(&args[2])?)?;
    let before = batch["previous_through"]
        .as_str()
        .context("missing previous checkpoint")?;
    let after = batch["trading_days"]
        .as_array()
        .context("missing trading days")?
        .last()
        .context("no days")?
        .as_str()
        .context("invalid day")?
        .to_owned();
    let boundary = format!(
        "{}T00:00:00+08:00",
        batch["trading_days"][0]
            .as_str()
            .context("missing first day")?
    );
    let mut baseline = original.clone();
    baseline.fee_versions.retain(|v| v.valid_from < boundary);
    let removed: std::collections::HashSet<_> = original
        .fee_versions
        .iter()
        .filter(|v| v.valid_from >= boundary)
        .map(|v| (v.contract_id, v.valid_from.as_str()))
        .collect();
    for fee in &mut baseline.fee_versions {
        if fee
            .valid_to
            .as_ref()
            .is_some_and(|end| removed.contains(&(fee.contract_id, end.as_str())))
        {
            fee.valid_to = None;
        }
    }
    let compact_before = before.replace('-', "");
    for contract in &mut baseline.contracts {
        contract.active = contract
            .listing_date
            .as_ref()
            .is_some_and(|v| v <= &compact_before)
            && contract
                .expiry_date
                .as_ref()
                .is_some_and(|v| v >= &compact_before);
    }
    baseline.history_end = baseline
        .fee_versions
        .iter()
        .map(|v| &v.valid_from)
        .max()
        .context("no history")?
        .clone();
    let bytes = encode_archive_bytes(&baseline)?;
    batch["baseline_sha256"] = Value::String(sha256_hex(&bytes));
    let root = PathBuf::from(&args[3]);
    std::fs::create_dir(&root)?;
    let archive_path = root.join("baseline.fmeta.zst");
    let batch_path = root.join("batch.json");
    std::fs::write(&archive_path, &bytes)?;
    std::fs::write(&batch_path, serde_json::to_vec(&batch)?)?;
    let report = future_meta_daemon::archive_update::update(
        &archive_path,
        &batch_path,
        &root.join("result"),
    )?;
    let updated = decode_archive_bytes(&std::fs::read(root.join("result/latest.fmeta.zst"))?)?;
    let mut expected_fees = original.fee_versions.clone();
    let mut actual_fees = updated.fee_versions.clone();
    expected_fees.sort_by_key(|v| (v.contract_id, v.valid_from.clone()));
    actual_fees.sort_by_key(|v| (v.contract_id, v.valid_from.clone()));
    ensure!(
        actual_fees == expected_fees,
        "replayed fee history differs from approved archive"
    );
    ensure!(
        updated.contract_spec_versions == original.contract_spec_versions,
        "specification history changed"
    );
    let mut expected_contracts = original.contracts;
    let compact_after = after.replace('-', "");
    for contract in &mut expected_contracts {
        contract.active = contract
            .listing_date
            .as_ref()
            .is_some_and(|v| v <= &compact_after)
            && contract
                .expiry_date
                .as_ref()
                .is_some_and(|v| v >= &compact_after);
    }
    ensure!(
        updated.contracts == expected_contracts,
        "contract metadata changed"
    );
    println!("{}", serde_json::to_string_pretty(&report)?);
    println!("Approved business history matches exactly; active flags use verified trading date.");
    Ok(())
}
