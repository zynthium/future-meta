use future_meta::query::FutureMeta;
use future_meta_daemon::cffex_metadata::{
    CffexMetadataImportOptions, import_contract_metadata as import_cffex_metadata,
};
use future_meta_daemon::contract_base_info::{
    ContractBaseInfoImportOptions, import_contract_base_info,
};
use future_meta_daemon::coverage::{
    CoverageBoundary, CoverageFindingKind, audit_history_coverage, audit_history_coverage_to_path,
};
use future_meta_daemon::czce::{CzceParameterImportOptions, import_daily_parameters};
use future_meta_daemon::db::{
    OfficialEvidenceLevel, OfficialEvidenceReference, OfficialHistoryRow,
    ReviewObservationTimeRepair, apply_official_fee_transition, apply_official_fee_tuple,
    apply_official_listed_contract_fee_tuple, connect, connect_readonly, ensure_schema,
    migrate_known_contract_spec_history, rekey_fee_history_for_review,
    repair_review_observation_times, replace_with_official_parameter_history,
    validate_fee_history_integrity,
};
use future_meta_daemon::dce::{
    DceCalendarImportOptions, DceParameterImportOptions,
    import_daily_settlement_parameters as import_dce_parameters,
    import_trading_calendar_lifecycles as import_dce_calendar,
};
use future_meta_daemon::export::export_archive;
use future_meta_daemon::gfex::{
    GfexCalendarImportOptions, GfexParameterImportOptions, import_daily_settlement_parameters,
    import_trading_calendar_lifecycles,
};
use future_meta_daemon::hash::row_rule_hash;
use future_meta_daemon::ine::{
    IneParameterImportOptions, import_daily_parameters as import_ine_parameters,
};
use future_meta_daemon::model::AllowedRow;
use future_meta_daemon::official::{
    EvidenceKind, OfficialEvidence, OfficialFeeAdjustment, OfficialVerification,
    apply_verified_adjustments, stage_adjustment, stage_adjustment_json, stage_adjustments_json,
};
use future_meta_daemon::official_history::{OfficialHistoryImportOptions, import_adjustments};
use future_meta_daemon::official_metadata::{
    OfficialMetadataImportOptions, import_contract_metadata,
};
use future_meta_daemon::product_spec::{ProductSpecImportOptions, import_product_specs};
use future_meta_daemon::shfe::{
    ShfeParameterImportOptions, import_monthly_parameters as import_shfe_parameters,
};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use time::{Date, Month};

fn czce_parameter_html(base_fee: &str, fee_mode: &str, close_today_fee: &str) -> String {
    format!(
        r"<html><body><p>2020-01-02</p><table>
        <tr><td>合约代码</td><td>交易手续费</td><td>手续费收取方式</td><td>日内平今仓交易手续费</td></tr>
        <tr><td>SR005</td><td>{base_fee}</td><td>{fee_mode}</td><td>{close_today_fee}</td></tr>
        </table></body></html>"
    )
}

#[test]
fn contract_base_info_import_discovers_unseeded_contract_from_official_spec() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    connection
        .execute(
            "insert into contracts(
                 symbol, listing_date, expiry_date, lot_size, tick_size,
                 first_seen_at, last_seen_at, active
             ) values('SHFE.cu2001', null, null, 5.0, 10.0,
                      '2026-08-25T00:00:00Z', '2026-08-25T00:00:00Z', 0)",
            [],
        )
        .unwrap();
    let contract_id: i64 = connection.last_insert_rowid();
    connection
        .execute(
            "insert into contract_spec_versions(
                 contract_id, lot_size, tick_size, valid_from, valid_to,
                 source_kind, source_url, first_seen_at, last_seen_at
             ) values(?1, 5.0, 10.0, '2019-01-01T00:00:00+08:00',
                      null, 'official',
                      'https://www.shfe.com.cn/products/futures/metal/cu_f/',
                      '2026-08-25T00:00:00Z', '2026-08-25T00:00:00Z')",
            [contract_id],
        )
        .unwrap();
    drop(connection);

    let body = br#"{"ContractBaseInfo":[
        {"INSTRUMENTID":"cu2001","OPENDATE":"20190116","EXPIREDATE":"20200115","TRADINGDAY":"20200102"},
        {"INSTRUMENTID":"cu2002","OPENDATE":"20190218","EXPIREDATE":"20200217","TRADINGDAY":"20200102"}
    ]}"#;
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.dat")), body).unwrap();
    let manifest = directory.path().join("contract-base-info.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\treport_date\tcanonical_url\tsha256\trecord_count\nSHFE\t20200102\thttps://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo20200102.dat\t{sha256}\t2\n"
        ),
    )
    .unwrap();

    let result = import_contract_base_info(&ContractBaseInfoImportOptions {
        history_db: db_path.clone(),
        exchange: "SHFE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-25T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.contracts, 2);
    let connection = connect(&db_path).unwrap();
    let discovered: (String, String, f64, f64, i64) = connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size, active
             from contracts where symbol = 'SHFE.cu2002'",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                    record.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        discovered,
        ("20190218".to_owned(), "20200217".to_owned(), 5.0, 10.0, 0)
    );
}

fn write_czce_parameter_fixture(
    directory: &std::path::Path,
    html: &str,
) -> (std::path::PathBuf, String) {
    let sha256 = hex::encode(Sha256::digest(html.as_bytes()));
    std::fs::write(directory.join(format!("{sha256}.htm")), html).unwrap();
    let url =
        "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2020/20200102/FutureDataClearParams.htm";
    let manifest = directory.join("manifest.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\tsha256\turl\tbyte_count\tcontent_type\n20200102\tok\t{sha256}\t{url}\t{}\ttext/html; charset=utf-8\n",
            html.len()
        ),
    )
    .unwrap();
    (manifest, sha256)
}

#[test]
fn czce_daily_parameter_import_records_lower_confidence_official_history() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(8.0),
        raw_text: Some("8元/手".to_owned()),
    };
    baseline.close_yesterday_fee = baseline.open_fee.clone();
    baseline.close_today_fee = baseline.open_fee.clone();
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, sha256) = write_czce_parameter_fixture(directory.path(), &html);
    let result = import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.snapshots, 1);
    assert_eq!(result.contracts, 1);
    assert_eq!(result.versions, 1);
    let conn = connect(&db_path).unwrap();
    let (source, open, close_yesterday, close_today, level, retained_sha): (
        String,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "select v.source_kind, v.open_fee_json, v.close_yesterday_fee_json,
                    v.close_today_fee_json, e.evidence_level, e.body_sha256
             from fee_versions v
             join contracts c on c.id = v.contract_id
             join fee_version_evidence e on e.contract_id = v.contract_id
                  and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'CZCE.SR005' and v.valid_from = '2020-01-02T00:00:00+08:00'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(source, "official");
    assert_eq!(level, "official_parameter");
    assert_eq!(retained_sha, sha256);
    for json in [open, close_yesterday] {
        let fee: future_meta::model::FeeSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(fee.kind, future_meta::model::FeeKind::CnyPerLot);
        assert_eq!(fee.value, Some(3.0));
    }
    let fee: future_meta::model::FeeSpec = serde_json::from_str(&close_today).unwrap();
    assert_eq!(fee.kind, future_meta::model::FeeKind::Zero);
    assert_eq!(fee.value, Some(0.0));
}

#[test]
fn czce_daily_parameter_import_rejects_hash_mismatch_before_writing() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    drop(conn);
    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, sha256) = write_czce_parameter_fixture(directory.path(), &html);
    std::fs::write(directory.path().join(format!("{sha256}.htm")), "changed").unwrap();

    let error = import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap_err();

    assert!(error.to_string().contains("byte count mismatch"));
    let conn = connect(&db_path).unwrap();
    let evidence: i64 = conn
        .query_row("select count(*) from fee_version_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(evidence, 0);
}

#[test]
fn czce_daily_parameter_import_collapses_unchanged_adjacent_snapshots() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let first = czce_parameter_html("3.00", "绝对值", "0.00");
    let second = first.replace("2020-01-02", "2020-01-03");
    let first_hash = hex::encode(Sha256::digest(first.as_bytes()));
    let second_hash = hex::encode(Sha256::digest(second.as_bytes()));
    std::fs::write(directory.path().join(format!("{first_hash}.htm")), &first).unwrap();
    std::fs::write(directory.path().join(format!("{second_hash}.htm")), &second).unwrap();
    let first_url =
        "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2020/20200102/FutureDataClearParams.htm";
    let second_url =
        "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2020/20200103/FutureDataClearParams.htm";
    let manifest = directory.path().join("manifest.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\tsha256\turl\tbyte_count\tcontent_type\n20200102\tok\t{first_hash}\t{first_url}\t{}\ttext/html\n20200103\tok\t{second_hash}\t{second_url}\t{}\ttext/html\n",
            first.len(), second.len()
        ),
    )
    .unwrap();

    let result = import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path,
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.snapshots, 2);
    assert_eq!(result.versions, 1);
}

#[test]
fn czce_parameter_import_rejects_conflict_with_paired_official_version() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
    let paired_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(9.0),
        raw_text: Some("9元/手".to_owned()),
    };
    apply_official_fee_tuple(
        &mut conn,
        "CZCE.SR005",
        "2020-01-02T00:00:00+08:00",
        &[paired_fee.clone(), paired_fee.clone(), paired_fee],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();
    let (contract_id, rule_hash): (i64, String) = conn
        .query_row(
            "select contract_id, rule_hash from fee_versions
             where valid_from = '2020-01-02T00:00:00+08:00'",
            [],
            |record| Ok((record.get(0)?, record.get(1)?)),
        )
        .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(?1, '2020-01-02T00:00:00+08:00', ?2, 'paired_official',
                  'https://www.czce.com.cn/notice', ?3, '2026-08-24T00:00:00Z')",
        rusqlite::params![contract_id, rule_hash, "a".repeat(64)],
    )
    .unwrap();
    drop(conn);
    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, _) = write_czce_parameter_fixture(directory.path(), &html);

    let error = import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap_err();

    assert!(
        error
            .to_string()
            .contains("paired official fee tuple conflicts")
    );
    let conn = connect(&db_path).unwrap();
    let fee_json: String = conn
        .query_row(
            "select v.open_fee_json from fee_versions v join contracts c on c.id = v.contract_id
             where c.symbol = 'CZCE.SR005' and v.valid_from = '2020-01-02T00:00:00+08:00'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let fee: future_meta::model::FeeSpec = serde_json::from_str(&fee_json).unwrap();
    assert_eq!(fee.value, Some(9.0));
}

#[test]
fn parameter_import_preserves_later_paired_official_evidence_for_same_rule() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline.clone()], "2026-08-24T00:00:00Z").unwrap();

    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(3.0),
        raw_text: Some("3元/手".to_owned()),
    };
    apply_official_fee_tuple(
        &mut conn,
        "CZCE.SR005",
        "2020-01-03T00:00:00+08:00",
        &[fee.clone(), fee.clone(), fee.clone()],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();
    let (contract_id, paired_rule_hash): (i64, String) = conn
        .query_row(
            "select contract_id, rule_hash from fee_versions
             where valid_from = '2020-01-03T00:00:00+08:00'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(?1, '2020-01-03T00:00:00+08:00', ?2, 'paired_official',
                  'https://www.czce.com.cn/notice', ?3, '2026-08-24T00:00:00Z')",
        rusqlite::params![contract_id, paired_rule_hash, "a".repeat(64)],
    )
    .unwrap();

    let mut parameter_row = baseline;
    parameter_row.open_fee = fee.clone();
    parameter_row.close_yesterday_fee = fee.clone();
    parameter_row.close_today_fee = fee;
    parameter_row.source_updated_at = Some("2020-01-02 00:00:00".to_owned());
    replace_with_official_parameter_history(
        &mut conn,
        &[OfficialHistoryRow {
            row: parameter_row,
            coverage_end_exclusive: "2020-01-03T00:00:00+08:00".to_owned(),
            evidence_level: OfficialEvidenceLevel::OfficialParameter,
            evidence: vec![OfficialEvidenceReference {
                canonical_url: "https://www.czce.com.cn/parameters".to_owned(),
                body_sha256: "b".repeat(64),
            }],
        }],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();

    let retained: i64 = conn
        .query_row(
            "select count(*) from fee_version_evidence evidence
             join fee_versions version
               on version.contract_id = evidence.contract_id
              and version.valid_from = evidence.valid_from
              and version.rule_hash = evidence.rule_hash",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let evidence: i64 = conn
        .query_row("select count(*) from fee_version_evidence", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(evidence, 2);
    assert_eq!(retained, evidence);
}

#[test]
fn official_rebuild_restores_orphaned_evidence_key_without_deleting_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "GFEX.lc2708".to_owned();
    baseline.listing_date = Some("20260701".to_owned());
    baseline.expiry_date = Some("20270801".to_owned());
    baseline.source_updated_at = Some("2026-07-01 00:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline.clone()], "2026-08-24T00:00:00Z").unwrap();

    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(1.6),
        raw_text: Some("成交金额的万分之一点六".to_owned()),
    };
    let mut official_row = baseline;
    official_row.trading_status = future_meta::model::TradingStatus::Unknown;
    official_row.buy_margin_rate = None;
    official_row.sell_margin_rate = None;
    official_row.is_main_contract = false;
    official_row.open_fee = fee.clone();
    official_row.close_yesterday_fee = fee.clone();
    official_row.close_today_fee = fee;
    official_row.source_updated_at = Some("2026-08-18T00:00:00+08:00".to_owned());
    let mut uncorroborated_row = official_row.clone();
    uncorroborated_row.trading_status = future_meta::model::TradingStatus::Trading;
    uncorroborated_row.buy_margin_rate = Some(12.0);
    uncorroborated_row.sell_margin_rate = Some(13.0);
    uncorroborated_row.is_main_contract = true;
    insert_official_fixture_rows(&mut conn, &[uncorroborated_row], "2026-08-24T00:00:00Z").unwrap();
    conn.execute(
        "update fee_versions set source_kind = 'official'
         where valid_from = '2026-08-18T00:00:00+08:00'",
        [],
    )
    .unwrap();
    let contract_id: i64 = conn
        .query_row(
            "select id from contracts where symbol = 'GFEX.lc2708'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let rule_hash = row_rule_hash(&official_row);
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(?1, '2026-08-18T00:00:00+08:00', ?2, 'paired_official',
                  'http://www.gfex.com.cn/notice', ?3, '2026-08-24T00:00:00Z')",
        rusqlite::params![contract_id, rule_hash, "c".repeat(64)],
    )
    .unwrap();

    replace_with_official_parameter_history(
        &mut conn,
        &[OfficialHistoryRow {
            row: official_row,
            coverage_end_exclusive: "2026-08-19T00:00:00+08:00".to_owned(),
            evidence_level: OfficialEvidenceLevel::PairedOfficial,
            evidence: vec![OfficialEvidenceReference {
                canonical_url: "http://www.gfex.com.cn/notice".to_owned(),
                body_sha256: "c".repeat(64),
            }],
        }],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();

    let closure: i64 = conn
        .query_row(
            "select count(*) from fee_version_evidence evidence
             join fee_versions version
               on version.contract_id = evidence.contract_id
              and version.valid_from = evidence.valid_from
              and version.rule_hash = evidence.rule_hash",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(closure, 1);
}

#[test]
fn paired_official_rebuild_adds_new_evidence_for_matching_fee_tuple() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "GFEX.ps2708".to_owned();
    row.listing_date = Some("20260701".to_owned());
    row.expiry_date = Some("20270801".to_owned());
    row.source_updated_at = Some("2026-08-18T00:00:00+08:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[row.clone()], "2026-08-24T00:00:00Z").unwrap();
    let contract_id: i64 = conn
        .query_row(
            "select id from contracts where symbol = 'GFEX.ps2708'",
            [],
            |record| record.get(0),
        )
        .unwrap();
    let rule_hash: String = conn
        .query_row("select rule_hash from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(?1, '2026-08-18T00:00:00+08:00', ?2, 'paired_official',
                  'http://www.gfex.com.cn/existing', ?3, '2026-08-24T00:00:00Z')",
        rusqlite::params![contract_id, rule_hash, "a".repeat(64)],
    )
    .unwrap();

    replace_with_official_parameter_history(
        &mut conn,
        &[OfficialHistoryRow {
            row,
            coverage_end_exclusive: "2026-08-19T00:00:00+08:00".to_owned(),
            evidence_level: OfficialEvidenceLevel::PairedOfficial,
            evidence: vec![OfficialEvidenceReference {
                canonical_url: "http://www.gfex.com.cn/verified".to_owned(),
                body_sha256: "b".repeat(64),
            }],
        }],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();

    let evidence_count: i64 = conn
        .query_row("select count(*) from fee_version_evidence", [], |record| {
            record.get(0)
        })
        .unwrap();
    assert_eq!(evidence_count, 2);
}

#[test]
fn review_rekey_restores_orphaned_fee_evidence_without_discarding_it() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.5),
        raw_text: Some("0.5元".to_owned()),
    };
    conn.execute(
        "insert into contracts(
           symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         ) values(
           'GFEX.lc2708', '20260701', '20270801', 1, 1,
           '2026-08-23T04:00:00Z', '2026-08-23T04:00:00Z', 0
         )",
        [],
    )
    .unwrap();
    let contract_id = conn.last_insert_rowid();
    let fee_json = serde_json::to_string(&fee).unwrap();
    conn.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         ) values(
           ?1, 'legacy-version-hash', null, null, ?2, ?2, ?2,
           'Unknown', 0, 'official', '2026-08-18T00:00:00+08:00',
           '2026-08-18T00:00:00+08:00', null,
           '2026-08-23T04:00:00Z', '2026-08-23T04:00:00Z'
         )",
        rusqlite::params![contract_id, fee_json],
    )
    .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(
           ?1, '2026-08-18T00:00:00+08:00', 'legacy-evidence-hash',
           'paired_official', 'http://www.gfex.com.cn/notice', ?2,
           '2026-08-23T04:00:00Z'
         )",
        rusqlite::params![contract_id, "a".repeat(64)],
    )
    .unwrap();

    let report = rekey_fee_history_for_review(&mut conn, "2026-08-24T00:00:00Z").unwrap();

    assert_eq!(report.fee_versions, 1);
    assert_eq!(report.evidence, 1);
    validate_fee_history_integrity(&conn).unwrap();
    let rule_hash: String = conn
        .query_row("select rule_hash from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();
    let evidence_hash: String = conn
        .query_row("select rule_hash from fee_version_evidence", [], |record| {
            record.get(0)
        })
        .unwrap();
    assert_eq!(evidence_hash, rule_hash);
    let audit_count: i64 = conn
        .query_row(
            "select count(*) from review_fee_history_repairs where repair_kind = 'fee_hash_rekey'",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1);
}

