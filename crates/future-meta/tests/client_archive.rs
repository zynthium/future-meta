use future_meta::archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex};
use future_meta::error::FutureMetaError;
use future_meta::model::{
    Contract, ContractFee, ContractSpecVersion, FeeArchiveV1, FeeArchiveV2, FeeKind, FeeSpec,
    LEGACY_SCHEMA_VERSION, SCHEMA_VERSION, TradingStatus,
};
use future_meta::query::FutureMeta;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

fn assert_amount(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-12,
        "expected {expected}, got {actual}"
    );
}

fn sample_archive() -> FeeArchiveV1 {
    FeeArchiveV1 {
        schema_version: LEGACY_SCHEMA_VERSION,
        generated_at: "2026-06-04T12:00:00+08:00".to_owned(),
        history_start: "2026-06-04T12:00:00+08:00".to_owned(),
        history_end: "2026-06-04T12:00:00+08:00".to_owned(),
        contracts: vec![Contract {
            id: 1,
            symbol: "SHFE.cu2607".to_owned(),
            listing_date: Some("20250716".to_owned()),
            expiry_date: Some("20260715".to_owned()),
            lot_size: 5.0,
            tick_size: 10.0,
            active: true,
        }],
        fee_versions: vec![ContractFee {
            contract_id: 1,
            rule_hash: "abc".to_owned(),
            buy_margin_rate: Some(12.0),
            sell_margin_rate: Some(12.0),
            open_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            close_yesterday_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            close_today_fee: FeeSpec {
                kind: FeeKind::CnyPerLot,
                value: Some(0.1),
                raw_text: Some("0.1元".to_owned()),
            },
            trading_status: TradingStatus::Trading,
            is_main_contract: true,
            source_updated_at: Some("2026-03-27 22:56:54".to_owned()),
            valid_from: "2026-06-04T12:00:00+08:00".to_owned(),
            valid_to: None,
        }],
    }
}

#[test]
fn archive_roundtrips_through_zstd_bincode() {
    let archive = sample_archive();

    let bytes = encode_archive_bytes(&archive).unwrap();
    assert!(bytes.len() > 8);
    let decoded = decode_archive_bytes(&bytes).unwrap();

    assert_eq!(decoded.contracts, archive.contracts);
    assert_eq!(decoded.fee_versions, archive.fee_versions);
    assert_eq!(decoded.contract_spec_versions.len(), 1);
    assert_amount(decoded.contract_spec_versions[0].tick_size, 10.0);
}

#[test]
fn version_two_archive_roundtrips_with_spec_history() {
    let archive = FeeArchiveV2::from(sample_archive());

    let bytes = encode_archive_bytes(&archive).unwrap();
    let decoded = decode_archive_bytes(&bytes).unwrap();

    assert_eq!(decoded, archive);
    assert_eq!(decoded.schema_version, SCHEMA_VERSION);
}

#[cfg(feature = "download")]
#[tokio::test]
async fn load_file_decodes_archive() {
    let bytes = encode_archive_bytes(&sample_archive()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("latest.fmeta.zst");
    tokio::fs::write(&path, bytes).await.unwrap();

    let meta = FutureMeta::load_file(&path).await.unwrap();

    assert_eq!(meta.contracts().len(), 1);
}

#[test]
fn sha256_is_stable_lowercase_hex() {
    assert_eq!(
        sha256_hex(b"future-meta"),
        "4bf01f255e72f4a58d156f5064bc17eb6bcf78ce5da2215bfd4610ee93d87bec"
    );
}

#[test]
fn decode_rejects_unsupported_schema_version() {
    let archive = FeeArchiveV1 {
        schema_version: SCHEMA_VERSION + 1,
        ..sample_archive()
    };
    let bytes = encode_archive_bytes(&archive).unwrap();

    let err = decode_archive_bytes(&bytes).unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::UnsupportedSchemaVersion {
            found,
            supported
        } if found == SCHEMA_VERSION + 1 && supported == SCHEMA_VERSION
    ));
}

#[test]
fn decode_rejects_trailing_bincode_bytes() {
    let archive = sample_archive();
    let mut encoded = bincode::serde::encode_to_vec(&archive, bincode::config::standard()).unwrap();
    encoded.extend_from_slice(b"trailing");
    let bytes = zstd::stream::encode_all(encoded.as_slice(), 19).unwrap();

    let err = decode_archive_bytes(&bytes).unwrap_err();

    assert!(matches!(err, FutureMetaError::CorruptArchive(_)));
}

