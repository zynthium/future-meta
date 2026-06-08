use future_meta::archive::{decode_archive_bytes, encode_archive_bytes, sha256_hex};
use future_meta::error::FutureMetaError;
use future_meta::model::{
    Contract, ContractFee, FeeArchiveV1, FeeKind, FeeSpec, SCHEMA_VERSION, TradingStatus,
};
use future_meta::query::FutureMeta;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

fn sample_archive() -> FeeArchiveV1 {
    FeeArchiveV1 {
        schema_version: SCHEMA_VERSION,
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

    assert_eq!(decoded, archive);
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
fn queries_underlying_and_main_continuous() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let fees = meta
        .underlying_fees_asof("SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();

    assert_eq!(fees.len(), 1);

    let main = meta
        .main_contract_fee_asof("KQ.m@SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap();
    assert_eq!(main.rule_hash, "abc");
}

#[test]
fn rejects_index_for_main_contract_query() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let err = meta
        .main_contract_fee_asof("KQ.i@SHFE.cu", "2026-06-04T12:00:00+08:00")
        .unwrap_err();

    assert!(matches!(
        err,
        FutureMetaError::UnsupportedSymbolKind(symbol) if symbol == "KQ.i@SHFE.cu"
    ));
}

#[test]
fn rejects_queries_before_history_start() {
    let meta = FutureMeta::from_archive(sample_archive()).unwrap();
    let err = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T11:59:59+08:00")
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
    archive.fee_versions[0].valid_to = Some("2026-06-04T13:00:00+08:00".to_owned());
    next_fee.rule_hash = "def".to_owned();
    next_fee.valid_from = "2026-06-04T13:00:00+08:00".to_owned();
    next_fee.valid_to = None;
    archive.fee_versions.push(next_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let before_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T12:59:59+08:00")
        .unwrap();
    let at_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T13:00:00+08:00")
        .unwrap();

    assert_eq!(before_boundary.rule_hash, "abc");
    assert_eq!(at_boundary.rule_hash, "def");
}

#[test]
fn treats_valid_to_as_exclusive_with_equivalent_utc_timestamp() {
    let mut archive = sample_archive();
    let mut next_fee = archive.fee_versions[0].clone();
    archive.fee_versions[0].valid_to = Some("2026-06-04T13:00:00+08:00".to_owned());
    next_fee.rule_hash = "def".to_owned();
    next_fee.valid_from = "2026-06-04T13:00:00+08:00".to_owned();
    next_fee.valid_to = None;
    archive.fee_versions.push(next_fee);

    let meta = FutureMeta::from_archive(archive).unwrap();
    let at_boundary = meta
        .contract_fee_asof("SHFE.cu2607", "2026-06-04T05:00:00Z")
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