#[test]
fn review_time_repair_requires_explicit_decision_for_reverse_observation() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.5),
        raw_text: Some("0.5元".to_owned()),
    };
    conn.execute(
        "insert into contracts(
           symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         ) values(
           'GFEX.ps2708', '20260701', '20270801', 1, 1,
           '2026-08-23T04:00:00Z', '2026-08-23T04:00:00Z', 0
         )",
        [],
    )
    .unwrap();
    let contract_id = conn.last_insert_rowid();
    let fee_json = serde_json::to_string(&fee).unwrap();
    conn.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         ) values(
           ?1, 'fee-hash', null, null, ?2, ?2, ?2,
           'Unknown', 0, 'official', '2026-08-18T00:00:00+08:00',
           '2026-08-18T00:00:00+08:00', null,
           '2026-08-23T04:33:39.127331Z', '2026-08-23T04:00:00Z'
         )",
        rusqlite::params![contract_id, fee_json],
    )
    .unwrap();

    let error =
        repair_review_observation_times(&mut conn, &[], "2026-08-24T00:00:00Z").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("missing review observation-time repair")
    );
    let report = repair_review_observation_times(
        &mut conn,
        &[ReviewObservationTimeRepair {
            symbol: "GFEX.ps2708".to_owned(),
            valid_from: "2026-08-18T00:00:00+08:00".to_owned(),
            last_seen_at: "2026-08-23T04:33:39.127331Z".to_owned(),
            rationale: "The upstream timestamp belongs in source_updated_at, not last_seen_at."
                .to_owned(),
        }],
        "2026-08-24T00:00:00Z",
    )
    .unwrap();

    assert_eq!(report, 1);
    let repaired_last_seen_at: String = conn
        .query_row("select last_seen_at from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();
    assert_eq!(repaired_last_seen_at, "2026-08-23T04:33:39.127331Z");
    let audit_count: i64 = conn
        .query_row(
            "select count(*) from review_fee_history_repairs where repair_kind = 'observation_time'",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert_eq!(audit_count, 1);
    let repeat = repair_review_observation_times(
        &mut conn,
        &[ReviewObservationTimeRepair {
            symbol: "GFEX.ps2708".to_owned(),
            valid_from: "2026-08-18T00:00:00+08:00".to_owned(),
            last_seen_at: "2026-08-23T04:33:39.127331Z".to_owned(),
            rationale: "The upstream timestamp belongs in source_updated_at, not last_seen_at."
                .to_owned(),
        }],
        "2026-08-24T00:01:00Z",
    )
    .unwrap();
    assert_eq!(repeat, 0);
}

#[test]
fn czce_parameter_import_admits_historical_contract_from_product_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "CZCE.SR999".to_owned();
    exemplar.lot_size = 10.0;
    exemplar.tick_size = 1.0;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);
    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, _) = write_czce_parameter_fixture(directory.path(), &html);

    import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    let conn = connect(&db_path).unwrap();
    let contract: (String, f64, f64, String, String) = conn
        .query_row(
            "select c.listing_date, c.lot_size, c.tick_size, v.source_kind, s.source_kind
             from contracts c join fee_versions v on v.contract_id = c.id
             join contract_spec_versions s on s.contract_id = c.id
             where c.symbol = 'CZCE.SR005'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(
        contract,
        (
            "20200102".to_owned(),
            10.0,
            1.0,
            "official".to_owned(),
            "official".to_owned()
        )
    );
}

#[test]
fn czce_parameter_presence_fills_missing_observed_lifecycle_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = None;
    baseline.expiry_date = None;
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);
    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, _) = write_czce_parameter_fixture(directory.path(), &html);

    import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    let conn = connect(&db_path).unwrap();
    let dates: (String, Option<String>) = conn
        .query_row(
            "select listing_date, expiry_date from contracts where symbol = 'CZCE.SR005'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(dates, ("20200102".to_owned(), None));
}

#[test]
fn czce_parameter_history_removes_contradicted_lower_confidence_versions() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[baseline.clone()], "2019-12-31T22:00:00+08:00")
        .unwrap();
    let mut lower = baseline;
    lower.open_fee.value = Some(99.0);
    lower.close_yesterday_fee.value = Some(99.0);
    lower.close_today_fee.value = Some(99.0);
    lower.source_updated_at = Some("2020-01-03 22:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[lower], "2020-01-03T22:00:00+08:00").unwrap();
    drop(conn);

    let first = czce_parameter_html("3.00", "绝对值", "0.00");
    let second = first.replace("2020-01-02", "2020-01-04");
    let first_hash = hex::encode(Sha256::digest(first.as_bytes()));
    let second_hash = hex::encode(Sha256::digest(second.as_bytes()));
    std::fs::write(directory.path().join(format!("{first_hash}.htm")), &first).unwrap();
    std::fs::write(directory.path().join(format!("{second_hash}.htm")), &second).unwrap();
    let manifest = directory.path().join("manifest.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\tsha256\turl\tbyte_count\tcontent_type\n20200102\tok\t{first_hash}\thttps://www.czce.com.cn/cn/DFSStaticFiles/Future/2020/20200102/FutureDataClearParams.htm\t{}\ttext/html\n20200104\tok\t{second_hash}\thttps://www.czce.com.cn/cn/DFSStaticFiles/Future/2020/20200104/FutureDataClearParams.htm\t{}\ttext/html\n",
            first.len(), second.len()
        ),
    )
    .unwrap();

    import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    let conn = connect(&db_path).unwrap();
    let lower_count: i64 = conn
        .query_row(
            "select count(*) from fee_versions v join contracts c on c.id = v.contract_id
             where c.symbol = 'CZCE.SR005' and v.source_kind != 'official'
               and v.valid_from >= '2020-01-02T00:00:00+08:00'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(lower_count, 0);
}