#[test]
fn queries_contract_fee_asof() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fee = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T12:00:00+08:00")
        .unwrap();

    assert!(fee.is_main_contract);
    assert_eq!(fee.rule_hash, "abc");
}

#[test]
fn queries_contract_specification_asof_across_tick_change() {
    let legacy = sample_archive();
    let mut archive = FeeArchiveV2::from(legacy);
    archive.history_start = "2026-01-01T00:00:00+08:00".to_owned();
    archive.contract_spec_versions = vec![
        ContractSpecVersion {
            contract_id: 1,
            lot_size: 5.0,
            tick_size: 10.0,
            valid_from: "2026-01-01T00:00:00+08:00".to_owned(),
            valid_to: Some("2026-04-10T00:00:00+08:00".to_owned()),
        },
        ContractSpecVersion {
            contract_id: 1,
            lot_size: 5.0,
            tick_size: 5.0,
            valid_from: "2026-04-10T00:00:00+08:00".to_owned(),
            valid_to: None,
        },
    ];
    let meta = FutureMeta::from_archive(archive).unwrap();

    let before = meta
        .contract_spec_asof("SHFE.cu2607", "2026-04-09T23:59:59+08:00")
        .unwrap();
    let after = meta
        .contract_spec_asof("SHFE.cu2607", "2026-04-10T00:00:00+08:00")
        .unwrap();

    assert_amount(before.tick_size, 10.0);
    assert_amount(before.tick_value(), 50.0);
    assert_amount(after.tick_size, 5.0);
    assert_amount(after.tick_value(), 25.0);
}

#[test]
fn queries_contract_fee_asof_with_equivalent_utc_timestamp() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fee = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T04:00:00Z")
        .unwrap();

    assert_eq!(fee.rule_hash, "abc");
}

#[test]
fn queries_contract_fee_with_preparsed_timestamp() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let at = OffsetDateTime::parse("2026-06-04T04:00:00Z", &Rfc3339).unwrap();

    let fee = meta.contract_fee_at("SHFE.cu2607", at).unwrap();

    assert_eq!(fee.rule_hash, "abc");
}

#[test]
fn timestamp_queries_use_exchange_local_date_not_wall_clock_time() {
    let mut archive = sample_archive();
    let mut same_day_fee = archive.fee_versions[0].clone();
    archive.fee_versions[0].valid_to = Some("2026-06-04T13:00:00+08:00".to_owned());
    same_day_fee.rule_hash = "same-day-latest".to_owned();
    same_day_fee.open_fee.value = Some(0.2);
    same_day_fee.valid_from = "2026-06-04T13:00:00+08:00".to_owned();
    same_day_fee.valid_to = None;
    archive.fee_versions.push(same_day_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let morning = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T09:00:00+08:00")
        .unwrap();
    let afternoon = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T14:00:00+08:00")
        .unwrap();

    assert_eq!(morning.rule_hash, "same-day-latest");
    assert_eq!(afternoon.rule_hash, "same-day-latest");
}

#[test]
fn queries_contract_fee_on_trading_date() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();

    let fee = meta.contract_fee_on("SHFE.cu2607", trading_date).unwrap();

    assert_eq!(fee.rule_hash, "abc");
}

#[test]
fn queries_contract_fee_with_resolved_handle() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let at = OffsetDateTime::parse("2026-06-04T04:00:00Z", &Rfc3339).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();

    let fee_at = meta.contract_fee_for_handle_at(handle, at).unwrap();
    let fee_on = meta
        .contract_fee_for_handle_on(handle, trading_date)
        .unwrap();

    assert_eq!(fee_at.rule_hash, "abc");
    assert_eq!(fee_on.rule_hash, "abc");
}

