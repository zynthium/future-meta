use future_meta::archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex};
use future_meta::model::{
    Contract, ContractFee, ContractSpecVersion, FeeArchiveV3, FeeKind, FeeSpec, SCHEMA_VERSION,
    TradingStatus,
};
use serde_json::{Value, json};
use std::process::Command;

const SYMBOLS: [&str; 6] = [
    "CFFEX.IF2611",
    "CZCE.SR701",
    "DCE.m2701",
    "GFEX.si2701",
    "INE.sc2701",
    "SHFE.cu2701",
];

fn baseline() -> FeeArchiveV3 {
    let fee = FeeSpec {
        kind: FeeKind::CnyPerLot,
        value: Some(1.0),
        raw_text: None,
    };
    FeeArchiveV3 {
        schema_version: SCHEMA_VERSION,
        generated_at: "2026-10-30T11:00:00Z".into(),
        history_start: "2026-10-29T00:00:00+08:00".into(),
        history_end: "2026-10-29T00:00:00+08:00".into(),
        contracts: SYMBOLS
            .iter()
            .enumerate()
            .map(|(id, symbol)| Contract {
                id: u32::try_from(id + 1).unwrap(),
                symbol: (*symbol).into(),
                listing_date: Some("20260101".into()),
                expiry_date: Some("20261231".into()),
                lot_size: 10.0,
                tick_size: 1.0,
                active: true,
            })
            .collect(),
        contract_spec_versions: (1..=6)
            .map(|id| ContractSpecVersion {
                contract_id: id,
                lot_size: 10.0,
                tick_size: 1.0,
                valid_from: "2026-01-01T00:00:00+08:00".into(),
                valid_to: None,
            })
            .collect(),
        fee_versions: (1..=6)
            .map(|id| ContractFee {
                contract_id: id,
                rule_hash: "approved-old-hash".into(),
                buy_margin_rate: Some(10.0),
                sell_margin_rate: Some(10.0),
                open_fee: fee.clone(),
                close_yesterday_fee: fee.clone(),
                close_today_fee: fee.clone(),
                intraday_open_fee: None,
                trading_status: TradingStatus::Unknown,
                is_main_contract: false,
                source_updated_at: None,
                valid_from: "2026-10-29T00:00:00+08:00".into(),
                valid_to: None,
            })
            .collect(),
    }
}

fn batch(bytes: &[u8], days: &[&str]) -> Value {
    json!({"baseline_sha256": sha256_hex(bytes), "previous_through": "2026-10-29", "trading_days": days,
        "generated_at": "2026-11-02T11:00:00Z", "snapshots": days.iter().flat_map(|day| SYMBOLS.iter().map(move |symbol| {
            json!({"exchange": symbol.split('.').next().unwrap(), "trade_date": day, "rows": [{
                "symbol": symbol, "listing_date": "2026-01-01", "expiry_date": "2026-12-31",
                "lot_size": 10.0, "tick_size": 1.0, "buy_margin_rate": 10.0, "sell_margin_rate": 10.0,
                "open_fee": {"kind":"CnyPerLot","value":1.0,"raw_text":"1.00"},
                "close_yesterday_fee": {"kind":"CnyPerLot","value":1.0,"raw_text":null},
                "close_today_fee": {"kind":"CnyPerLot","value":1.0,"raw_text":null},
                "intraday_open_fee": null, "trading_status":"Unknown", "is_main_contract":false,
                "source_updated_at":"2026-10-30T18:00:00+08:00", "spec_effective_at": null
            }]})
        })).collect::<Vec<_>>()})
}

fn run(input: &Value, bytes: &[u8], success: bool) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let archive = dir.path().join("old.fmeta.zst");
    let batch = dir.path().join("batch.json");
    let out = dir.path().join("out");
    std::fs::write(&archive, bytes).unwrap();
    std::fs::write(&batch, serde_json::to_vec(input).unwrap()).unwrap();
    let result = Command::new(env!("CARGO_BIN_EXE_future-meta-daemon"))
        .args(["update-archive", "--archive"])
        .arg(archive)
        .arg("--input")
        .arg(batch)
        .arg("--out")
        .arg(&out)
        .output()
        .unwrap();
    assert_eq!(
        result.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    if !success {
        assert!(
            !out.exists(),
            "failed batches must not publish partial output"
        );
    }
    (dir, out)
}

#[test]
fn unchanged_preserves_exact_archive_bytes_despite_observation_and_text_changes() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let (_dir, out) = run(&batch(&bytes, &["2026-10-30"]), &bytes, true);
    assert_eq!(std::fs::read(out.join("latest.fmeta.zst")).unwrap(), bytes);
    let report: Value =
        serde_json::from_slice(&std::fs::read(out.join("update-report.json")).unwrap()).unwrap();
    assert_eq!(report["changed"], false);
    assert_eq!(report["checked_through"], "2026-10-30");
}

#[test]
fn missed_days_preserve_change_then_reversion() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-10-30", "2026-11-02"]);
    input["snapshots"][0]["rows"][0]["open_fee"]["value"] = json!(2.0);
    let (_dir, out) = run(&input, &bytes, true);
    let archive =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    let versions: Vec<_> = archive
        .fee_versions
        .iter()
        .filter(|v| v.contract_id == 1)
        .collect();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[1].open_fee.value, Some(2.0));
    assert_eq!(versions[2].open_fee.value, Some(1.0));
    assert_eq!(
        versions[0].valid_to.as_deref(),
        Some("2026-10-30T00:00:00+08:00")
    );
    assert_eq!(
        versions[1].valid_to.as_deref(),
        Some("2026-11-02T00:00:00+08:00")
    );
}