#[test]
fn paired_official_history_import_materializes_listing_tuple_and_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "CFFEX.IF9999".to_owned();
    exemplar.lot_size = 300.0;
    exemplar.tick_size = 0.2;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let notice = b"official listing notice";
    let parameter = b"official settlement parameter";
    let notice_sha = hex::encode(Sha256::digest(notice));
    let parameter_sha = hex::encode(Sha256::digest(parameter));
    std::fs::write(directory.path().join(format!("{notice_sha}.html")), notice).unwrap();
    std::fs::write(
        directory.path().join(format!("{parameter_sha}.csv")),
        parameter,
    )
    .unwrap();
    let input = directory.path().join("adjustments.json");
    std::fs::write(
        &input,
        format!(
            r#"[{{
              "symbol":"CFFEX.IF2001",
              "effective_at":"2020-01-02T00:00:00+08:00",
              "scope":"listing-day fee schedule",
              "open_fee":{{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"万分之0.23"}},
              "close_yesterday_fee":{{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"万分之0.23"}},
              "close_today_fee":{{"kind":"TurnoverRatePerTenThousand","value":3.45,"raw_text":"万分之3.45"}},
              "previous_fees":null,
              "evidence":[
                {{"canonical_url":"http://www.cffex.com.cn/cn/jystz/20200101/1.html","mirror_url":null,"sha256":"{notice_sha}","published_at":"2020-01-01T00:00:00+08:00","kind":"notice"}},
                {{"canonical_url":"http://www.cffex.com.cn/sj/jscs/202001/02/20200102_1.csv","mirror_url":null,"sha256":"{parameter_sha}","published_at":"2020-01-02T00:00:00+08:00","kind":"settlement_parameter"}}
              ]
            }}]"#
        ),
    )
    .unwrap();

    let result = import_adjustments(&OfficialHistoryImportOptions {
        history_db: db_path.clone(),
        inputs: vec![input],
        evidence_db: None,
        exchange: None,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        through: Date::from_calendar_date(2020, Month::December, 31).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.adjustments, 1);
    assert_eq!(result.versions, 1);
    let conn = connect(&db_path).unwrap();
    let (listing_date, source_kind, evidence_count, level): (String, String, i64, String) = conn
        .query_row(
            "select c.listing_date, v.source_kind, count(e.canonical_url), min(e.evidence_level)
             from contracts c join fee_versions v on v.contract_id = c.id
             join fee_version_evidence e on e.contract_id = v.contract_id
               and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'CFFEX.IF2001' group by c.id, v.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(listing_date, "20200102");
    assert_eq!(source_kind, "official");
    assert_eq!(evidence_count, 2);
    assert_eq!(level, "paired_official");
}

#[test]
fn paired_official_history_reconstructs_partial_adjustment_from_prior_tuple() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "CFFEX.IF9999".to_owned();
    exemplar.lot_size = 300.0;
    exemplar.tick_size = 0.2;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);
    let notice = b"official notice";
    let parameter = b"official parameter";
    let notice_sha = hex::encode(Sha256::digest(notice));
    let parameter_sha = hex::encode(Sha256::digest(parameter));
    std::fs::write(directory.path().join(format!("{notice_sha}.html")), notice).unwrap();
    std::fs::write(
        directory.path().join(format!("{parameter_sha}.csv")),
        parameter,
    )
    .unwrap();
    let evidence = format!(
        r#"[
          {{"canonical_url":"http://www.cffex.com.cn/cn/jystz/20200101/1.html","mirror_url":null,"sha256":"{notice_sha}","published_at":"2020-01-01T00:00:00+08:00","kind":"notice"}},
          {{"canonical_url":"http://www.cffex.com.cn/sj/jscs/202001/02/20200102_1.csv","mirror_url":null,"sha256":"{parameter_sha}","published_at":"2020-01-02T00:00:00+08:00","kind":"settlement_parameter"}}
        ]"#
    );
    let input = directory.path().join("adjustments.json");
    std::fs::write(
        &input,
        format!(
            r#"[
              {{"symbol":"CFFEX.IF2001","effective_at":"2020-01-02T00:00:00+08:00","scope":"listing","open_fee":{{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"0.23"}},"close_yesterday_fee":{{"kind":"TurnoverRatePerTenThousand","value":0.23,"raw_text":"0.23"}},"close_today_fee":{{"kind":"TurnoverRatePerTenThousand","value":3.45,"raw_text":"3.45"}},"previous_fees":null,"evidence":{evidence}}},
              {{"symbol":"CFFEX.IF2001","effective_at":"2020-01-02T00:00:00+08:00","scope":"close-today adjustment","open_fee":null,"close_yesterday_fee":null,"close_today_fee":{{"kind":"TurnoverRatePerTenThousand","value":2.3,"raw_text":"2.3"}},"previous_fees":null,"evidence":{evidence}}}
            ]"#
        ),
    )
    .unwrap();

    let result = import_adjustments(&OfficialHistoryImportOptions {
        history_db: db_path.clone(),
        inputs: vec![input],
        evidence_db: None,
        exchange: None,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        through: Date::from_calendar_date(2020, Month::December, 31).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.versions, 1);
    let conn = connect(&db_path).unwrap();
    let tuple: (String, String) = conn
        .query_row(
            "select open_fee_json, close_today_fee_json from fee_versions v
             join contracts c on c.id = v.contract_id where c.symbol = 'CFFEX.IF2001'
             order by v.valid_from desc limit 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let open: future_meta::model::FeeSpec = serde_json::from_str(&tuple.0).unwrap();
    let close_today: future_meta::model::FeeSpec = serde_json::from_str(&tuple.1).unwrap();
    assert_eq!(open.value, Some(0.23));
    assert_eq!(close_today.value, Some(2.3));
}

#[test]
fn official_metadata_import_replaces_lifecycle_and_specification_history() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "SHFE.cu2607".to_owned();
    row.listing_date = None;
    row.expiry_date = None;
    insert_official_fixture_rows(&mut conn, &[row], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let evidence = b"official SHFE contract specification";
    let sha256 = hex::encode(Sha256::digest(evidence));
    std::fs::write(directory.path().join(format!("{sha256}.html")), evidence).unwrap();
    let manifest = directory.path().join("metadata.tsv");
    std::fs::write(
        &manifest,
        format!(
            "symbol\tlisting_date\texpiry_date\tvalid_from\tvalid_to\tlot_size\ttick_size\tlifecycle_url\tlifecycle_sha256\tspecification_url\tspecification_sha256\n\
             SHFE.cu2607\t2025-07-16\t2026-07-15\t2025-07-16T00:00:00+08:00\t\t5\t10\thttps://www.shfe.com.cn/products/futures/metal/cu_f/\t{sha256}\thttps://www.shfe.com.cn/products/futures/metal/cu_f/\t{sha256}\n"
        ),
    )
    .unwrap();

    let result = import_contract_metadata(&OfficialMetadataImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.contracts, 1);
    assert_eq!(result.specification_versions, 1);
    let conn = connect(&db_path).unwrap();
    let persisted: (String, String, String, String, String, i64) = conn
        .query_row(
            "select c.listing_date, c.expiry_date, s.source_kind, s.source_url,
                    e.body_sha256, count(l.canonical_url)
             from contracts c
             join contract_spec_versions s on s.contract_id = c.id
             join contract_spec_evidence e on e.contract_id = s.contract_id
               and e.valid_from = s.valid_from
             join contract_lifecycle_evidence l on l.contract_id = c.id
             where c.symbol = 'SHFE.cu2607'
             group by c.id, s.id, e.body_sha256",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                    record.get(4)?,
                    record.get(5)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(persisted.0, "20250716");
    assert_eq!(persisted.1, "20260715");
    assert_eq!(persisted.2, "official");
    assert_eq!(
        persisted.3,
        "https://www.shfe.com.cn/products/futures/metal/cu_f/"
    );
    assert_eq!(persisted.4, sha256);
    assert_eq!(persisted.5, 1);
}

#[test]
fn contract_base_info_import_sets_exact_shfe_lifecycle_with_retained_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut rows = Vec::new();
    for symbol in ["SHFE.cu2001", "SHFE.cu2002"] {
        let mut row = official_fixture_rows().remove(0);
        row.symbol = symbol.to_owned();
        row.listing_date = None;
        row.expiry_date = None;
        rows.push(row);
    }
    insert_official_fixture_rows(&mut conn, &rows, "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = br#"{"ContractBaseInfo":[
      {"INSTRUMENTID":"cu2001","OPENDATE":"20190116","EXPIREDATE":"20200115","TRADINGDAY":"20200102"},
      {"INSTRUMENTID":"cu2002 ","OPENDATE":"20190218","EXPIREDATE":"20200217","TRADINGDAY":"20200102"}
    ]}"#;
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.dat")), body).unwrap();
    let manifest = directory.path().join("contract-base-info.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\treport_date\tcanonical_url\tsha256\trecord_count\n\
             SHFE\t20200102\thttps://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo20200102.dat\t{sha256}\t2\n"
        ),
    )
    .unwrap();

    let result = import_contract_base_info(&ContractBaseInfoImportOptions {
        history_db: db_path.clone(),
        exchange: "SHFE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.snapshots, 1);
    assert_eq!(result.contracts, 2);
    assert_eq!(result.evidence_links, 2);
    let conn = connect(&db_path).unwrap();
    let lifecycles = conn
        .prepare(
            "select c.symbol, c.listing_date, c.expiry_date, l.canonical_url, l.body_sha256
             from contracts c join contract_lifecycle_evidence l on l.contract_id = c.id
             where c.symbol like 'SHFE.%' order by c.symbol",
        )
        .unwrap()
        .query_map([], |record| {
            Ok((
                record.get::<_, String>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, String>(2)?,
                record.get::<_, String>(3)?,
                record.get::<_, String>(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(lifecycles.len(), 2);
    assert_eq!(lifecycles[0].0, "SHFE.cu2001");
    assert_eq!(lifecycles[0].1, "20190116");
    assert_eq!(lifecycles[0].2, "20200115");
    assert_eq!(lifecycles[0].4, sha256);
    assert_eq!(lifecycles[1].0, "SHFE.cu2002");
    assert_eq!(lifecycles[1].1, "20190218");
    assert_eq!(lifecycles[1].2, "20200217");
}

#[test]
fn contract_base_info_import_rejects_incomplete_database_coverage_atomically() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut rows = Vec::new();
    for symbol in ["INE.sc2001", "INE.sc2002"] {
        let mut row = official_fixture_rows().remove(0);
        row.symbol = symbol.to_owned();
        row.listing_date = None;
        row.expiry_date = None;
        rows.push(row);
    }
    insert_official_fixture_rows(&mut conn, &rows, "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = br#"{"ContractBaseInfo":[
      {"INSTRUMENTID":"sc2001","OPENDATE":"20190102","EXPIREDATE":"20200102","TRADINGDAY":"20200102"}
    ]}"#;
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.dat")), body).unwrap();
    let manifest = directory.path().join("contract-base-info.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\treport_date\tcanonical_url\tsha256\trecord_count\n\
             INE\t20200102\thttps://www.ine.cn/data/busiparamdata/future/ContractBaseInfo20200102.dat\t{sha256}\t1\n"
        ),
    )
    .unwrap();

    let error = import_contract_base_info(&ContractBaseInfoImportOptions {
        history_db: db_path.clone(),
        exchange: "INE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap_err();

    assert!(error.to_string().contains("INE.sc2002"));
    let conn = connect(&db_path).unwrap();
    let changed: i64 = conn
        .query_row(
            "select count(*) from contracts
             where symbol like 'INE.%' and listing_date is not null",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert_eq!(changed, 0);
}

#[test]
fn contract_base_info_import_rejects_duplicate_report_dates() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "SHFE.cu2001".to_owned();
    row.listing_date = None;
    row.expiry_date = None;
    insert_official_fixture_rows(&mut conn, &[row], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let bodies = [
        br#"{"ContractBaseInfo":[{"INSTRUMENTID":"cu2001","OPENDATE":"20190116","EXPIREDATE":"20200115","TRADINGDAY":"20200102"}]}"#.as_slice(),
        br#"{"ContractBaseInfo":[{"INSTRUMENTID":"cu2001","OPENDATE":"20190116","EXPIREDATE":"20200115","TRADINGDAY":"20200102"}],"revision":2}"#.as_slice(),
    ];
    let hashes = bodies
        .iter()
        .map(|body| {
            let sha256 = hex::encode(Sha256::digest(body));
            std::fs::write(directory.path().join(format!("{sha256}.dat")), body).unwrap();
            sha256
        })
        .collect::<Vec<_>>();
    let url = "https://www.shfe.com.cn/data/busiparamdata/future/ContractBaseInfo20200102.dat";
    let manifest = directory.path().join("contract-base-info.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\treport_date\tcanonical_url\tsha256\trecord_count\n\
             SHFE\t20200102\t{url}\t{}\t1\n\
             SHFE\t20200102\t{url}\t{}\t1\n",
            hashes[0], hashes[1]
        ),
    )
    .unwrap();

    let error = import_contract_base_info(&ContractBaseInfoImportOptions {
        history_db: db_path,
        exchange: "SHFE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap_err();

    assert!(error.to_string().contains("duplicate report date"));
}

#[test]
fn contract_base_info_import_uses_latest_official_expiry_revision() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "INE.sc2010".to_owned();
    row.listing_date = None;
    row.expiry_date = None;
    insert_official_fixture_rows(&mut conn, &[row], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let snapshots = [
        (
            "20200102",
            "20200930",
            br#"{"ContractBaseInfo":[{"INSTRUMENTID":"sc2010","OPENDATE":"20190924","EXPIREDATE":"20200930","TRADINGDAY":"20200102"}]}"#.as_slice(),
        ),
        (
            "20200203",
            "20200923",
            br#"{"ContractBaseInfo":[{"INSTRUMENTID":"sc2010","OPENDATE":"20190924","EXPIREDATE":"20200923","TRADINGDAY":"20200203"}]}"#.as_slice(),
        ),
    ];
    let manifest = directory.path().join("contract-base-info.tsv");
    let mut manifest_body =
        "exchange\treport_date\tcanonical_url\tsha256\trecord_count\n".to_owned();
    let mut latest_sha = String::new();
    for (report_date, _, body) in snapshots {
        let sha256 = hex::encode(Sha256::digest(body));
        std::fs::write(directory.path().join(format!("{sha256}.dat")), body).unwrap();
        writeln!(
            manifest_body,
            "INE\t{report_date}\thttps://www.ine.cn/data/busiparamdata/future/ContractBaseInfo{report_date}.dat\t{sha256}\t1"
        )
        .unwrap();
        latest_sha = sha256;
    }
    std::fs::write(&manifest, manifest_body).unwrap();

    let result = import_contract_base_info(&ContractBaseInfoImportOptions {
        history_db: db_path.clone(),
        exchange: "INE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.evidence_links, 1);
    let conn = connect(&db_path).unwrap();
    let lifecycle: (String, String, i64) = conn
        .query_row(
            "select c.expiry_date, l.body_sha256, count(*)
             from contracts c join contract_lifecycle_evidence l on l.contract_id = c.id
             where c.symbol = 'INE.sc2010' group by c.id, l.body_sha256",
            [],
            |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, ("20200923".to_owned(), latest_sha, 1));
}

#[test]
fn product_spec_import_maps_official_product_versions_to_contract_lifetimes() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut rows = Vec::new();
    for (symbol, listing, expiry) in [
        ("INE.ec2404", "20230818", "20240429"),
        ("INE.ec2606", "20250210", "20260629"),
    ] {
        let mut row = official_fixture_rows().remove(0);
        row.symbol = symbol.to_owned();
        row.listing_date = Some(listing.to_owned());
        row.expiry_date = Some(expiry.to_owned());
        row.lot_size = 50.0;
        row.tick_size = 0.1;
        rows.push(row);
    }
    insert_official_fixture_rows(&mut conn, &rows, "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let old_body = b"official EC standard contract: multiplier 50, tick 0.1";
    let old_sha = hex::encode(Sha256::digest(old_body));
    std::fs::write(directory.path().join(format!("{old_sha}.html")), old_body).unwrap();
    let new_body = b"official EC revised standard contract: multiplier 50, tick 0.5";
    let new_sha = hex::encode(Sha256::digest(new_body));
    std::fs::write(directory.path().join(format!("{new_sha}.html")), new_body).unwrap();
    let manifest = directory.path().join("product-spec.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\tproduct\tvalid_from\tvalid_to\tlot_size\ttick_size\tcanonical_url\tsha256\n\
             INE\tec\t2023-08-17\t2026-05-11\t50\t0.1\thttps://www.ine.com.cn/products/futures/index_f/ec_f/standard_ec_f/202312/t20231205_802544.html\t{old_sha}\n\
             INE\tec\t2026-05-11\t\t50\t0.5\thttps://www.ine.com.cn/products/futures/index_f/ec_f/standard_ec_f/202605/t20260511_831625.html\t{new_sha}\n"
        ),
    )
    .unwrap();

    let result = import_product_specs(&ProductSpecImportOptions {
        history_db: db_path.clone(),
        exchange: "INE".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.products, 1);
    assert_eq!(result.contracts, 2);
    assert_eq!(result.versions, 3);
    let conn = connect(&db_path).unwrap();
    let versions = conn
        .prepare(
            "select c.symbol, s.tick_size, s.valid_from, s.valid_to,
                    s.source_kind, e.body_sha256
             from contracts c join contract_spec_versions s on s.contract_id = c.id
             join contract_spec_evidence e on e.contract_id = s.contract_id
               and e.valid_from = s.valid_from
             where c.symbol like 'INE.ec%' order by c.symbol, s.valid_from",
        )
        .unwrap()
        .query_map([], |record| {
            Ok((
                record.get::<_, String>(0)?,
                record.get::<_, f64>(1)?,
                record.get::<_, String>(2)?,
                record.get::<_, Option<String>>(3)?,
                record.get::<_, String>(4)?,
                record.get::<_, String>(5)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(versions.len(), 3);
    assert_eq!(versions[0].0, "INE.ec2404");
    assert!((versions[0].1 - 0.1).abs() < f64::EPSILON);
    assert_eq!(versions[0].2, "2023-08-18T00:00:00+08:00");
    assert_eq!(versions[0].3.as_deref(), Some("2024-04-30T00:00:00+08:00"));
    assert_eq!(versions[0].4, "official");
    assert_eq!(versions[0].5, old_sha);
    assert_eq!(versions[1].0, "INE.ec2606");
    assert!((versions[1].1 - 0.1).abs() < f64::EPSILON);
    assert_eq!(versions[1].3.as_deref(), Some("2026-05-11T00:00:00+08:00"));
    assert!((versions[2].1 - 0.5).abs() < f64::EPSILON);
    assert_eq!(versions[2].2, "2026-05-11T00:00:00+08:00");
    assert_eq!(versions[2].3.as_deref(), Some("2026-06-30T00:00:00+08:00"));
    assert_eq!(versions[2].5, new_sha);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cffex_metadata_import_uses_calendar_expiry_and_splits_ts_tick_history() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "CFFEX.TS2312".to_owned();
    row.listing_date = Some("20230313".to_owned());
    row.expiry_date = None;
    row.lot_size = 20_000.0;
    row.tick_size = 0.002;
    row.source_updated_at = Some("2023-03-13 00:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2026-08-24T00:00:00Z").unwrap();

    let listing_body = b"official TS2312 listing notice";
    let listing_sha = hex::encode(Sha256::digest(listing_body));
    std::fs::write(
        directory.path().join(format!("{listing_sha}.html")),
        listing_body,
    )
    .unwrap();
    let (contract_id, valid_from, rule_hash): (i64, String, String) = conn
        .query_row(
            "select c.id, v.valid_from, v.rule_hash from contracts c
             join fee_versions v on v.contract_id = c.id
             where c.symbol = 'CFFEX.TS2312' order by v.valid_from limit 1",
            [],
            |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?)),
        )
        .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(?1, ?2, ?3, 'paired_official', ?4, ?5, ?6)",
        rusqlite::params![
            contract_id,
            valid_from,
            rule_hash,
            "http://www.cffex.com.cn/cn/jystz/20230310/1.html",
            listing_sha,
            "2026-08-24T00:00:00Z"
        ],
    )
    .unwrap();
    drop(conn);

    let old_spec = b"official initial TS contract: tick 0.005";
    let old_spec_sha = hex::encode(Sha256::digest(old_spec));
    std::fs::write(
        directory.path().join(format!("{old_spec_sha}.pdf")),
        old_spec,
    )
    .unwrap();
    let new_spec = b"official TS change: tick 0.002";
    let new_spec_sha = hex::encode(Sha256::digest(new_spec));
    std::fs::write(
        directory.path().join(format!("{new_spec_sha}.html")),
        new_spec,
    )
    .unwrap();
    let calendar = b"<docs><doc><pubdate>2023-12-08</pubdate><title><![CDATA[ TS2312\
        \xE6\x9C\x80\xE5\x90\x8E\xE4\xBA\xA4\xE6\x98\x93\xE6\x97\xA5 ]]></title></doc></docs>";
    let calendar_sha = hex::encode(Sha256::digest(calendar));
    std::fs::write(
        directory.path().join(format!("{calendar_sha}.xml")),
        calendar,
    )
    .unwrap();

    let products = directory.path().join("cffex-products.tsv");
    std::fs::write(
        &products,
        format!(
            "product\tvalid_from\tvalid_to\tlot_size\ttick_size\texpiry_rule\tspecification_url\tspecification_sha256\n\
             TS\t2018-08-17T00:00:00+08:00\t2023-11-06T00:00:00+08:00\t20000\t0.005\tsecond_friday\thttp://www.cffex.com.cn/u/cms/www/201808/ts.pdf\t{old_spec_sha}\n\
             TS\t2023-11-06T00:00:00+08:00\t\t20000\t0.002\tsecond_friday\thttp://www.cffex.com.cn/cn/jystz/20231020/34772.html\t{new_spec_sha}\n"
        ),
    )
    .unwrap();
    let calendars = directory.path().join("cffex-calendars.tsv");
    std::fs::write(
        &calendars,
        format!(
            "month\tcanonical_url\tsha256\n\
             2023-12\thttp://www.cffex.com.cn/sj/jyrl/202312/index_6782.xml\t{calendar_sha}\n"
        ),
    )
    .unwrap();

    let result = import_cffex_metadata(&CffexMetadataImportOptions {
        history_db: db_path.clone(),
        product_manifest: products,
        calendar_manifest: calendars,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.contracts, 1);
    assert_eq!(result.specification_versions, 2);
    let conn = connect(&db_path).unwrap();
    let lifecycle: (String, i64) = conn
        .query_row(
            "select c.expiry_date, count(l.canonical_url) from contracts c
             join contract_lifecycle_evidence l on l.contract_id = c.id
             where c.symbol = 'CFFEX.TS2312' group by c.id",
            [],
            |record| Ok((record.get(0)?, record.get(1)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, ("20231208".to_owned(), 2));
    let versions = conn
        .prepare(
            "select s.tick_size, s.valid_from, s.valid_to, s.source_kind
             from contract_spec_versions s join contracts c on c.id = s.contract_id
             where c.symbol = 'CFFEX.TS2312' order by valid_from",
        )
        .unwrap()
        .query_map([], |record| {
            Ok((
                record.get::<_, f64>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, Option<String>>(2)?,
                record.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(versions.len(), 2);
    assert!((versions[0].0 - 0.005).abs() < f64::EPSILON);
    assert_eq!(versions[0].1, "2023-03-13T00:00:00+08:00");
    assert_eq!(versions[0].2.as_deref(), Some("2023-11-06T00:00:00+08:00"));
    assert!((versions[1].0 - 0.002).abs() < f64::EPSILON);
    assert_eq!(versions[1].3, "official");
}

#[test]
fn product_spec_import_accepts_gfex_official_contract_page() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "GFEX.si2308".to_owned();
    row.listing_date = Some("20221222".to_owned());
    row.expiry_date = Some("20230814".to_owned());
    row.lot_size = 5.0;
    row.tick_size = 5.0;
    let mut incomplete = row.clone();
    incomplete.symbol = "GFEX.si9999".to_owned();
    incomplete.expiry_date = None;
    insert_official_fixture_rows(&mut conn, &[row, incomplete], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);
    let body = b"official GFEX industrial silicon contract: 5 tonnes per lot, 5 yuan tick";
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.html")), body).unwrap();
    let manifest = directory.path().join("gfex-spec.tsv");
    std::fs::write(
        &manifest,
        format!(
            "exchange\tproduct\tvalid_from\tvalid_to\tlot_size\ttick_size\tcanonical_url\tsha256\n\
             GFEX\tsi\t2022-12-22\t\t5\t5\thttps://www.gfex.com.cn/gfex/gyeg/sspz.shtml\t{sha256}\n"
        ),
    )
    .unwrap();
    let result = import_product_specs(&ProductSpecImportOptions {
        history_db: db_path,
        exchange: "GFEX".to_owned(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.contracts, 1);
}

#[test]
fn gfex_daily_settlement_import_maps_complete_fee_tuple() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "GFEX.si9999".to_owned();
    exemplar.lot_size = 5.0;
    exemplar.tick_size = 5.0;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = r#"{"code":"0","data":[{"contractId":"si2308","openFee":1,"offsetFee":1,"shortOpenFee":1,"shortOffsetFee":0,"style":"比例值"}]}"#
        .as_bytes();
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("gfex.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tproducts\tcontent_type\n\
             20221222\tok\t20221222\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList\t1\tsi\tapplication/json\n"
        ),
    )
    .unwrap();

    let result = import_daily_settlement_parameters(&GfexParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2022, Month::December, 22).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.snapshots, 1);
    assert_eq!(result.contracts, 1);
    let conn = connect(&db_path).unwrap();
    let tuple: (String, String, String, String) = conn
        .query_row(
            "select v.open_fee_json, v.close_yesterday_fee_json,
                    v.close_today_fee_json, e.evidence_level
             from fee_versions v join contracts c on c.id = v.contract_id
             join fee_version_evidence e on e.contract_id = v.contract_id
               and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'GFEX.si2308' and v.source_kind = 'official'",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                ))
            },
        )
        .unwrap();
    let open: future_meta::model::FeeSpec = serde_json::from_str(&tuple.0).unwrap();
    let close_yesterday: future_meta::model::FeeSpec = serde_json::from_str(&tuple.1).unwrap();
    let close_today: future_meta::model::FeeSpec = serde_json::from_str(&tuple.2).unwrap();
    assert_eq!(open.value, Some(1.0));
    assert_eq!(close_yesterday.value, Some(1.0));
    assert_eq!(close_today.kind, future_meta::model::FeeKind::Zero);
    assert_eq!(tuple.3, "official_parameter");
}

#[test]
fn dce_daily_settlement_import_maps_complete_fee_tuple() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "DCE.a2005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200515".to_owned());
    baseline.lot_size = 10.0;
    baseline.tick_size = 1.0;
    insert_official_fixture_rows(&mut connection, &[baseline], "2026-08-25T00:00:00Z").unwrap();
    drop(connection);

    let body = r#"{"success":true,"code":200,"data":[{"contractId":"a2005","openFee":"2","offsetFee":"2","shortOffsetFee":"4","style":"绝对值"}]}"#.as_bytes();
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("dce.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tcontent_type\n20200102\tok\t20200102\t{sha256}\thttp://www.dce.com.cn/dcereport/publicweb/tradepara/futAndOptSettle\t1\tapplication/json\n"
        ),
    )
    .unwrap();

    let result = import_dce_parameters(&DceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-25T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.snapshots, 1);
    assert_eq!(result.contracts, 1);

    let connection = connect(&db_path).unwrap();
    let tuple = connection
        .query_row(
            "select v.open_fee_json, v.close_yesterday_fee_json, v.close_today_fee_json, e.evidence_level
             from fee_versions v
             join contracts c on c.id = v.contract_id
             join fee_version_evidence e on e.contract_id = v.contract_id
                 and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'DCE.a2005' and v.source_kind = 'official'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?)),
        )
        .unwrap();
    let open: future_meta::model::FeeSpec = serde_json::from_str(&tuple.0).unwrap();
    let close_yesterday: future_meta::model::FeeSpec = serde_json::from_str(&tuple.1).unwrap();
    let close_today: future_meta::model::FeeSpec = serde_json::from_str(&tuple.2).unwrap();
    assert_eq!(open.value, Some(2.0));
    assert_eq!(close_yesterday.value, Some(2.0));
    assert_eq!(close_today.value, Some(4.0));
    assert_eq!(tuple.3, "official_parameter");
}

#[test]
fn dce_calendar_import_records_exact_lifecycle_with_two_official_events() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "DCE.a2001".to_owned();
    baseline.listing_date = None;
    baseline.expiry_date = None;
    baseline.lot_size = 10.0;
    baseline.tick_size = 1.0;
    insert_official_fixture_rows(&mut connection, &[baseline], "2026-08-25T00:00:00Z").unwrap();
    drop(connection);

    let listing = r#"{"success":true,"code":200,"data":[{"calendarDate":"20190116","contractId":"a2001期货合约","eventType":"期货合约开始交易日","tradeType":"1"}]}"#;
    let expiry = r#"{"success":true,"code":200,"data":[{"calendarDate":"20200115","contractId":"a2001期货合约","eventType":"合约最后交易日","tradeType":"0"}]}"#;
    let listing_sha = hex::encode(Sha256::digest(listing.as_bytes()));
    let expiry_sha = hex::encode(Sha256::digest(expiry.as_bytes()));
    std::fs::write(
        directory.path().join(format!("{listing_sha}.json")),
        listing,
    )
    .unwrap();
    std::fs::write(directory.path().join(format!("{expiry_sha}.json")), expiry).unwrap();
    let manifest = directory.path().join("dce-calendar.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tcontent_type\n20190116\tok\t20190116\t{listing_sha}\thttp://www.dce.com.cn/dcereport/publicweb/trading/searchNotity\t1\tapplication/json\n20200115\tok\t20200115\t{expiry_sha}\thttp://www.dce.com.cn/dcereport/publicweb/trading/searchNotity\t1\tapplication/json\n"
        ),
    )
    .unwrap();

    let result = import_dce_calendar(&DceCalendarImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-25T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.snapshots, 2);
    assert_eq!(result.contracts, 1);
    assert_eq!(result.evidence_links, 2);

    let connection = connect(&db_path).unwrap();
    let lifecycle = connection
        .query_row(
            "select listing_date, expiry_date from contracts where symbol = 'DCE.a2001'",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, ("20190116".to_owned(), "20200115".to_owned()));
}

#[test]
fn gfex_parameter_import_uses_latest_observation_when_manifest_dates_are_not_sorted() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "GFEX.si2609".to_owned();
    exemplar.lot_size = 5.0;
    exemplar.tick_size = 5.0;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = r#"{"code":"0","data":[{"contractId":"si2609","openFee":1,"offsetFee":1,"shortOffsetFee":0,"style":"比例值"}]}"#;
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("gfex-unsorted.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tproducts\tcontent_type\n\
             20260818\tok\t20260818\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList\t1\tsi\tapplication/json\n\
             20260817\tok\t20260817\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList\t1\tsi\tapplication/json\n"
        ),
    )
    .unwrap();

    let result = import_daily_settlement_parameters(&GfexParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    });

    assert!(result.is_ok(), "{result:?}");
    let connection = connect(&db_path).unwrap();
    let valid_from: String = connection
        .query_row(
            "select v.valid_from
             from fee_versions v
             join contracts c on c.id = v.contract_id
             where c.symbol = 'GFEX.si2609' and v.source_kind = 'official'
             and v.valid_from >= '2026-08-17T00:00:00+08:00' order by v.valid_from limit 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(valid_from, "2026-08-17T00:00:00+08:00");
}

#[test]
fn gfex_parameter_import_covers_contract_listing_after_pre_listing_observation() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "GFEX.lc2708".to_owned();
    exemplar.listing_date = Some("20260818".to_owned());
    exemplar.lot_size = 1.0;
    exemplar.tick_size = 20.0;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = r#"{"code":"0","data":[{"contractId":"lc2708","openFee":1.6,"offsetFee":1.6,"shortOffsetFee":3.2,"style":"比例值"}]}"#;
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("gfex-pre-listing.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tproducts\tcontent_type\n\
             20260817\tok\t20260817\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList\t1\tlc\tapplication/json\n"
        ),
    )
    .unwrap();

    let result = import_daily_settlement_parameters(&GfexParameterImportOptions {
        history_db: db_path,
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    });

    assert!(result.is_ok(), "{result:?}");
}

#[test]
fn gfex_calendar_import_records_exact_lifecycle_with_two_official_events() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "GFEX.si2308".to_owned();
    row.listing_date = Some("20230101".to_owned());
    row.expiry_date = None;
    row.lot_size = 5.0;
    row.tick_size = 5.0;
    insert_official_fixture_rows(&mut connection, &[row], "2026-08-24T00:00:00Z").unwrap();
    drop(connection);

    let body = r#"{"code":"0","data":[
      {"calendarDate":"20221222","contractId":"si2308合约","eventType":"期货合约开始交易日"},
      {"calendarDate":"20230814","contractId":"si2308合约","eventType":"合约最后交易日"}
    ]}"#
    .as_bytes();
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("gfex-calendar.tsv");
    std::fs::write(
        &manifest,
        format!(
            "report_date\tstatus\tsha256\turl\n\
             20221222\tok\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTpTradingCalendar/loadList\n"
        ),
    )
    .unwrap();

    let result = import_trading_calendar_lifecycles(&GfexCalendarImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.contracts, 1);
    assert_eq!(result.evidence_links, 1);
    let connection = connect(&db_path).unwrap();
    let lifecycle: (String, String, i64) = connection
        .query_row(
            "select c.listing_date, c.expiry_date, count(e.contract_id)
             from contracts c join contract_lifecycle_evidence e on e.contract_id = c.id
             where c.symbol = 'GFEX.si2308' group by c.id",
            [],
            |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?)),
        )
        .unwrap();
    assert_eq!(lifecycle, ("20221222".to_owned(), "20230814".to_owned(), 1));
}

#[test]
fn gfex_parameter_import_selects_pre_transition_lithium_tick() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let exemplar = official_fixture_rows().remove(0);
    let mut old = exemplar.clone();
    old.symbol = "GFEX.lc9998".to_owned();
    old.lot_size = 1.0;
    old.tick_size = 50.0;
    let mut current = exemplar;
    current.symbol = "GFEX.lc9999".to_owned();
    current.lot_size = 1.0;
    current.tick_size = 20.0;
    insert_official_fixture_rows(&mut conn, &[old, current], "2026-08-24T00:00:00Z").unwrap();
    drop(conn);

    let body = r#"{"code":"0","data":[{"contractId":"lc2408","openFee":0.8,"offsetFee":0.8,"shortOffsetFee":0.8,"style":"比例值"}]}"#.as_bytes();
    let sha256 = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{sha256}.json")), body).unwrap();
    let manifest = directory.path().join("gfex.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tsha256\turl\trecord_count\tproducts\tcontent_type\n\
             20230721\tok\t20230721\t{sha256}\thttp://www.gfex.com.cn/u/interfacesWebTiFutAndOptSettle/loadList\t1\tlc\tapplication/json\n"
        ),
    )
    .unwrap();

    import_daily_settlement_parameters(&GfexParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2023, Month::July, 21).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    let conn = connect(&db_path).unwrap();
    let tick: f64 = conn
        .query_row(
            "select tick_size from contracts where symbol = 'GFEX.lc2408'",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert!((tick - 50.0).abs() < f64::EPSILON);
}

#[test]
fn ine_daily_parameter_import_requires_and_records_close_today_rule() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = official_fixture_rows().remove(0);
    exemplar.symbol = "INE.lu2107".to_owned();
    exemplar.listing_date = Some("20210416".to_owned());
    exemplar.expiry_date = Some("20220415".to_owned());
    exemplar.lot_size = 10.0;
    exemplar.tick_size = 1.0;
    insert_official_fixture_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
    conn.execute_batch(
        "delete from contract_spec_versions
         where contract_id = (select id from contracts where symbol = 'INE.lu2107');
         insert into contract_spec_versions(
             contract_id, lot_size, tick_size, valid_from, valid_to,
             source_kind, source_url, first_seen_at, last_seen_at
         )
         select id, 10, 1, '2021-04-16T00:00:00+08:00',
                '2022-04-16T00:00:00+08:00', 'official',
                'https://www.ine.cn/products/futures/energy/lu_f/',
                '2026-08-24T00:00:00Z', '2026-08-24T00:00:00Z'
         from contracts where symbol = 'INE.lu2107';",
    )
    .unwrap();
    drop(conn);

    let body = br#"{"o_code":"0","report_date":"20210416","o_cursor":[{"PRODUCTID":"lu_f","INSTRUMENTID":"lu2107","TRADEFEERATIO":0.01,"TRADEFEEUNIT":0}]}"#;
    let parameter_sha = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{parameter_sha}.dat")), body).unwrap();
    let manifest = directory.path().join("ine.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tupdate_date\tsha256\turl\trecord_count\tproducts\n\
             20210416\tok\t20210416\t20210416 20:01:00\t{parameter_sha}\thttps://www.ine.cn/data/tradedata/future/dailydata/js20210416.dat\t1\tlu_f\n"
        ),
    )
    .unwrap();

    let rule_body = b"LU listing notice: close-today commission is not waived";
    let rule_sha = hex::encode(Sha256::digest(rule_body));
    std::fs::write(directory.path().join(format!("{rule_sha}.html")), rule_body).unwrap();
    let rules = directory.path().join("ine-close-today.tsv");
    std::fs::write(
        &rules,
        format!(
            "scope\tvalid_from\tvalid_to\tclose_today_kind\tclose_today_value\tcanonical_url\tsha256\n\
             INE.lu\t2020-06-22T00:00:00+08:00\t\tsame_as_general\t\thttps://www.ine.cn/publicnotice/notice/202006/t20200610_812278.html\t{rule_sha}\n"
        ),
    )
    .unwrap();

    let result = import_ine_parameters(&IneParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        close_today_rules: rules,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2021, Month::April, 16).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.snapshots, 1);
    assert_eq!(result.contracts, 1);
    assert_eq!(result.versions, 1);
    let conn = connect(&db_path).unwrap();
    let (open, close_yesterday, close_today, level, evidence_count): (
        String,
        String,
        String,
        String,
        i64,
    ) = conn
        .query_row(
            "select v.open_fee_json, v.close_yesterday_fee_json,
                    v.close_today_fee_json, min(e.evidence_level), count(*)
             from fee_versions v join contracts c on c.id = v.contract_id
             join fee_version_evidence e on e.contract_id = v.contract_id
               and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'INE.lu2107' and v.source_kind = 'official'
             group by v.id",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                    record.get(4)?,
                ))
            },
        )
        .unwrap();
    for json in [open, close_yesterday, close_today] {
        let fee: future_meta::model::FeeSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(
            fee.kind,
            future_meta::model::FeeKind::TurnoverRatePerTenThousand
        );
        assert_eq!(fee.value, Some(0.1));
    }
    assert_eq!(level, "paired_official");
    assert_eq!(evidence_count, 2);
}