#[test]
fn queries_contract_metadata_and_derives_tick_value() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();

    let by_symbol = meta.contract("SHFE.cu2607").unwrap();
    let by_handle = meta.contract_for_handle(handle).unwrap();
    let day_by_symbol = day.contract("SHFE.cu2607").unwrap();
    let day_by_handle = day.contract_for_handle(handle).unwrap();

    assert_eq!(by_symbol.symbol, "SHFE.cu2607");
    assert_amount(by_symbol.lot_size, 5.0);
    assert_amount(by_symbol.tick_size, 10.0);
    assert_amount(by_symbol.tick_value(), 50.0);
    assert!(std::ptr::eq(by_symbol, by_handle));
    assert!(std::ptr::eq(by_symbol, day_by_symbol));
    assert!(std::ptr::eq(by_symbol, day_by_handle));
}

#[test]
fn rejects_unknown_symbol_and_foreign_handle_for_contract_metadata() {
    let first = FutureMeta::from_archive(sample_archive()).unwrap();
    let second = FutureMeta::from_archive(sample_archive()).unwrap();
    let foreign_handle = first.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = second.for_trading_day(trading_date).unwrap();

    let unknown = second.contract("SHFE.al2607").unwrap_err();
    let foreign = second.contract_for_handle(foreign_handle).unwrap_err();
    let day_foreign = day.contract_for_handle(foreign_handle).unwrap_err();

    assert!(matches!(
        unknown,
        FutureMetaError::UnknownContract(symbol) if symbol == "SHFE.al2607"
    ));
    assert!(matches!(foreign, FutureMetaError::InvalidContractHandle));
    assert!(matches!(
        day_foreign,
        FutureMetaError::InvalidContractHandle
    ));
}

#[test]
fn rejects_contract_handle_from_another_client() {
    let first = FutureMeta::from_archive(sample_archive()).unwrap();
    let second = FutureMeta::from_archive(sample_archive()).unwrap();
    let handle = first.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = second.for_trading_day(trading_date).unwrap();

    let err = second
        .contract_fee_for_handle_on(handle, trading_date)
        .unwrap_err();
    let day_err = day.prepare_fee(handle).unwrap_err();

    assert!(matches!(err, FutureMetaError::InvalidContractHandle));
    assert!(matches!(day_err, FutureMetaError::InvalidContractHandle));
}

#[test]
fn queries_contract_fee_from_trading_day_snapshot() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();
    let handle = day.resolve_contract("SHFE.cu2607").unwrap();

    let fee = day.fee_rule(handle).unwrap();
    let symbol_fee = day.fee_rule_by_symbol("SHFE.cu2607").unwrap();

    assert_eq!(fee.rule_hash, "abc");
    assert_eq!(symbol_fee.rule_hash, "abc");
}

#[test]
fn prepares_daily_fee_for_hot_loop_amounts() {
    let mut archive = sample_archive();
    archive.fee_versions[0].open_fee = FeeSpec {
        kind: FeeKind::CnyPerLot,
        value: Some(2.0),
        raw_text: Some("2元".to_owned()),
    };
    archive.fee_versions[0].close_yesterday_fee = FeeSpec {
        kind: FeeKind::Zero,
        value: Some(0.0),
        raw_text: Some("0".to_owned()),
    };
    archive.fee_versions[0].close_today_fee = FeeSpec {
        kind: FeeKind::TurnoverRatePerTenThousand,
        value: Some(0.5),
        raw_text: Some("0.5/万分之".to_owned()),
    };

    let meta = FutureMeta::from_archive(archive).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();
    let handle = day.resolve_contract("SHFE.cu2607").unwrap();

    let fee = day.prepare_fee(handle).unwrap();

    assert_amount(fee.open_amount(70_000.0, 3.0), 6.0);
    assert_amount(fee.close_yesterday_amount(70_000.0, 3.0), 0.0);
    assert_amount(fee.close_today_amount(70_000.0, 2.0), 35.0);
}

#[test]
fn rejects_unknown_fee_when_preparing_hot_loop_fee() {
    let mut archive = sample_archive();
    archive.fee_versions[0].open_fee = FeeSpec {
        kind: FeeKind::Unknown,
        value: None,
        raw_text: Some("按交易所通知".to_owned()),
    };
    let meta = FutureMeta::from_archive(archive).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();
    let handle = day.resolve_contract("SHFE.cu2607").unwrap();

    let err = day.prepare_fee(handle).unwrap_err();

    assert!(
        matches!(err, FutureMetaError::UnsupportedFeeRule(message) if message.contains("SHFE.cu2607 open_fee"))
    );
}