#[test]
fn incomplete_exchange_or_missing_active_contract_never_publishes() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-10-30"]);
    input["snapshots"].as_array_mut().unwrap().pop();
    run(&input, &bytes, false);
    let mut input = batch(&bytes, &["2026-10-30"]);
    input["snapshots"][0]["rows"] = json!([]);
    run(&input, &bytes, false);
}

#[test]
fn invalid_parameters_and_mismatched_baseline_fail_closed() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-10-30"]);
    input["snapshots"][0]["rows"][0]["buy_margin_rate"] = Value::Null;
    run(&input, &bytes, false);
    input = batch(&bytes, &["2026-10-30"]);
    input["baseline_sha256"] = json!("0".repeat(64));
    run(&input, &bytes, false);
}

#[test]
fn specification_and_margin_changes_are_versions_without_renumbering_contracts() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-10-30"]);
    let row = &mut input["snapshots"][0]["rows"][0];
    row["lot_size"] = json!(20.0);
    row["spec_effective_at"] = json!("2026-10-30T00:00:00+08:00");
    row["buy_margin_rate"] = json!(12.0);
    let (_dir, out) = run(&input, &bytes, true);
    let archive =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    assert_eq!(archive.contracts[0].id, 1);
    assert_eq!(archive.contracts[0].lot_size, 20.0);
    assert_eq!(
        archive
            .contract_spec_versions
            .iter()
            .filter(|v| v.contract_id == 1)
            .count(),
        2
    );
    assert_eq!(
        archive
            .fee_versions
            .iter()
            .filter(|v| v.contract_id == 1)
            .count(),
        2
    );
}

#[test]
fn finite_lifecycle_caps_survive_fee_and_specification_transitions() {
    let mut archive = baseline();
    archive.contract_spec_versions[0].valid_to = Some("2027-01-01T00:00:00+08:00".into());
    archive.fee_versions[0].valid_to = Some("2027-01-01T00:00:00+08:00".into());
    let bytes = encode_archive_bytes(&archive).unwrap();
    let mut input = batch(&bytes, &["2026-10-30"]);
    let row = &mut input["snapshots"][0]["rows"][0];
    row["tick_size"] = json!(0.5);
    row["spec_effective_at"] = json!("2026-10-30T00:00:00+08:00");
    row["open_fee"]["value"] = json!(2.0);
    let (_dir, out) = run(&input, &bytes, true);
    let updated =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    let spec = updated
        .contract_spec_versions
        .iter()
        .rev()
        .find(|v| v.contract_id == 1)
        .unwrap();
    let fee = updated
        .fee_versions
        .iter()
        .rev()
        .find(|v| v.contract_id == 1)
        .unwrap();
    assert_eq!(spec.valid_to.as_deref(), Some("2027-01-01T00:00:00+08:00"));
    assert_eq!(fee.valid_to.as_deref(), Some("2027-01-01T00:00:00+08:00"));
}

#[test]
fn official_expiry_correction_moves_only_lifecycle_derived_caps() {
    let mut archive = baseline();
    archive.contracts[0].expiry_date = Some("20261230".into());
    archive.contract_spec_versions[0].valid_to = Some("2026-12-31T00:00:00+08:00".into());
    archive.fee_versions[0].valid_to = Some("2026-12-31T00:00:00+08:00".into());
    let bytes = encode_archive_bytes(&archive).unwrap();
    let input = batch(&bytes, &["2026-10-30"]);
    let (_dir, out) = run(&input, &bytes, true);
    let updated =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    assert_eq!(
        updated.contract_spec_versions[0].valid_to.as_deref(),
        Some("2027-01-01T00:00:00+08:00")
    );
    assert_eq!(
        updated.fee_versions[0].valid_to.as_deref(),
        Some("2027-01-01T00:00:00+08:00")
    );
    let meta = future_meta::FutureMeta::from_archive(updated).unwrap();
    assert!(
        meta.contract_spec_on(
            "CFFEX.IF2611",
            time::Date::from_calendar_date(2026, time::Month::December, 31).unwrap()
        )
        .is_ok()
    );
}

#[test]
fn new_contract_specification_is_unavailable_after_its_confirmed_expiry() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-10-30"]);
    let mut added = input["snapshots"][2]["rows"][0].clone();
    added["symbol"] = json!("DCE.m2711");
    input["snapshots"][2]["rows"]
        .as_array_mut()
        .unwrap()
        .push(added);
    let (_dir, out) = run(&input, &bytes, true);
    let updated =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    assert_eq!(
        updated
            .contracts
            .iter()
            .find(|c| c.symbol == "DCE.m2711")
            .unwrap()
            .id,
        7
    );
    let meta = future_meta::FutureMeta::from_archive(updated).unwrap();
    assert!(
        meta.contract_spec_on(
            "DCE.m2711",
            time::Date::from_calendar_date(2027, time::Month::January, 1).unwrap()
        )
        .is_err()
    );
}

#[test]
fn explicit_next_trading_day_allows_a_verified_night_specification_boundary() {
    let bytes = encode_archive_bytes(&baseline()).unwrap();
    let mut input = batch(&bytes, &["2026-11-03"]);
    input["previous_through"] = json!("2026-11-02");
    let row = &mut input["snapshots"][5]["rows"][0];
    row["lot_size"] = json!(20.0);
    row["spec_effective_at"] = json!("2026-11-02T21:00:00+08:00");
    row["spec_trade_date"] = json!("2026-11-03");
    let (_dir, out) = run(&input, &bytes, true);
    let updated =
        decode_archive_bytes(&std::fs::read(out.join("latest.fmeta.zst")).unwrap()).unwrap();
    let spec = updated
        .contract_spec_versions
        .iter()
        .rev()
        .find(|v| v.contract_id == 6)
        .unwrap();
    assert_eq!(spec.valid_from, "2026-11-02T21:00:00+08:00");
}