#[test]
fn ine_daily_parameter_import_selects_official_product_spec_at_observation_date() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut old = official_fixture_rows().remove(0);
    old.symbol = "INE.ec9998".to_owned();
    old.listing_date = Some("20200101".to_owned());
    old.expiry_date = Some("20211231".to_owned());
    old.lot_size = 50.0;
    old.tick_size = 0.1;
    let mut revised = old.clone();
    revised.symbol = "INE.ec9999".to_owned();
    revised.listing_date = Some("20260511".to_owned());
    revised.expiry_date = Some("20270511".to_owned());
    revised.tick_size = 0.5;
    insert_official_fixture_rows(&mut conn, &[old, revised], "2026-08-24T00:00:00Z").unwrap();
    conn.execute_batch(
        "delete from contract_spec_versions
         where contract_id in (
             select id from contracts where symbol in ('INE.ec9998', 'INE.ec9999')
         );
         insert into contract_spec_versions(
             contract_id, lot_size, tick_size, valid_from, valid_to,
             source_kind, source_url, first_seen_at, last_seen_at
         )
         select id, 50, 0.1, '2020-01-01T00:00:00+08:00',
                '2022-01-01T00:00:00+08:00', 'official',
                'https://www.ine.cn/products/futures/index_f/ec_f/',
                '2026-08-24T00:00:00Z', '2026-08-24T00:00:00Z'
         from contracts where symbol = 'INE.ec9998';
         insert into contract_spec_versions(
             contract_id, lot_size, tick_size, valid_from, valid_to,
             source_kind, source_url, first_seen_at, last_seen_at
         )
         select id, 50, 0.5, '2026-05-11T00:00:00+08:00', null,
                'official',
                'https://www.ine.cn/products/futures/index_f/ec_f/',
                '2026-08-24T00:00:00Z', '2026-08-24T00:00:00Z'
         from contracts where symbol = 'INE.ec9999';",
    )
    .unwrap();
    drop(conn);

    let body = br#"{"o_code":"0","report_date":"20210416","o_cursor":[{"PRODUCTID":"ec_f","INSTRUMENTID":"ec2404","TRADEFEERATIO":0.01,"TRADEFEEUNIT":0}]}"#;
    let parameter_sha = hex::encode(Sha256::digest(body));
    std::fs::write(directory.path().join(format!("{parameter_sha}.dat")), body).unwrap();
    let manifest = directory.path().join("ine.tsv");
    std::fs::write(
        &manifest,
        format!(
            "requested_date\tstatus\treport_date\tupdate_date\tsha256\turl\trecord_count\tproducts\n\
             20210416\tok\t20210416\t20210416 20:01:00\t{parameter_sha}\thttps://www.ine.cn/data/tradedata/future/dailydata/js20210416.dat\t1\tec_f\n"
        ),
    )
    .unwrap();
    let rule_body = b"official EC close-today rule";
    let rule_sha = hex::encode(Sha256::digest(rule_body));
    std::fs::write(directory.path().join(format!("{rule_sha}.html")), rule_body).unwrap();
    let rules = directory.path().join("ine-close-today.tsv");
    std::fs::write(
        &rules,
        format!(
            "scope\tvalid_from\tvalid_to\tclose_today_kind\tclose_today_value\tcanonical_url\tsha256\n\
             INE.ec\t2020-01-01T00:00:00+08:00\t\tsame_as_general\t\thttps://www.ine.cn/publicnotice/notice/202308/t20230811_814262.html\t{rule_sha}\n"
        ),
    )
    .unwrap();

    import_ine_parameters(&IneParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        close_today_rules: rules,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2021, Month::April, 16).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    let conn = connect(&db_path).unwrap();
    let tick: f64 = conn
        .query_row(
            "select tick_size from contracts where symbol = 'INE.ec2404'",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert!((tick - 0.1).abs() < f64::EPSILON);
}