#[test]
fn trading_day_snapshot_preserves_no_version_errors() {
    let mut archive = sample_archive();
    archive.fee_versions[0].valid_to = Some("2026-06-05T00:00:00+08:00".to_owned());
    let meta = FutureMeta::from_archive(archive).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 5).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();

    let err = day.fee_rule(handle).unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::NoVersionAt(symbol) if symbol == "SHFE.cu2607"
    ));
}

#[test]
fn trading_day_snapshot_preserves_no_version_for_contract_before_first_fee() {
    let mut archive = sample_archive();
    archive.contracts.push(Contract {
        id: 2,
        symbol: "SHFE.cu2608".to_owned(),
        listing_date: Some("20260604".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: true,
    });
    let mut future_fee = archive.fee_versions[0].clone();
    future_fee.contract_id = 2;
    future_fee.rule_hash = "future".to_owned();
    future_fee.valid_from = "2026-06-05T00:00:00+08:00".to_owned();
    archive.fee_versions.push(future_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();
    let handle = day.resolve_contract("SHFE.cu2608").unwrap();

    let err = day.fee_rule(handle).unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::NoVersionAt(symbol) if symbol == "SHFE.cu2608"
    ));
}

#[test]
fn trading_day_snapshot_rejects_dates_before_history() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 3).unwrap();

    let err = meta.for_trading_day(trading_date).unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::NotAvailableBeforeHistoryStart
    ));
}

#[test]
fn queries_concrete_contracts_and_rejects_kq_fee_aliases() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fees = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();

    assert_eq!(fees.len(), 1);
    assert_eq!(fees[0].contract_id, 1);

    for symbol in ["KQ.m@SHFE.cu", "KQ.i@SHFE.cu"] {
        let err = meta
            .contract_fee_asof(symbol, "2026-06-04T12:00:00+08:00")
            .unwrap_err();
        assert!(matches!(
            err,
            FutureMetaError::UnsupportedSymbolKind(rejected) if rejected == symbol
        ));
    }
}

#[test]
fn rejects_kq_aliases_for_contract_metadata_and_specs() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();

    for symbol in ["KQ.m@SHFE.cu", "KQ.i@SHFE.cu"] {
        let contract_err = meta.contract(symbol).unwrap_err();
        assert!(matches!(
            contract_err,
            FutureMetaError::UnsupportedSymbolKind(rejected) if rejected == symbol
        ));

        let spec_err = meta
            .contract_spec_asof(symbol, "2026-06-04T12:00:00+08:00")
            .unwrap_err();
        assert!(matches!(
            spec_err,
            FutureMetaError::UnsupportedSymbolKind(rejected) if rejected == symbol
        ));
    }
}

#[test]
fn rejects_archives_that_persist_kq_aliases_as_contracts() {
    let mut archive = FeeArchiveV2::from(sample_archive());
    archive.contracts[0].symbol = "KQ.m@SHFE.cu".to_owned();

    let err = FutureMeta::from_archive(archive).unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::CorruptArchive(message)
            if message.contains("non-concrete contract symbol KQ.m@SHFE.cu")
    ));
}

#[test]
fn clone_shares_indexed_storage() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let cloned = meta.clone();

    assert!(std::ptr::eq(meta.contracts(), cloned.contracts()));
}

#[test]
fn parsed_time_underlying_queries_return_concrete_contracts() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let at = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();

    let at_fees = meta
        .underlying_fees_at("SHFE.cu", at)
        .unwrap()
        .collect::<Vec<_>>();
    let on_fees = meta
        .underlying_fees_on("SHFE.cu", trading_date)
        .unwrap()
        .collect::<Vec<_>>();

    assert_eq!(at_fees.len(), 1);
    assert_eq!(on_fees.len(), 1);
    assert_eq!(at_fees[0].contract_id, 1);
    assert_eq!(on_fees[0].contract_id, 1);
}

#[test]
fn underlying_query_keeps_concrete_fees_with_unknown_trading_status() {
    let mut archive = FeeArchiveV2::from(sample_archive());
    archive.fee_versions[0].trading_status = TradingStatus::Unknown;
    let meta = FutureMeta::from_archive(archive).unwrap();

    let fees = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();

    assert_eq!(fees.len(), 1);
    assert_eq!(fees[0].contract_id, 1);
}