#[test]
fn shfe_monthly_parameter_import_pairs_general_fee_with_close_today_notice() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    let mut baseline = official_fixture_rows().remove(0);
    baseline.symbol = "SHFE.cu2001".to_owned();
    baseline.listing_date = Some("20190116".to_owned());
    baseline.expiry_date = Some("20200115".to_owned());
    insert_official_fixture_rows(&mut connection, &[baseline], "2026-08-24T00:00:00Z").unwrap();
    drop(connection);

    let parameter_body = b"official SHFE monthly parameter table";
    let parameter_sha = hex::encode(Sha256::digest(parameter_body));
    std::fs::write(
        directory.path().join(format!("{parameter_sha}.html")),
        parameter_body,
    )
    .unwrap();
    let parameters = directory.path().join("shfe-monthly.tsv");
    std::fs::write(
        &parameters,
        format!(
            "reported_month\tsource_kind\tparent_url\tsource_url\tsha256\tproduct\tcontract\tadjustment_date_text\tfee_context\traw_fee_value\tsource_cell\n\
             2020-01\thtml\thttps://www.shfe.com.cn/reports/businessdata/adjtomonthlysettlementprm/202001/t20200131_795583.html\thttps://www.shfe.com.cn/reports/businessdata/adjtomonthlysettlementprm/202001/t20200131_795583.html\t{parameter_sha}\tCU\tCU2001\t12月31日(星期二)\t交易手续费 | ‰\t0.05\tR1C1\n"
        ),
    )
    .unwrap();

    let notice_body = b"official SHFE notice: CU2001 close-today is 2 CNY";
    let notice_sha = hex::encode(Sha256::digest(notice_body));
    std::fs::write(
        directory.path().join(format!("{notice_sha}.html")),
        notice_body,
    )
    .unwrap();
    let rules = directory.path().join("shfe-close-today.tsv");
    std::fs::write(
        &rules,
        format!(
            "symbol\tvalid_from\tvalid_to\tclose_today_kind\tclose_today_value\tcanonical_url\tsha256\n\
             SHFE.cu2001\t2019-12-31T00:00:00+08:00\t\tCnyPerLot\t2\thttps://www.shfe.com.cn/publicnotice/notice/201910/t20191008_795105.html\t{notice_sha}\n"
        ),
    )
    .unwrap();

    let result = import_shfe_parameters(&ShfeParameterImportOptions {
        history_db: db_path.clone(),
        parameter_manifest: parameters,
        close_today_rules: rules,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        through: Date::from_calendar_date(2020, Month::January, 15).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.contracts, 1);
    assert_eq!(result.versions, 1);

    let connection = connect(&db_path).unwrap();
    let (open, close_yesterday, close_today, level, evidence_count): (
        String,
        String,
        String,
        String,
        i64,
    ) = connection
        .query_row(
            "select v.open_fee_json, v.close_yesterday_fee_json,
                    v.close_today_fee_json, min(e.evidence_level), count(*)
             from fee_versions v join contracts c on c.id = v.contract_id
             join fee_version_evidence e on e.contract_id = v.contract_id
               and e.valid_from = v.valid_from and e.rule_hash = v.rule_hash
             where c.symbol = 'SHFE.cu2001' and v.source_kind = 'official'
             group by v.id",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                    record.get(4)?,
                ))
            },
        )
        .unwrap();
    for json in [open, close_yesterday] {
        let fee: future_meta::model::FeeSpec = serde_json::from_str(&json).unwrap();
        assert_eq!(
            fee.kind,
            future_meta::model::FeeKind::TurnoverRatePerTenThousand
        );
        assert_eq!(fee.value, Some(0.5));
    }
    let close_today: future_meta::model::FeeSpec = serde_json::from_str(&close_today).unwrap();
    assert_eq!(close_today.kind, future_meta::model::FeeKind::CnyPerLot);
    assert_eq!(close_today.value, Some(2.0));
    assert_eq!(level, "paired_official");
    assert_eq!(evidence_count, 2);
}

fn attach_fixture_fee_evidence(connection: &rusqlite::Connection) {
    connection
        .execute(
            "insert or ignore into fee_version_evidence(
                contract_id, valid_from, rule_hash, evidence_level,
                canonical_url, body_sha256, recorded_at
             )
             select contract_id, valid_from, rule_hash, 'official_parameter',
                    'https://www.shfe.com.cn/fees/' || id,
                    lower(hex(zeroblob(32))), '2026-06-04T12:00:00+08:00'
             from fee_versions",
            [],
        )
        .unwrap();
}

#[test]
fn verified_official_adjustment_is_staged_in_an_isolated_evidence_database() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let adjustment = OfficialFeeAdjustment {
        symbol: "INE.sc2604".to_owned(),
        effective_at: "2026-03-10T00:00:00+08:00".to_owned(),
        scope: "all listed SC contracts".to_owned(),
        previous_fees: None,
        open_fee: None,
        close_yesterday_fee: None,
        close_today_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(60.0),
            raw_text: Some("60元/手".to_owned()),
        }),
        evidence: vec![
            OfficialEvidence {
                canonical_url:
                    "https://www.ine.cn/eng/circularnews/circular/202603/t20260306_830603.html"
                        .to_owned(),
                mirror_url: Some(
                    "https://www.ine.cn/publicnotice/notice/202603/t20260306_830600.html"
                        .to_owned(),
                ),
                sha256: "3ec81135a7f0f995de49c39a3178b173a29dbb1ed124b6328f26153498c310c3"
                    .to_owned(),
                published_at: "2026-03-06T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::Notice,
            },
            OfficialEvidence {
                canonical_url:
                    "https://www.ine.cn/publicnotice/notice/202603/W020260306643830614686.doc"
                        .to_owned(),
                mirror_url: None,
                sha256: "aaf0ad447304c6f0af9543680ab1ae5da6b513e818f43b6f109b1be005004997"
                    .to_owned(),
                published_at: "2026-03-06T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::FeeSchedule,
            },
        ],
    };

    let staged = stage_adjustment(&evidence_db, &adjustment).unwrap();

    assert_eq!(staged.verification, OfficialVerification::Verified);
    assert_eq!(staged.evidence_count, 2);
    let conn = rusqlite::Connection::open(&evidence_db).unwrap();
    let candidate_count: i64 = conn
        .query_row("select count(*) from official_fee_adjustments", [], |row| {
            row.get(0)
        })
        .unwrap();
    let production_history_exists: i64 = conn
        .query_row(
            "select count(*) from sqlite_master where type = 'table' and name = 'fee_versions'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(candidate_count, 1);
    assert_eq!(production_history_exists, 0);
}

#[test]
fn paired_settlement_parameters_bracketing_effective_day_verify_complete_tuple() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let rate = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(1.0),
        raw_text: Some("1/万分之".to_owned()),
    };
    let adjustment = OfficialFeeAdjustment {
        symbol: "CZCE.SA610".to_owned(),
        effective_at: "2026-06-08T00:00:00+08:00".to_owned(),
        scope: "CZCE daily settlement parameters".to_owned(),
        previous_fees: None,
        open_fee: Some(rate.clone()),
        close_yesterday_fee: Some(rate.clone()),
        close_today_fee: Some(rate),
        evidence: vec![
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260605/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260608/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                published_at: "2026-06-08T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
        ],
    };

    assert_eq!(
        stage_adjustment(&evidence_db, &adjustment)
            .unwrap()
            .verification,
        OfficialVerification::Verified
    );
}

#[test]
fn paired_settlement_parameters_must_bracket_effective_day() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(3.0),
        raw_text: Some("3元/手".to_owned()),
    };
    let adjustment = OfficialFeeAdjustment {
        symbol: "CZCE.PL610".to_owned(),
        effective_at: "2026-06-08T00:00:00+08:00".to_owned(),
        scope: "CZCE daily settlement parameters".to_owned(),
        previous_fees: None,
        open_fee: Some(fee.clone()),
        close_yesterday_fee: Some(fee),
        close_today_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::Zero,
            value: Some(0.0),
            raw_text: Some("0元/手".to_owned()),
        }),
        evidence: vec![
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260608/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                published_at: "2026-06-08T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260609/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                published_at: "2026-06-09T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
        ],
    };

    assert_eq!(
        stage_adjustment(&evidence_db, &adjustment)
            .unwrap()
            .verification,
        OfficialVerification::Provisional
    );
}

#[test]
fn paired_settlement_parameters_require_a_complete_fee_tuple() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let adjustment = OfficialFeeAdjustment {
        symbol: "CZCE.PL610".to_owned(),
        effective_at: "2026-06-08T00:00:00+08:00".to_owned(),
        scope: "CZCE daily settlement parameters".to_owned(),
        previous_fees: None,
        open_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(3.0),
            raw_text: Some("3元/手".to_owned()),
        }),
        close_yesterday_fee: None,
        close_today_fee: None,
        evidence: vec![
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260605/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
            OfficialEvidence {
                canonical_url: "https://www.czce.com.cn/cn/DFSStaticFiles/Future/2026/20260608/FutureDataClearParams.htm".to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_owned(),
                published_at: "2026-06-08T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
        ],
    };

    assert_eq!(
        stage_adjustment(&evidence_db, &adjustment)
            .unwrap()
            .verification,
        OfficialVerification::Provisional
    );
}

#[test]
fn verified_official_adjustment_with_incomplete_fee_tuple_cannot_apply() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let history_db = dir.path().join("history.sqlite");
    let adjustment = OfficialFeeAdjustment {
        symbol: "SHFE.cu2607".to_owned(),
        effective_at: "2026-06-06T00:00:00+08:00".to_owned(),
        scope: "CU2607".to_owned(),
        previous_fees: None,
        open_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(0.2),
            raw_text: Some("0.2元/手".to_owned()),
        }),
        close_yesterday_fee: None,
        close_today_fee: None,
        evidence: vec![
            OfficialEvidence {
                canonical_url: "https://www.shfe.com.cn/a.html".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::Notice,
            },
            OfficialEvidence {
                canonical_url: "https://www.shfe.com.cn/a.doc".to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::FeeSchedule,
            },
        ],
    };
    stage_adjustment(&evidence_db, &adjustment).unwrap();
    let mut history = connect(&history_db).unwrap();
    ensure_schema(&history).unwrap();
    insert_official_fixture_rows(
        &mut history,
        &official_fixture_rows(),
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();

    let error = apply_verified_adjustments(&history_db, &evidence_db, "2026-06-05T23:00:00+08:00")
        .unwrap_err();

    assert!(error.to_string().contains("complete fee tuple"));
}

#[test]
fn complete_verified_official_adjustment_requires_and_uses_matching_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let history_db = dir.path().join("history.sqlite");
    let notice_url = "https://www.shfe.com.cn/notice.html";
    let schedule_url = "https://www.shfe.com.cn/schedule.doc";
    let notice_body = "official notice";
    let schedule_body = "official schedule";
    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.2),
        raw_text: Some("0.2元/手".to_owned()),
    };
    let adjustment = OfficialFeeAdjustment {
        symbol: "SHFE.cu2607".to_owned(),
        effective_at: "2026-06-06T00:00:00+08:00".to_owned(),
        scope: "CU2607".to_owned(),
        previous_fees: None,
        open_fee: Some(fee.clone()),
        close_yesterday_fee: Some(fee.clone()),
        close_today_fee: Some(fee),
        evidence: vec![
            OfficialEvidence {
                canonical_url: notice_url.to_owned(),
                mirror_url: None,
                sha256: hex::encode(Sha256::digest(notice_body.as_bytes())),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::Notice,
            },
            OfficialEvidence {
                canonical_url: schedule_url.to_owned(),
                mirror_url: None,
                sha256: hex::encode(Sha256::digest(schedule_body.as_bytes())),
                published_at: "2026-06-05T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::FeeSchedule,
            },
        ],
    };
    stage_adjustment(&evidence_db, &adjustment).unwrap();
    let mut history = connect(&history_db).unwrap();
    ensure_schema(&history).unwrap();
    insert_official_fixture_rows(
        &mut history,
        &official_fixture_rows(),
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();
    future_meta_daemon::db::record_official_document_snapshot(
        &history,
        notice_url,
        notice_body,
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();
    future_meta_daemon::db::record_official_document_snapshot(
        &history,
        schedule_url,
        schedule_body,
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();

    let applied =
        apply_verified_adjustments(&history_db, &evidence_db, "2026-06-05T23:00:00+08:00").unwrap();

    assert_eq!(applied.adjustments, 1);
    let source: String = history
        .query_row(
            "select source_kind from fee_versions where valid_to is null",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(source, "official");
}

#[test]
fn official_staging_accepts_http_only_for_cffex_primary_domain() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let adjustment = OfficialFeeAdjustment {
        symbol: "CFFEX.IF2001".to_owned(),
        effective_at: "2020-01-01T00:00:00+08:00".to_owned(),
        scope: "all listed IF contracts".to_owned(),
        previous_fees: None,
        open_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
            value: Some(0.23),
            raw_text: Some("万分之0.23".to_owned()),
        }),
        close_yesterday_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
            value: Some(0.23),
            raw_text: Some("万分之0.23".to_owned()),
        }),
        close_today_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
            value: Some(3.45),
            raw_text: Some("万分之3.45".to_owned()),
        }),
        evidence: vec![
            OfficialEvidence {
                canonical_url: "http://www.cffex.com.cn/cn/jystz/20190419/23719.html".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                published_at: "2019-04-19T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::Notice,
            },
            OfficialEvidence {
                canonical_url: "http://www.cffex.com.cn/sj/jscs/201912/23/20191223_1.csv"
                    .to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
                published_at: "2019-12-23T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::SettlementParameter,
            },
        ],
    };

    assert_eq!(
        stage_adjustment(&evidence_db, &adjustment)
            .unwrap()
            .verification,
        OfficialVerification::Verified
    );

    let mut insecure_ine = adjustment;
    insecure_ine.symbol = "INE.sc2001".to_owned();
    for evidence in &mut insecure_ine.evidence {
        evidence.canonical_url = evidence.canonical_url.replace("cffex.com.cn", "ine.cn");
    }
    assert!(stage_adjustment(&evidence_db, &insecure_ine).is_err());
}

#[test]
fn official_adjustment_json_input_uses_the_same_isolated_staging_path() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let payload = r#"{
      "symbol": "INE.lu2604",
      "effective_at": "2026-03-10T00:00:00+08:00",
      "scope": "all listed LU contracts",
      "open_fee": null,
      "close_yesterday_fee": null,
      "close_today_fee": {
        "kind": "TurnoverRatePerTenThousand",
        "value": 0.3,
        "raw_text": "成交金额的万分之零点三"
      },
      "evidence": [
        {
          "canonical_url": "https://www.ine.cn/eng/circularnews/circular/202603/t20260306_830603.html",
          "mirror_url": "https://www.ine.cn/publicnotice/notice/202603/t20260306_830600.html",
          "sha256": "3ec81135a7f0f995de49c39a3178b173a29dbb1ed124b6328f26153498c310c3",
          "published_at": "2026-03-06T00:00:00+08:00",
          "kind": "notice"
        },
        {
          "canonical_url": "https://www.ine.cn/publicnotice/notice/202603/W020260306643830614686.doc",
          "mirror_url": null,
          "sha256": "aaf0ad447304c6f0af9543680ab1ae5da6b513e818f43b6f109b1be005004997",
          "published_at": "2026-03-06T00:00:00+08:00",
          "kind": "fee_schedule"
        }
      ]
    }"#;

    let staged = stage_adjustment_json(&evidence_db, payload).unwrap();

    assert_eq!(staged.verification, OfficialVerification::Verified);
}

#[test]
fn official_adjustment_batch_json_stages_each_concrete_contract() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let payload = r#"[
      {
        "symbol": "INE.sc2604",
        "effective_at": "2026-03-11T00:00:00+08:00",
        "scope": "all listed SC contracts",
        "open_fee": {"kind": "CnyPerLot", "value": 40.0, "raw_text": "40元/手"},
        "close_yesterday_fee": {"kind": "CnyPerLot", "value": 40.0, "raw_text": "40元/手"},
        "close_today_fee": {"kind": "CnyPerLot", "value": 240.0, "raw_text": "240元/手"},
        "evidence": [
          {
            "canonical_url": "https://www.ine.cn/eng/circularnews/circular/202603/t20260309_830636.html",
            "mirror_url": "https://www.ine.cn/publicnotice/notice/202603/t20260309_830634.html",
            "sha256": "f8e275edf08767dcc2ecc99239004c75680be62d63c729c91f2d1b358a84afa3",
            "published_at": "2026-03-09T00:00:00+08:00",
            "kind": "notice"
          },
          {
            "canonical_url": "https://www.ine.cn/publicnotice/notice/202603/W020260309662330567322.doc",
            "mirror_url": null,
            "sha256": "ff1594548d24076c99af4a290541ccda123aaa32b3c276821cdcf15f8dd87cfb",
            "published_at": "2026-03-09T00:00:00+08:00",
            "kind": "fee_schedule"
          }
        ]
      },
      {
        "symbol": "INE.lu2604",
        "effective_at": "2026-03-11T00:00:00+08:00",
        "scope": "all listed LU contracts",
        "open_fee": {"kind": "TurnoverRatePerTenThousand", "value": 1.0, "raw_text": "成交金额的万分之一"},
        "close_yesterday_fee": {"kind": "TurnoverRatePerTenThousand", "value": 1.0, "raw_text": "成交金额的万分之一"},
        "close_today_fee": {"kind": "TurnoverRatePerTenThousand", "value": 3.0, "raw_text": "成交金额的万分之三"},
        "evidence": [
          {
            "canonical_url": "https://www.ine.cn/eng/circularnews/circular/202603/t20260309_830636.html",
            "mirror_url": "https://www.ine.cn/publicnotice/notice/202603/t20260309_830634.html",
            "sha256": "f8e275edf08767dcc2ecc99239004c75680be62d63c729c91f2d1b358a84afa3",
            "published_at": "2026-03-09T00:00:00+08:00",
            "kind": "notice"
          },
          {
            "canonical_url": "https://www.ine.cn/publicnotice/notice/202603/W020260309662330567322.doc",
            "mirror_url": null,
            "sha256": "ff1594548d24076c99af4a290541ccda123aaa32b3c276821cdcf15f8dd87cfb",
            "published_at": "2026-03-09T00:00:00+08:00",
            "kind": "fee_schedule"
          }
        ]
      }
    ]"#;

    let staged = stage_adjustments_json(&evidence_db, payload).unwrap();

    assert_eq!(staged.adjustments, 2);
    assert_eq!(staged.verified, 2);
    let conn = rusqlite::Connection::open(&evidence_db).unwrap();
    let candidate_count: i64 = conn
        .query_row("select count(*) from official_fee_adjustments", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(candidate_count, 2);
}

#[test]
fn restaging_an_official_adjustment_replaces_its_evidence_links() {
    let dir = tempfile::tempdir().unwrap();
    let evidence_db = dir.path().join("official-evidence.sqlite");
    let mut adjustment = OfficialFeeAdjustment {
        symbol: "INE.sc2604".to_owned(),
        effective_at: "2026-03-10T00:00:00+08:00".to_owned(),
        scope: "all listed SC contracts".to_owned(),
        previous_fees: None,
        open_fee: None,
        close_yesterday_fee: None,
        close_today_fee: Some(future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(60.0),
            raw_text: Some("60元/手".to_owned()),
        }),
        evidence: vec![
            OfficialEvidence {
                canonical_url: "https://www.ine.cn/notice/first.html".to_owned(),
                mirror_url: None,
                sha256: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                    .to_owned(),
                published_at: "2026-03-06T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::Notice,
            },
            OfficialEvidence {
                canonical_url: "https://www.ine.cn/notice/first.doc".to_owned(),
                mirror_url: None,
                sha256: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_owned(),
                published_at: "2026-03-06T00:00:00+08:00".to_owned(),
                kind: EvidenceKind::FeeSchedule,
            },
        ],
    };
    stage_adjustment(&evidence_db, &adjustment).unwrap();

    adjustment.close_today_fee.as_mut().unwrap().value = Some(40.0);
    adjustment.evidence = vec![
        OfficialEvidence {
            canonical_url: "https://www.ine.cn/notice/reviewed.html".to_owned(),
            mirror_url: None,
            sha256: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc".to_owned(),
            published_at: "2026-03-07T00:00:00+08:00".to_owned(),
            kind: EvidenceKind::Notice,
        },
        OfficialEvidence {
            canonical_url: "https://www.ine.cn/notice/reviewed.doc".to_owned(),
            mirror_url: None,
            sha256: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd".to_owned(),
            published_at: "2026-03-07T00:00:00+08:00".to_owned(),
            kind: EvidenceKind::FeeSchedule,
        },
    ];
    stage_adjustment(&evidence_db, &adjustment).unwrap();

    let conn = rusqlite::Connection::open(&evidence_db).unwrap();
    let linked_urls = conn
        .prepare(
            "select evidence.canonical_url
             from official_adjustment_evidence link
             join official_evidence evidence on evidence.id = link.evidence_id
             order by evidence.canonical_url",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(
        linked_urls,
        vec![
            "https://www.ine.cn/notice/reviewed.doc".to_owned(),
            "https://www.ine.cn/notice/reviewed.html".to_owned(),
        ]
    );
}

#[test]
fn official_parameter_history_starts_no_earlier_than_contract_listing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = official_fixture_rows().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-19 22:56:54".to_owned());

    replace_with_official_parameter_history(
        &mut conn,
        &[OfficialHistoryRow {
            row,
            coverage_end_exclusive: "2026-03-21T00:00:00+08:00".to_owned(),
            evidence_level: OfficialEvidenceLevel::OfficialParameter,
            evidence: vec![OfficialEvidenceReference {
                canonical_url: "https://www.shfe.com.cn/parameter".to_owned(),
                body_sha256: "a".repeat(64),
            }],
        }],
        "2026-03-20T12:00:00+08:00",
    )
    .unwrap();

    let valid_from: String = conn
        .query_row("select valid_from from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();

    assert_eq!(valid_from, "2026-03-20T00:00:00+08:00");
}

#[test]
fn schema_initialization_does_not_clamp_existing_fee_version_to_contract_listing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = official_fixture_rows().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-20 22:56:54".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2026-03-20T23:00:00+08:00").unwrap();
    conn.execute(
        "update fee_versions set valid_from = '2026-03-19T00:00:00+08:00'",
        [],
    )
    .unwrap();

    ensure_schema(&conn).unwrap();

    let valid_from: String = conn
        .query_row("select valid_from from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();
    assert_eq!(valid_from, "2026-03-19T00:00:00+08:00");
}

#[test]
fn export_rejects_fee_version_before_contract_listing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let out = dir.path().join("public");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = official_fixture_rows().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-20 22:56:54".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2026-03-20T23:00:00+08:00").unwrap();
    conn.execute(
        "update fee_versions set valid_from = '2026-03-19T00:00:00+08:00'",
        [],
    )
    .unwrap();
    attach_fixture_fee_evidence(&conn);
    drop(conn);

    let error = export_archive(&db_path, &out).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("fee versions before contract listing")
    );
}

#[test]
fn schema_initialization_does_not_remove_prelisting_fee_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = official_fixture_rows().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-20 22:56:54".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2026-03-20T23:00:00+08:00").unwrap();
    conn.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         ) select contract_id, 'prelisting-rule', buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, 'official', source_updated_at,
           '2026-03-19T00:00:00+08:00', '2026-03-20T00:00:00+08:00',
           first_seen_at, last_seen_at
         from fee_versions",
        [],
    )
    .unwrap();
    let contract_id: i64 = conn
        .query_row("select id from contracts", [], |record| record.get(0))
        .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values (?1, '2026-03-19T00:00:00+08:00', 'prelisting-rule',
           'official_parameter', 'https://www.shfe.com.cn/prelisting.html',
           ?2, '2026-03-20T23:00:00+08:00')",
        rusqlite::params![contract_id, "a".repeat(64)],
    )
    .unwrap();

    ensure_schema(&conn).unwrap();

    let versions: i64 = conn
        .query_row("select count(*) from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();
    let evidence: i64 = conn
        .query_row("select count(*) from fee_version_evidence", [], |record| {
            record.get(0)
        })
        .unwrap();
    assert_eq!(versions, 2);
    assert_eq!(evidence, 1);
}

#[test]
fn schema_initialization_does_not_create_missing_specification_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    insert_official_fixture_rows(
        &mut conn,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    conn.execute("delete from contract_spec_versions", [])
        .unwrap();

    ensure_schema(&conn).unwrap();

    let specifications: i64 = conn
        .query_row(
            "select count(*) from contract_spec_versions",
            [],
            |record| record.get(0),
        )
        .unwrap();
    assert_eq!(specifications, 0);
}

#[test]
fn schema_accepts_official_fee_version_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let seeded = official_fixture_rows();
    insert_official_fixture_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    conn.execute("update fee_versions set source_kind = 'official'", [])
        .unwrap();

    let source: String = conn
        .query_row("select source_kind from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(source, "official");
}

#[test]
fn verified_official_fee_tuple_creates_forward_official_version() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let seeded = official_fixture_rows();
    insert_official_fixture_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();
    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.2),
        raw_text: Some("0.2元/手".to_owned()),
    };

    apply_official_fee_tuple(
        &mut conn,
        "SHFE.cu2607",
        "2026-06-06T00:00:00+08:00",
        &[fee.clone(), fee.clone(), fee],
        "2026-06-05T23:00:00+08:00",
    )
    .unwrap();

    let (versions, source, open_fee): (i64, String, String) = conn
        .query_row(
            "select count(*), max(source_kind), max(open_fee_json) from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(versions, 2);
    assert_eq!(source, "official");
    assert!(open_fee.contains("0.2"));
}

#[test]
fn verified_official_transition_repairs_a_premature_single_baseline_rule() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut premature = official_fixture_rows().remove(0);
    premature.symbol = "CZCE.PL701".to_owned();
    let target = [
        future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(3.0),
            raw_text: Some("3元/手".to_owned()),
        },
        future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(3.0),
            raw_text: Some("3元/手".to_owned()),
        },
        future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::Zero,
            value: Some(0.0),
            raw_text: Some("0元/手".to_owned()),
        },
    ];
    premature.open_fee = target[0].clone();
    premature.close_yesterday_fee = target[1].clone();
    premature.close_today_fee = target[2].clone();
    premature.source_updated_at = Some("2026-06-05T00:00:00+08:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[premature], "2026-06-05T00:00:00+08:00").unwrap();
    let previous_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(1.0),
        raw_text: Some("1/万分之".to_owned()),
    };

    apply_official_fee_transition(
        &mut conn,
        "CZCE.PL701",
        "2026-06-08T00:00:00+08:00",
        &[previous_fee.clone(), previous_fee.clone(), previous_fee],
        &target,
        "2026-08-23T12:00:00+08:00",
    )
    .unwrap();

    let versions = conn
        .prepare(
            "select valid_from, valid_to, open_fee_json, source_kind
             from fee_versions order by valid_from",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();

    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].0, "2026-06-05T00:00:00+08:00");
    assert_eq!(versions[0].1.as_deref(), Some("2026-06-08T00:00:00+08:00"));
    assert!(versions[0].2.contains("TurnoverRatePerTenThousand"));
    assert_eq!(versions[1].0, "2026-06-08T00:00:00+08:00");
    assert!(versions[1].2.contains("CnyPerLot"));
    assert_eq!(versions[1].3, "official");
}