#[test]
fn rejects_queries_before_history_start() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let err = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-03T23:59:59+08:00")
        .unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::NotAvailableBeforeHistoryStart
    ));
}

#[test]
fn rejects_invalid_query_timestamp() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let err = meta
        .contract_fee_asof("SHFE.cu2607", "20260604")
        .unwrap_err();

    assert!(matches!(err, FutureMetaError::InvalidTimestamp(_)));
}

#[test]
fn rejects_unknown_contract_and_underlying() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();

    let contract_err = meta
        .contract_fee_asof("SHFE.al2607", "2026-06-04T12:00:00+08:00")
        .unwrap_err();
    assert!(matches!(
        contract_err,
        FutureMetaError::UnknownContract(symbol) if symbol == "SHFE.al2607"
    ));

    let handle_err = meta.resolve_contract("SHFE.al2607").unwrap_err();
    assert!(matches!(
        handle_err,
        FutureMetaError::UnknownContract(symbol) if symbol == "SHFE.al2607"
    ));

    let underlying_err = meta
        .underlying_fees_asof("SHFE.al", "2026-06-04T12:00:00+08:00")
        .unwrap_err();
    assert!(matches!(
        underlying_err,
        FutureMetaError::UnknownUnderlyingSymbol(symbol) if symbol == "SHFE.al"
    ));
}

#[test]
fn treats_valid_to_as_exclusive() {
    let mut archive = sample_archive();
    let mut next_fee = archive.fee_versions[0].clone();
    archive.fee_versions[0].valid_to = Some("2026-06-05T00:00:00+08:00".to_owned());
    next_fee.rule_hash = "def".to_owned();
    next_fee.valid_from = "2026-06-05T00:00:00+08:00".to_owned();
    next_fee.valid_to = None;
    archive.fee_versions.push(next_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let before_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T23:59:59+08:00")
        .unwrap();
    let at_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-05T00:00:00+08:00")
        .unwrap();

    assert_eq!(before_boundary.rule_hash, "abc");
    assert_eq!(at_boundary.rule_hash, "def");
}

#[test]
fn prepared_fee_cursors_change_only_at_trading_day_boundary() {
    let mut archive = sample_archive();
    let mut same_day_fee = archive.fee_versions[0].clone();
    archive.fee_versions[0].valid_to = Some("2026-06-04T13:00:00+08:00".to_owned());
    same_day_fee.rule_hash = "same-day-latest".to_owned();
    same_day_fee.open_fee.value = Some(0.2);
    same_day_fee.valid_from = "2026-06-04T13:00:00+08:00".to_owned();
    same_day_fee.valid_to = None;
    archive.fee_versions.push(same_day_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let start = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let later = OffsetDateTime::parse("2026-06-04T14:00:00+08:00", &Rfc3339).unwrap();
    let next_day_start = OffsetDateTime::parse("2026-06-05T00:00:00+08:00", &Rfc3339).unwrap();
    let start_unix_nanos = i64::try_from(start.unix_timestamp_nanos()).unwrap();
    let later_unix_nanos = i64::try_from(later.unix_timestamp_nanos()).unwrap();
    let next_day_start_unix_nanos = i64::try_from(next_day_start.unix_timestamp_nanos()).unwrap();

    let mut cursors = meta
        .prepare_fee_cursors([handle], trading_date, start_unix_nanos)
        .unwrap();

    assert_eq!(cursors.next_change_unix_nanos(), next_day_start_unix_nanos);
    assert_amount(cursors.current(0).unwrap().open_amount(70_000.0, 1.0), 0.2);

    cursors.advance_to(trading_date, later_unix_nanos).unwrap();

    assert_eq!(cursors.next_change_unix_nanos(), next_day_start_unix_nanos);
    assert_amount(cursors.current(0).unwrap().open_amount(70_000.0, 1.0), 0.2);
}

#[test]
fn prepared_fee_cursors_reject_timestamp_before_trading_day_start() {
    let mut archive = sample_archive();
    archive.history_start = "2026-06-03T12:00:00+08:00".to_owned();
    archive.fee_versions[0].valid_from = "2026-06-03T12:00:00+08:00".to_owned();

    let meta = FutureMeta::from_archive(archive).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let start = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let before_day = OffsetDateTime::parse("2026-06-03T23:59:59+08:00", &Rfc3339).unwrap();
    let start_unix_nanos = i64::try_from(start.unix_timestamp_nanos()).unwrap();
    let before_day_unix_nanos = i64::try_from(before_day.unix_timestamp_nanos()).unwrap();

    let mut cursors = meta
        .prepare_fee_cursors([handle], trading_date, start_unix_nanos)
        .unwrap();
    let err = cursors
        .advance_and_get_unix_nanos(0, before_day_unix_nanos)
        .unwrap_err();

    assert!(matches!(err, FutureMetaError::InvalidTimestamp(_)));
}

#[test]
fn prepared_fee_cursors_use_caller_slot_order() {
    let mut archive = sample_archive();
    archive.contracts.push(Contract {
        id: 2,
        symbol: "SHFE.cu2608".to_owned(),
        listing_date: Some("20250716".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: true,
    });
    let mut second_fee = archive.fee_versions[0].clone();
    second_fee.contract_id = 2;
    second_fee.open_fee.value = Some(0.3);
    archive.fee_versions.push(second_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let day = meta.for_trading_day(trading_date).unwrap();
    let cu2608 = day.resolve_contract("SHFE.cu2608").unwrap();
    let cu2607 = day.resolve_contract("SHFE.cu2607").unwrap();
    let start = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let start_unix_nanos = i64::try_from(start.unix_timestamp_nanos()).unwrap();

    let mut cursors = day
        .prepare_fee_cursors([cu2608, cu2607], start_unix_nanos)
        .unwrap();

    assert_amount(
        cursors
            .advance_and_get_unix_nanos(0, start_unix_nanos)
            .unwrap()
            .open_amount(70_000.0, 1.0),
        0.3,
    );
    assert_amount(
        cursors
            .advance_and_get_unix_nanos(1, start_unix_nanos)
            .unwrap()
            .open_amount(70_000.0, 1.0),
        0.1,
    );
}

#[test]
fn prepared_fee_cursors_advance_across_trading_days() {
    let mut archive = sample_archive();
    archive.history_end = "2026-06-05T12:00:00+08:00".to_owned();
    archive.fee_versions[0].valid_to = Some("2026-06-05T00:00:00+08:00".to_owned());
    let mut next_day_fee = archive.fee_versions[0].clone();
    next_day_fee.rule_hash = "next-day".to_owned();
    next_day_fee.open_fee.value = Some(0.2);
    next_day_fee.valid_from = "2026-06-05T00:00:00+08:00".to_owned();
    next_day_fee.valid_to = None;
    archive.fee_versions.push(next_day_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let june_4 = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let june_5 = Date::from_calendar_date(2026, Month::June, 5).unwrap();
    let start = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let next_day_start = OffsetDateTime::parse("2026-06-05T00:00:00+08:00", &Rfc3339).unwrap();
    let start_unix_nanos = i64::try_from(start.unix_timestamp_nanos()).unwrap();
    let next_day_start_unix_nanos = i64::try_from(next_day_start.unix_timestamp_nanos()).unwrap();

    let mut cursors = meta
        .prepare_fee_cursors([handle], june_4, start_unix_nanos)
        .unwrap();

    assert_eq!(cursors.current_trading_date(), june_4);
    assert_eq!(cursors.next_change_unix_nanos(), next_day_start_unix_nanos);
    assert_amount(cursors.current(0).unwrap().open_amount(70_000.0, 1.0), 0.1);

    cursors
        .advance_to(june_5, next_day_start_unix_nanos)
        .unwrap();

    assert_eq!(cursors.current_trading_date(), june_5);
    assert_amount(cursors.current(0).unwrap().open_amount(70_000.0, 1.0), 0.2);
}

#[test]
fn prepared_fee_cursors_reject_backwards_ticks() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let handle = meta.resolve_contract("SHFE.cu2607").unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let start = OffsetDateTime::parse("2026-06-04T12:00:00+08:00", &Rfc3339).unwrap();
    let later = OffsetDateTime::parse("2026-06-04T12:00:01+08:00", &Rfc3339).unwrap();
    let start_unix_nanos = i64::try_from(start.unix_timestamp_nanos()).unwrap();
    let later_unix_nanos = i64::try_from(later.unix_timestamp_nanos()).unwrap();

    let mut cursors = meta
        .prepare_fee_cursors([handle], trading_date, start_unix_nanos)
        .unwrap();

    cursors.advance_to(trading_date, later_unix_nanos).unwrap();
    let err = cursors
        .advance_and_get(trading_date, 0, start_unix_nanos)
        .unwrap_err();

    assert!(matches!(err, FutureMetaError::InvalidTimestamp(_)));
}

#[test]
fn prepared_fee_cursors_validate_empty_table_start_timestamp() {
    let mut archive = sample_archive();
    archive.history_start = "2026-06-03T12:00:00+08:00".to_owned();
    archive.fee_versions[0].valid_from = "2026-06-03T12:00:00+08:00".to_owned();
    let meta = FutureMeta::from_archive(archive).unwrap();
    let trading_date = Date::from_calendar_date(2026, Month::June, 4).unwrap();
    let before_day = OffsetDateTime::parse("2026-06-03T23:59:59+08:00", &Rfc3339).unwrap();
    let before_day_unix_nanos = i64::try_from(before_day.unix_timestamp_nanos()).unwrap();

    let err = meta
        .prepare_fee_cursors([], trading_date, before_day_unix_nanos)
        .unwrap_err();

    assert!(matches!(err, FutureMetaError::InvalidTimestamp(_)));
}

#[test]
fn treats_valid_to_as_exclusive_with_equivalent_utc_timestamp() {
    let mut archive = sample_archive();
    let mut next_fee = archive.fee_versions[0].clone();
    archive.fee_versions[0].valid_to = Some("2026-06-05T00:00:00+08:00".to_owned());
    next_fee.rule_hash = "def".to_owned();
    next_fee.valid_from = "2026-06-05T00:00:00+08:00".to_owned();
    next_fee.valid_to = None;
    archive.fee_versions.push(next_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let at_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T16:00:00Z")
        .unwrap();

    assert_eq!(at_boundary.rule_hash, "def");
}

#[test]
fn underlying_query_filters_status_and_contract_dates() {
    let mut archive = sample_archive();

    archive.contracts.push(Contract {
        id: 2,
        symbol: "SHFE.cu2608".to_owned(),
        listing_date: Some("20260605".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: true,
    });
    archive.contracts.push(Contract {
        id: 3,
        symbol: "SHFE.cu2606".to_owned(),
        listing_date: Some("20250601".to_owned()),
        expiry_date: Some("20260603".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: false,
    });
    archive.contracts.push(Contract {
        id: 4,
        symbol: "SHFE.cu2609".to_owned(),
        listing_date: Some("20250601".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: true,
    });

    let mut not_listed_fee = archive.fee_versions[0].clone();
    not_listed_fee.contract_id = 2;
    not_listed_fee.rule_hash = "not-listed".to_owned();
    let mut expired_fee = archive.fee_versions[0].clone();
    expired_fee.contract_id = 3;
    expired_fee.rule_hash = "expired".to_owned();
    let mut not_trading_fee = archive.fee_versions[0].clone();
    not_trading_fee.contract_id = 4;
    not_trading_fee.rule_hash = "not-trading".to_owned();
    not_trading_fee.trading_status = TradingStatus::NotTrading;

    archive
        .fee_versions
        .extend([not_listed_fee, expired_fee, not_trading_fee]);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let fees = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();

    assert_eq!(fees.len(), 1);
    assert_eq!(fees[0].rule_hash, "abc");
}

#[test]
fn underlying_query_filters_contract_dates_using_exchange_local_date() {
    let mut archive = sample_archive();
    archive.contracts.push(Contract {
        id: 2,
        symbol: "SHFE.cu2608".to_owned(),
        listing_date: Some("20260605".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        lot_size: 5.0,
        tick_size: 10.0,
        active: true,
    });
    let mut listed_next_day_fee = archive.fee_versions[0].clone();
    listed_next_day_fee.contract_id = 2;
    listed_next_day_fee.rule_hash = "listed-next-day".to_owned();
    archive.fee_versions.push(listed_next_day_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let before_exchange_midnight = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T15:59:59Z")
        .unwrap();
    let at_exchange_midnight = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T16:00:00Z")
        .unwrap();

    assert_eq!(before_exchange_midnight.len(), 1);
    assert_eq!(at_exchange_midnight.len(), 2);
}