#[test]
fn official_listing_evidence_can_retime_and_fill_a_missing_baseline_contract() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(1.0),
        raw_text: Some("成交金额的万分之一".to_owned()),
    };
    let close_today = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(2.5),
        raw_text: Some("成交金额的万分之二点五".to_owned()),
    };
    let seed = AllowedRow {
        symbol: "GFEX.ps2707".to_owned(),
        listing_date: None,
        expiry_date: None,
        trading_status: future_meta::model::TradingStatus::Unknown,
        buy_margin_rate: None,
        sell_margin_rate: None,
        open_fee: fee.clone(),
        close_yesterday_fee: fee.clone(),
        close_today_fee: close_today.clone(),
        intraday_open_fee: None,
        lot_size: 3.0,
        tick_size: 5.0,
        source_updated_at: Some("2026-08-07T00:00:00+08:00".to_owned()),
        is_main_contract: false,
    };
    let metadata_source = AllowedRow {
        symbol: "GFEX.lc2708".to_owned(),
        lot_size: 1.0,
        tick_size: 50.0,
        ..seed.clone()
    };
    insert_official_fixture_rows(
        &mut conn,
        &[seed, metadata_source],
        "2026-08-18T00:00:00+08:00",
    )
    .unwrap();

    apply_official_listed_contract_fee_tuple(
        &mut conn,
        "GFEX.ps2707",
        "2026-07-15T00:00:00+08:00",
        &[fee.clone(), fee.clone(), close_today.clone()],
        "2026-08-23T00:00:00+08:00",
    )
    .unwrap();
    apply_official_listed_contract_fee_tuple(
        &mut conn,
        "GFEX.lc2707",
        "2026-07-15T00:00:00+08:00",
        &[
            future_meta::model::FeeSpec {
                kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
                value: Some(1.6),
                raw_text: Some("成交金额的万分之一点六".to_owned()),
            },
            future_meta::model::FeeSpec {
                kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
                value: Some(1.6),
                raw_text: Some("成交金额的万分之一点六".to_owned()),
            },
            future_meta::model::FeeSpec {
                kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
                value: Some(3.2),
                raw_text: Some("成交金额的万分之三点二".to_owned()),
            },
        ],
        "2026-08-23T00:00:00+08:00",
    )
    .unwrap();

    let ps: (String, String) = conn
        .query_row(
            "select valid_from, source_kind from fee_versions v join contracts c on c.id = v.contract_id where c.symbol = 'GFEX.ps2707'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(ps.0, "2026-07-15T00:00:00+08:00");
    assert_eq!(ps.1, "official");
    let lc: (f64, f64, String, String) = conn
        .query_row(
            "select c.lot_size, c.tick_size, v.valid_from, v.source_kind from contracts c join fee_versions v on v.contract_id = c.id where c.symbol = 'GFEX.lc2707'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        lc,
        (
            1.0,
            50.0,
            "2026-07-15T00:00:00+08:00".to_owned(),
            "official".to_owned()
        )
    );
}

#[test]
fn readonly_history_connection_rejects_collection_writes() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    connection
        .execute(
            "insert into baseline_state(
                baseline_version, source_sha256, row_count, contract_count, imported_at
             ) values('v11', ?1, 1, 1, '2026-06-04T12:00:00+08:00')",
            ["a".repeat(64)],
        )
        .unwrap();
    drop(connection);

    let readonly = connect_readonly(&db_path).unwrap();
    let error = readonly
        .execute(
            "insert into source_state(source_url) values('https://example.invalid/latest')",
            [],
        )
        .unwrap_err();

    assert!(error.to_string().contains("readonly"));
}

#[test]
fn connect_enables_foreign_keys() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();

    let enabled: i64 = conn
        .query_row("pragma foreign_keys", [], |row| row.get(0))
        .unwrap();

    assert_eq!(enabled, 1);
}

#[test]
fn known_official_spec_change_repairs_all_listed_contract_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "DCE.p2605".to_owned();
    row.listing_date = Some("20250519".to_owned());
    row.expiry_date = Some("20260525".to_owned());
    row.lot_size = 10.0;
    row.tick_size = 2.0;
    row.source_updated_at = Some("2025-05-19 00:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2025-05-19T01:00:00+08:00").unwrap();

    let changed = migrate_known_contract_spec_history(&mut conn, "2026-08-23T12:00:00Z").unwrap();

    assert_eq!(changed, 1);
    let specs = conn
        .prepare(
            "select tick_size, valid_from, valid_to, source_kind
             from contract_spec_versions order by valid_from",
        )
        .unwrap()
        .query_map([], |record| {
            Ok((
                record.get::<_, f64>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, Option<String>>(2)?,
                record.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].0.to_bits(), 2.0_f64.to_bits());
    assert_eq!(specs[0].2.as_deref(), Some("2026-04-10T00:00:00+08:00"));
    assert_eq!(specs[1].0.to_bits(), 1.0_f64.to_bits());
    assert_eq!(specs[1].1, "2026-04-10T00:00:00+08:00");
    assert_eq!(specs[1].2, None);
    assert_eq!(specs[1].3, "official");
}

#[test]
fn official_spec_migration_repairs_expired_contract_when_expiry_date_is_missing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = official_fixture_rows().remove(0);
    row.symbol = "DCE.p2405".to_owned();
    row.listing_date = Some("20230519".to_owned());
    row.expiry_date = None;
    row.lot_size = 10.0;
    row.tick_size = 2.0;
    row.source_updated_at = Some("2023-05-19 00:00:00".to_owned());
    insert_official_fixture_rows(&mut conn, &[row], "2023-05-19T01:00:00+08:00").unwrap();

    conn.execute_batch(
        "delete from contract_spec_versions
         where contract_id = (select id from contracts where symbol = 'DCE.p2405');
         insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (
           (select id from contracts where symbol = 'DCE.p2405'),
           10, 2, '2023-05-19T00:00:00+08:00', '2026-04-10T00:00:00+08:00',
           'official', 'http://www.dce.com.cn/dce/content/2026/ywggytz/18628268.html',
           '2026-08-23T12:00:00Z', '2026-08-23T12:00:00Z'
         );
         insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (
           (select id from contracts where symbol = 'DCE.p2405'),
           10, 1, '2026-04-10T00:00:00+08:00', null,
           'official', 'http://www.dce.com.cn/dce/content/2026/ywggytz/18628268.html',
           '2026-08-23T12:00:00Z', '2026-08-23T12:00:00Z'
         );
         update contracts set tick_size = 1 where symbol = 'DCE.p2405';",
    )
    .unwrap();

    let changed = migrate_known_contract_spec_history(&mut conn, "2026-08-23T12:00:00Z").unwrap();

    assert_eq!(changed, 1);
    let repeated = migrate_known_contract_spec_history(&mut conn, "2026-08-23T12:01:00Z").unwrap();
    assert_eq!(repeated, 0);
    let (tick_size, versions, version_tick, valid_to, source_kind): (
        f64,
        i64,
        f64,
        Option<String>,
        String,
    ) = conn
        .query_row(
            "select c.tick_size, count(s.id), min(s.tick_size), max(s.valid_to), max(s.source_kind)
             from contracts c join contract_spec_versions s on s.contract_id = c.id
             where c.symbol = 'DCE.p2405'",
            [],
            |record| {
                Ok((
                    record.get(0)?,
                    record.get(1)?,
                    record.get(2)?,
                    record.get(3)?,
                    record.get(4)?,
                ))
            },
        )
        .unwrap();
    assert_eq!(tick_size.to_bits(), 2.0_f64.to_bits());
    assert_eq!(versions, 1);
    assert_eq!(version_tick.to_bits(), 2.0_f64.to_bits());
    assert_eq!(valid_to, None);
    assert_eq!(source_kind, "v11_baseline");
}

#[test]
fn schema_enforces_core_constraints() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let bad_contract = conn.execute(
        "insert into contracts(
          symbol, lot_size, tick_size, first_seen_at, last_seen_at, active
        ) values ('SHFE.bad2607', 0, 10, '2026-06-04T12:00:00+08:00', '2026-06-04T12:00:00+08:00', 1)",
        [],
    );

    assert!(bad_contract.is_err());
}

#[test]
fn schema_removes_legacy_redundant_fee_versions_index() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    conn.execute_batch(
        "create index if not exists idx_fee_versions_contract
           on fee_versions(contract_id, valid_from);",
    )
    .unwrap();

    ensure_schema(&conn).unwrap();

    let redundant_index_exists: bool = conn
        .query_row(
            "select exists(
               select 1 from sqlite_master
               where type = 'index' and name = 'idx_fee_versions_contract'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let unique_index_exists: bool = conn
        .query_row(
            "select exists(
               select 1 from sqlite_master
               where type = 'index' and name = 'idx_fee_versions_contract_valid_from'
             )",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert!(!redundant_index_exists);
    assert!(unique_index_exists);
}

#[test]
fn export_rejects_orphan_fee_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let out = directory.path().join("public");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    attach_fixture_fee_evidence(&connection);
    let contract_id: i64 = connection
        .query_row(
            "select id from contracts where symbol = 'SHFE.cu2607'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "insert into fee_version_evidence(
                contract_id, valid_from, rule_hash, evidence_level,
                canonical_url, body_sha256, recorded_at
             ) values(?1, '2026-06-05T00:00:00+08:00', 'missing-rule',
                      'paired_official', 'https://www.shfe.com.cn/evidence', ?2,
                      '2026-06-06T00:00:00+08:00')",
            rusqlite::params![contract_id, "a".repeat(64)],
        )
        .unwrap();
    drop(connection);

    let error = export_archive(&db_path, &out).unwrap_err();

    assert!(error.to_string().contains("orphan fee-version evidence"));
}

#[test]
fn export_rejects_reverse_fee_observation_times() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let out = directory.path().join("public");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    attach_fixture_fee_evidence(&connection);
    connection
        .execute(
            "update fee_versions
             set first_seen_at = '2026-06-05T00:00:00+08:00',
                 last_seen_at = '2026-06-04T00:00:00+08:00'",
            [],
        )
        .unwrap();
    drop(connection);

    let error = export_archive(&db_path, &out).unwrap_err();

    assert!(error.to_string().contains("reverse fee observation times"));
}

#[test]
fn exports_archive_loadable_by_client() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let out = dir.path().join("public");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    insert_official_fixture_rows(
        &mut conn,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    attach_fixture_fee_evidence(&conn);
    let fee_effective_from: String = conn
        .query_row("select max(valid_from) from fee_versions", [], |row| {
            row.get(0)
        })
        .unwrap();
    drop(conn);

    export_archive(&db_path, &out).unwrap();
    let manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["artifact"], "latest.fmeta.zst");
    assert_eq!(
        manifest["mirrors"],
        serde_json::json!(["https://zynthium.github.io/future-meta/latest.fmeta.zst"])
    );
    assert_eq!(manifest["fee_effective_from"], fee_effective_from);
    assert_eq!(manifest["history_end"], fee_effective_from);

    let bytes = std::fs::read(out.join("latest.fmeta.zst")).unwrap();
    let archive = future_meta::archive::decode_archive_bytes(&bytes).unwrap();
    assert_eq!(archive.history_end, fee_effective_from);
    assert_ne!(archive.generated_at, archive.history_end);
    let meta = FutureMeta::from_archive(archive).unwrap();

    assert!(
        meta.contract_fee_asof("SHFE.cu2607", "2026-06-04T12:00:00+08:00")
            .is_ok()
    );
}

#[test]
fn export_refuses_untrusted_jin10_fee_versions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let out = dir.path().join("public");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    insert_official_fixture_rows(
        &mut conn,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    attach_fixture_fee_evidence(&conn);
    conn.execute("update fee_versions set source_kind = 'jin10'", [])
        .unwrap();
    drop(conn);

    let err = export_archive(&db_path, &out).unwrap_err();

    assert!(
        err.to_string()
            .contains("refusing to export non-official fee versions")
    );
}

#[test]
fn export_refuses_every_non_official_fee_source() {
    for source_kind in ["9qihuo", "jin10", "v11_baseline"] {
        let directory = tempfile::tempdir().unwrap();
        let db_path = directory.path().join("future-meta.sqlite");
        let out = directory.path().join("public");
        let mut connection = connect(&db_path).unwrap();
        ensure_schema(&connection).unwrap();
        insert_official_fixture_rows(
            &mut connection,
            &official_fixture_rows(),
            "2026-06-04T12:00:00+08:00",
        )
        .unwrap();
        connection
            .execute("update fee_versions set source_kind = ?1", [source_kind])
            .unwrap();
        drop(connection);

        let error = export_archive(&db_path, &out).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("refusing to export non-official fee versions"),
            "{source_kind}: {error:#}"
        );
    }
}

#[test]
fn export_rejects_official_fee_versions_without_retained_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let out = directory.path().join("public");
    let mut connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    drop(connection);

    let error = export_archive(&db_path, &out).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("fee versions missing official evidence")
    );
}

#[test]
fn integrity_rejects_malformed_fee_timestamps() {
    for (column, expected_field) in [
        ("valid_from", "valid_from"),
        ("valid_to", "valid_to"),
        ("first_seen_at", "first_seen_at"),
        ("last_seen_at", "last_seen_at"),
    ] {
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        ensure_schema(&connection).unwrap();
        let mut row = official_fixture_rows().remove(0);
        row.listing_date = None;
        insert_official_fixture_rows(&mut connection, &[row], "2026-06-04T12:00:00+08:00").unwrap();
        attach_fixture_fee_evidence(&connection);
        connection
            .execute(
                &format!("update fee_versions set {column} = 'not-rfc3339'"),
                [],
            )
            .unwrap();
        if column == "valid_from" {
            connection
                .execute(
                    "update fee_version_evidence set valid_from = 'not-rfc3339'",
                    [],
                )
                .unwrap();
        }

        let error = validate_fee_history_integrity(&connection).unwrap_err();

        assert!(
            error
                .to_string()
                .contains(&format!("invalid fee {expected_field} timestamp")),
            "{column}: {error:#}"
        );
    }
}

#[test]
fn integrity_rejects_open_fee_interval_before_a_later_version() {
    let mut connection = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&connection).unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    insert_official_fixture_rows(
        &mut connection,
        &changed_fixture_rows(),
        "2026-06-04T13:00:00+08:00",
    )
    .unwrap();
    attach_fixture_fee_evidence(&connection);
    connection
        .execute_batch(
            "drop index idx_fee_versions_open_contract;
             update fee_versions set valid_to = null where valid_to is not null;",
        )
        .unwrap();

    let error = validate_fee_history_integrity(&connection).unwrap_err();

    assert!(
        error
            .to_string()
            .contains("open fee intervals before later versions")
    );
}

#[test]
fn product_static_metadata_candidates_retain_verified_tick_size_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut old_tick = official_fixture_rows().remove(0);
    old_tick.symbol = "INE.ec2604".to_owned();
    old_tick.lot_size = 50.0;
    old_tick.tick_size = 0.1;
    let mut new_tick = old_tick.clone();
    new_tick.symbol = "INE.ec2605".to_owned();
    new_tick.tick_size = 0.5;
    insert_official_fixture_rows(
        &mut conn,
        &[old_tick, new_tick],
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();

    assert_eq!(
        metadata.get("INE.ec"),
        Some(&vec![
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 0.1,
            },
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 0.5,
            },
        ])
    );
}

#[test]
fn product_static_metadata_candidates_include_official_pre_change_oil_ticks() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut palm_oil = official_fixture_rows().remove(0);
    palm_oil.symbol = "DCE.p2605".to_owned();
    palm_oil.lot_size = 10.0;
    palm_oil.tick_size = 1.0;
    let mut soybean_oil = palm_oil.clone();
    soybean_oil.symbol = "DCE.y2605".to_owned();
    let mut lithium_carbonate = palm_oil.clone();
    lithium_carbonate.symbol = "GFEX.lc2605".to_owned();
    lithium_carbonate.lot_size = 1.0;
    lithium_carbonate.tick_size = 20.0;
    insert_official_fixture_rows(
        &mut conn,
        &[palm_oil, soybean_oil, lithium_carbonate],
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();

    assert_eq!(
        metadata.get("DCE.p"),
        Some(&vec![
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ])
    );
    assert_eq!(
        metadata.get("DCE.y"),
        Some(&vec![
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ])
    );
    assert_eq!(
        metadata.get("GFEX.lc"),
        Some(&vec![
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 1.0,
                tick_size: 20.0,
            },
            future_meta_daemon::model::ContractStaticMetadata {
                lot_size: 1.0,
                tick_size: 50.0,
            },
        ])
    );
}

#[test]
fn product_static_metadata_candidates_include_verified_delisted_strong_wheat() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();

    assert_eq!(
        metadata.get("CZCE.WH"),
        Some(&vec![future_meta_daemon::model::ContractStaticMetadata {
            lot_size: 20.0,
            tick_size: 1.0,
        }])
    );
}

#[test]
fn product_static_metadata_candidates_include_official_legacy_czce_contract_specs() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();
    for (product, lot_size, tick_size) in [
        ("CZCE.JR", 20.0, 1.0),
        ("CZCE.LR", 20.0, 1.0),
        ("CZCE.PM", 50.0, 1.0),
        ("CZCE.RI", 20.0, 1.0),
        ("CZCE.ZC", 100.0, 0.2),
    ] {
        assert_eq!(
            metadata.get(product),
            Some(&vec![future_meta_daemon::model::ContractStaticMetadata {
                lot_size,
                tick_size,
            }]),
            "{product}"
        );
    }
}

#[test]
fn coverage_audit_reports_unknown_lifecycle_and_missing_histories() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    conn.execute(
        "insert into contracts(
           symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         ) values (
           'SHFE.cu2001', null, null, 5.0, 10.0,
           '2019-01-01T00:00:00+08:00', '2020-01-15T00:00:00+08:00', 0
         )",
        [],
    )
    .unwrap();

    let report = audit_history_coverage(
        &conn,
        CoverageBoundary {
            from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
            through: Date::from_calendar_date(2020, Month::December, 31).unwrap(),
        },
    )
    .unwrap();
    let kinds = report
        .findings
        .iter()
        .map(|finding| finding.kind)
        .collect::<Vec<_>>();

    assert_eq!(report.contracts, 1);
    assert_eq!(report.complete_contracts, 0);
    assert!(kinds.contains(&CoverageFindingKind::MissingListingDate));
    assert!(kinds.contains(&CoverageFindingKind::MissingExpiryDate));
    assert!(kinds.contains(&CoverageFindingKind::MissingFeeHistory));
    assert!(kinds.contains(&CoverageFindingKind::MissingSpecificationHistory));
}

fn insert_complete_coverage_contract(conn: &rusqlite::Connection) {
    conn.execute(
        "insert into contracts(
           id, symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         ) values (
           1, 'SHFE.cu2001', '20200102', '20200131', 5.0, 10.0,
           '2020-01-02T00:00:00+08:00', '2020-01-31T00:00:00+08:00', 0
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "insert into fee_version_evidence(
           contract_id, valid_from, rule_hash, evidence_level,
           canonical_url, body_sha256, recorded_at
         ) values(
           1, '2020-01-02T00:00:00+08:00', 'official-rule', 'official_parameter',
           'https://www.shfe.com.cn/fees/cu.html', ?1, '2020-01-02T00:00:00+08:00'
         )",
        ["1".repeat(64)],
    )
    .unwrap();
    let fee = r#"{"kind":"CnyPerLot","value":5.0,"raw_text":"5元/手"}"#;
    conn.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         ) values (
           1, 'official-rule', null, null, ?1, ?1, ?1,
           'Trading', 0, 'official', null,
           '2020-01-02T00:00:00+08:00', null,
           '2020-01-02T00:00:00+08:00', '2020-01-31T00:00:00+08:00'
         )",
        [fee],
    )
    .unwrap();
    conn.execute(
        "insert into contract_spec_versions(
           contract_id, lot_size, tick_size, valid_from, valid_to,
           source_kind, source_url, first_seen_at, last_seen_at
         ) values (
           1, 5.0, 10.0, '2020-01-02T00:00:00+08:00', null,
           'official', 'https://www.shfe.com.cn/rules/cu.html',
           '2020-01-02T00:00:00+08:00', '2020-01-31T00:00:00+08:00'
         )",
        [],
    )
    .unwrap();
    conn.execute(
        "insert into contract_spec_evidence(
           contract_id, valid_from, canonical_url, body_sha256, recorded_at
         ) values(
           1, '2020-01-02T00:00:00+08:00',
           'https://www.shfe.com.cn/rules/cu.html', ?1,
           '2020-01-02T00:00:00+08:00'
         )",
        ["2".repeat(64)],
    )
    .unwrap();
    conn.execute(
        "insert into contract_lifecycle_evidence(
           contract_id, listing_date, expiry_date, canonical_url,
           body_sha256, recorded_at
         ) values(
           1, '20200102', '20200131',
           'https://www.shfe.com.cn/calendar/cu.html', ?1,
           '2020-01-02T00:00:00+08:00'
         )",
        ["3".repeat(64)],
    )
    .unwrap();
}

fn january_2020_coverage() -> CoverageBoundary {
    CoverageBoundary {
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        through: Date::from_calendar_date(2020, Month::January, 31).unwrap(),
    }
}

#[test]
fn coverage_audit_accepts_complete_official_interval_chains() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert_eq!(report.contracts, 1);
    assert_eq!(report.complete_contracts, 1);
    assert_eq!(report.findings.len(), 0, "{:?}", report.findings);
}

#[test]
fn coverage_audit_rejects_official_rows_without_retained_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute_batch(
        "delete from fee_version_evidence;
         delete from contract_spec_evidence;
         delete from contract_lifecycle_evidence;",
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();
    let kinds = report
        .findings
        .iter()
        .map(|finding| finding.kind)
        .collect::<Vec<_>>();

    assert!(kinds.contains(&CoverageFindingKind::MissingFeeEvidence));
    assert!(kinds.contains(&CoverageFindingKind::MissingSpecificationEvidence));
    assert!(kinds.contains(&CoverageFindingKind::MissingLifecycleEvidence));
}

#[test]
fn coverage_audit_treats_legacy_schema_as_missing_evidence() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute_batch(
        "drop table fee_version_evidence;
         drop table contract_spec_evidence;
         drop table contract_lifecycle_evidence;",
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();
    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::MissingFeeEvidence })
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::MissingSpecificationEvidence })
    );
    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::MissingLifecycleEvidence })
    );
}

#[test]
fn coverage_audit_rejects_fee_chain_starting_after_listing() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute(
        "update fee_versions set valid_from = '2020-01-03T00:00:00+08:00'",
        [],
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::FeeCoverageGap })
    );
}

#[test]
fn coverage_audit_rejects_non_official_specification_source() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute(
        "update contract_spec_versions set source_kind = 'v11_baseline'",
        [],
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::NonOfficialSpecificationSource })
    );
}

#[test]
fn coverage_audit_rejects_non_official_fee_source() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute("update fee_versions set source_kind = 'v11_baseline'", [])
        .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.kind == CoverageFindingKind::NonOfficialFeeSource)
    );
}

#[test]
fn coverage_audit_rejects_specification_chain_starting_after_listing() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute(
        "update contract_spec_versions set valid_from = '2020-01-03T00:00:00+08:00'",
        [],
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::SpecificationCoverageGap })
    );
}

#[test]
fn coverage_audit_rejects_overlapping_fee_intervals() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute(
        "update fee_versions set valid_to = '2020-01-20T00:00:00+08:00'",
        [],
    )
    .unwrap();
    let fee = r#"{"kind":"CnyPerLot","value":6.0,"raw_text":"6元/手"}"#;
    conn.execute(
        "insert into fee_versions(
           contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
           open_fee_json, close_yesterday_fee_json, close_today_fee_json,
           trading_status, is_main_contract, source_kind, source_updated_at,
           valid_from, valid_to, first_seen_at, last_seen_at
         ) values (
           1, 'overlap', null, null, ?1, ?1, ?1,
           'Trading', 0, 'official', null,
           '2020-01-10T00:00:00+08:00', null,
           '2020-01-10T00:00:00+08:00', '2020-01-31T00:00:00+08:00'
         )",
        [fee],
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| finding.kind == CoverageFindingKind::FeeIntervalOverlap)
    );
}

#[test]
fn coverage_audit_rejects_invalid_contract_specification_value() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    conn.execute_batch(
        "pragma ignore_check_constraints = on;
         update contract_spec_versions set tick_size = 0.0;
         pragma ignore_check_constraints = off;",
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::InvalidSpecificationValue })
    );
}

#[test]
fn coverage_audit_rejects_unknown_fee_value() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    ensure_schema(&conn).unwrap();
    insert_complete_coverage_contract(&conn);
    let unknown = r#"{"kind":"Unknown","value":null,"raw_text":"unknown"}"#;
    conn.execute(
        "update fee_versions set close_today_fee_json = ?1",
        [unknown],
    )
    .unwrap();

    let report = audit_history_coverage(&conn, january_2020_coverage()).unwrap();

    assert!(
        report
            .findings
            .iter()
            .any(|finding| { finding.kind == CoverageFindingKind::InvalidFeeValue })
    );
}

#[test]
fn coverage_report_writes_json_before_strict_failure() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("history.sqlite");
    let report_path = dir.path().join("coverage.json");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    conn.execute(
        "insert into contracts(
           symbol, listing_date, expiry_date, lot_size, tick_size,
           first_seen_at, last_seen_at, active
         ) values (
           'SHFE.cu2001', null, null, 5.0, 10.0,
           '2020-01-01T00:00:00+08:00', '2020-01-31T00:00:00+08:00', 0
         )",
        [],
    )
    .unwrap();
    drop(conn);

    let error =
        audit_history_coverage_to_path(&db_path, january_2020_coverage(), &report_path, true)
            .unwrap_err();
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();

    assert!(error.to_string().contains("strict coverage failed"));
    assert_eq!(json["boundary"]["from"], "2020-01-01");
    assert_eq!(json["contracts"], 1);
    assert_eq!(json["complete_contracts"], 0);
    assert_eq!(json["findings"][0]["symbol"], "SHFE.cu2001");
    assert_eq!(json["findings"][0]["kind"], "missing_listing_date");
}

#[test]
fn official_metadata_import_discovers_unseeded_contract() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let connection = connect(&db_path).unwrap();
    ensure_schema(&connection).unwrap();
    drop(connection);

    let evidence = b"official DCE contract metadata";
    let sha256 = hex::encode(Sha256::digest(evidence));
    std::fs::write(directory.path().join(&sha256), evidence).unwrap();
    let manifest = directory.path().join("metadata.tsv");
    std::fs::write(
        &manifest,
        format!(
            "symbol\tlisting_date\texpiry_date\tvalid_from\tvalid_to\tlot_size\ttick_size\tlifecycle_url\tlifecycle_sha256\tspecification_url\tspecification_sha256\nDCE.a2609\t2025-09-15\t2026-09-14\t2025-09-15T00:00:00+08:00\t\t10\t1\thttp://www.dce.com.cn/dcereport/publicweb/tradepara/contractInfo\t{sha256}\thttp://www.dce.com.cn/dcereport/publicweb/tradepara/contractInfo\t{sha256}\n"
        ),
    )
    .unwrap();

    let result = import_contract_metadata(&OfficialMetadataImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        observed_at: "2026-08-25T00:00:00Z".to_owned(),
    })
    .unwrap();
    assert_eq!(result.contracts, 1);
    let connection = connect(&db_path).unwrap();
    let row: (String, String, f64, f64) = connection
        .query_row(
            "select listing_date, expiry_date, lot_size, tick_size from contracts where symbol = 'DCE.a2609'",
            [],
            |record| Ok((record.get(0)?, record.get(1)?, record.get(2)?, record.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        row,
        ("20250915".to_owned(), "20260914".to_owned(), 10.0, 1.0)
    );
}

fn official_fixture_rows() -> Vec<AllowedRow> {
    use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
    let fee = FeeSpec {
        kind: FeeKind::CnyPerLot,
        value: Some(0.1),
        raw_text: Some("0.1元".to_owned()),
    };
    vec![AllowedRow {
        symbol: "SHFE.cu2607".to_owned(),
        listing_date: Some("20250716".to_owned()),
        expiry_date: Some("20260715".to_owned()),
        trading_status: TradingStatus::Trading,
        buy_margin_rate: Some(0.12),
        sell_margin_rate: Some(0.12),
        open_fee: fee.clone(),
        close_yesterday_fee: fee.clone(),
        close_today_fee: fee,
        intraday_open_fee: None,
        lot_size: 5.0,
        tick_size: 10.0,
        source_updated_at: Some("2026-03-27 22:56:54".to_owned()),
        is_main_contract: true,
    }]
}
fn changed_fixture_rows() -> Vec<AllowedRow> {
    let mut rows = source_updated_fixture_rows();
    rows[0].open_fee.value = Some(0.2);
    rows[0].open_fee.raw_text = Some("0.2元".to_owned());
    rows
}
fn source_updated_fixture_rows() -> Vec<AllowedRow> {
    let mut rows = official_fixture_rows();
    rows[0].source_updated_at = Some("2026-03-28 22:56:54".to_owned());
    rows
}

// Explicit official setup only. Tests select evidence and legacy corruption
// separately, rather than exercising a retired display-source ingest path.
fn insert_official_fixture_rows(
    conn: &mut rusqlite::Connection,
    rows: &[AllowedRow],
    observed: &str,
) -> anyhow::Result<()> {
    use rusqlite::params;
    for row in rows {
        let source = row.source_updated_at.as_deref().unwrap_or(observed);
        let day = &source[..10];
        let listing = row.listing_date.as_deref().map(|day| {
            if day.len() == 8 {
                format!("{}-{}-{}", &day[..4], &day[4..6], &day[6..])
            } else {
                day.to_owned()
            }
        });
        let from = format!(
            "{}T00:00:00+08:00",
            listing.as_deref().map_or(day, |listing| day.max(listing))
        );
        conn.execute("insert into contracts(symbol,listing_date,expiry_date,lot_size,tick_size,first_seen_at,last_seen_at,active) values(?1,?2,?3,?4,?5,?6,?6,0) on conflict(symbol) do update set listing_date=excluded.listing_date,expiry_date=excluded.expiry_date,lot_size=excluded.lot_size,tick_size=excluded.tick_size,last_seen_at=excluded.last_seen_at",params![row.symbol,row.listing_date,row.expiry_date,row.lot_size,row.tick_size,observed])?;
        let contract: i64 = conn.query_row(
            "select id from contracts where symbol=?1",
            [&row.symbol],
            |r| r.get(0),
        )?;
        conn.execute("insert or ignore into contract_spec_versions(contract_id,lot_size,tick_size,valid_from,valid_to,source_kind,source_url,first_seen_at,last_seen_at) values(?1,?2,?3,?4,null,'official','https://www.shfe.com.cn/specification',?5,?5)",params![contract,row.lot_size,row.tick_size,from,observed])?;
        conn.execute("update fee_versions set valid_to=?1 where contract_id=?2 and valid_to is null and julianday(valid_from)<julianday(?1)",params![from,contract])?;
        let status = match row.trading_status {
            future_meta::model::TradingStatus::Trading => "Trading",
            future_meta::model::TradingStatus::NotTrading => "NotTrading",
            future_meta::model::TradingStatus::Unknown => "Unknown",
        };
        conn.execute("insert into fee_versions(contract_id,rule_hash,buy_margin_rate,sell_margin_rate,open_fee_json,close_yesterday_fee_json,close_today_fee_json,intraday_open_fee_json,trading_status,is_main_contract,source_kind,source_updated_at,valid_from,valid_to,first_seen_at,last_seen_at) values(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,'official',?11,?12,null,?13,?13)",params![contract,row_rule_hash(row),row.buy_margin_rate,row.sell_margin_rate,serde_json::to_string(&row.open_fee)?,serde_json::to_string(&row.close_yesterday_fee)?,serde_json::to_string(&row.close_today_fee)?,row.intraday_open_fee.as_ref().map(serde_json::to_string).transpose()?,status,i64::from(row.is_main_contract),row.source_updated_at,from,observed])?;
    }
    Ok(())
}

#[test]
fn schema_initialization_preserves_legacy_source_labels_and_audit_records() {
    let directory = tempfile::tempdir().unwrap();
    let mut conn = connect(&directory.path().join("history.sqlite")).unwrap();
    ensure_schema(&conn).unwrap();
    insert_official_fixture_rows(
        &mut conn,
        &official_fixture_rows(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    conn.execute("insert into source_state(source_url,last_error_message) values('legacy-source','retained audit')",[]).unwrap();
    conn.execute(
        "insert into baseline_state values('legacy-baseline',?1,1,1,'2026-06-04T12:00:00+08:00')",
        ["a".repeat(64)],
    )
    .unwrap();
    for legacy in ["9qihuo", "jin10", "v11_baseline"] {
        conn.execute("update fee_versions set source_kind=?1", [legacy])
            .unwrap();
        ensure_schema(&conn).unwrap();
        let persisted: String = conn
            .query_row("select source_kind from fee_versions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(persisted, legacy);
    }
    let audit: String = conn
        .query_row(
            "select last_error_message from source_state where source_url='legacy-source'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(audit, "retained audit");
    let baselines: i64 = conn
        .query_row("select count(*) from baseline_state", [], |row| row.get(0))
        .unwrap();
    assert_eq!(baselines, 1);
}
