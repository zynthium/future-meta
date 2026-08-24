use future_meta::query::FutureMeta;
use future_meta_daemon::coverage::{
    CoverageBoundary, CoverageFindingKind, audit_history_coverage, audit_history_coverage_to_path,
};
use future_meta_daemon::czce::{CzceParameterImportOptions, import_daily_parameters};
use future_meta_daemon::db::{
    LatestCompletion, apply_official_fee_transition, apply_official_fee_tuple,
    apply_official_listed_contract_fee_tuple, compare_fee_rows_as_of, complete_latest_rows,
    connect, corroborate_new_contract_metadata, ensure_schema, ensure_seeded,
    migrate_known_contract_spec_history, record_new_contract_metadata_admissions,
    require_complete_latest_metadata, source_probe_hash, source_rule_set_hash, update_source_error,
    update_source_success, upsert_allowed_rows, upsert_latest_rows, upsert_v11_baseline_rows,
};
use future_meta_daemon::export::export_archive;
use future_meta_daemon::latest::parse_latest_html;
use future_meta_daemon::official::{
    EvidenceKind, OfficialEvidence, OfficialFeeAdjustment, OfficialVerification,
    apply_verified_adjustments, stage_adjustment, stage_adjustment_json, stage_adjustments_json,
};
use future_meta_daemon::official_history::{OfficialHistoryImportOptions, import_adjustments};
use future_meta_daemon::official_metadata::{
    OfficialMetadataImportOptions, import_contract_metadata,
};
use future_meta_daemon::parse::parse_csv;
use future_meta_daemon::refresh::{
    RefreshOptions, refresh_with_options, require_official_fee_change_admission, update_latest,
};
use future_meta_daemon::source::discover_sources_from_html;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use time::{Date, Month};

fn czce_parameter_html(base_fee: &str, fee_mode: &str, close_today_fee: &str) -> String {
    format!(
        r"<html><body><p>2020-01-02</p><table>
        <tr><td>合约代码</td><td>交易手续费</td><td>手续费收取方式</td><td>日内平今仓交易手续费</td></tr>
        <tr><td>SR005</td><td>{base_fee}</td><td>{fee_mode}</td><td>{close_today_fee}</td></tr>
        </table></body></html>"
    )
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
    let mut baseline = parse_csv(CSV_V1).unwrap().remove(0);
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
    upsert_v11_baseline_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
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
    let mut baseline = parse_csv(CSV_V1).unwrap().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    upsert_v11_baseline_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
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
fn czce_parameter_import_does_not_replace_paired_official_version() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = parse_csv(CSV_V1).unwrap().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    upsert_v11_baseline_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
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
    drop(conn);
    let html = czce_parameter_html("3.00", "绝对值", "0.00");
    let (manifest, _) = write_czce_parameter_fixture(directory.path(), &html);

    let result = import_daily_parameters(&CzceParameterImportOptions {
        history_db: db_path.clone(),
        manifest,
        snapshot_dir: directory.path().to_path_buf(),
        from: Date::from_calendar_date(2020, Month::January, 1).unwrap(),
        observed_at: "2026-08-24T00:00:00Z".to_owned(),
    })
    .unwrap();

    assert_eq!(result.versions, 0);
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
fn czce_parameter_import_admits_historical_contract_from_product_metadata() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut exemplar = parse_csv(CSV_V1).unwrap().remove(0);
    exemplar.symbol = "CZCE.SR999".to_owned();
    exemplar.lot_size = 10.0;
    exemplar.tick_size = 1.0;
    upsert_v11_baseline_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
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
            "v11_baseline".to_owned()
        )
    );
}

#[test]
fn czce_parameter_presence_fills_missing_observed_lifecycle_bounds() {
    let directory = tempfile::tempdir().unwrap();
    let db_path = directory.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = parse_csv(CSV_V1).unwrap().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = None;
    baseline.expiry_date = None;
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    upsert_v11_baseline_rows(&mut conn, &[baseline], "2026-08-24T00:00:00Z").unwrap();
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
    let mut baseline = parse_csv(CSV_V1).unwrap().remove(0);
    baseline.symbol = "CZCE.SR005".to_owned();
    baseline.listing_date = Some("20191201".to_owned());
    baseline.expiry_date = Some("20200520".to_owned());
    baseline.source_updated_at = Some("2019-12-31 22:00:00".to_owned());
    upsert_v11_baseline_rows(&mut conn, &[baseline.clone()], "2019-12-31T22:00:00+08:00").unwrap();
    let mut lower = baseline;
    lower.open_fee.value = Some(99.0);
    lower.close_yesterday_fee.value = Some(99.0);
    lower.close_today_fee.value = Some(99.0);
    lower.source_updated_at = Some("2020-01-03 22:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[lower], "2020-01-03T22:00:00+08:00").unwrap();
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
    let mut exemplar = parse_csv(CSV_V1).unwrap().remove(0);
    exemplar.symbol = "CFFEX.IF9999".to_owned();
    exemplar.lot_size = 300.0;
    exemplar.tick_size = 0.2;
    upsert_v11_baseline_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
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
    let mut exemplar = parse_csv(CSV_V1).unwrap().remove(0);
    exemplar.symbol = "CFFEX.IF9999".to_owned();
    exemplar.lot_size = 300.0;
    exemplar.tick_size = 0.2;
    upsert_v11_baseline_rows(&mut conn, &[exemplar], "2026-08-24T00:00:00Z").unwrap();
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
    let mut row = parse_csv(CSV_V1).unwrap().remove(0);
    row.symbol = "SHFE.cu2607".to_owned();
    row.listing_date = None;
    row.expiry_date = None;
    upsert_v11_baseline_rows(&mut conn, &[row], "2026-08-24T00:00:00Z").unwrap();
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

const CSV_V1: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.1元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-27 22:56:54,主力合约\n";
const CSV_V2: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.2元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-28 22:56:54,主力合约\n";
const CSV_V1_SOURCE_UPDATED: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.1元,0.1元,0.1元,5,10,50,0.2,49.8,2026-03-28 22:56:54,主力合约\n";
const CSV_V1_SOURCE_EMPTY: &str = "合约品种,合约代码,交易所编码,交易所名称,市价单最大下单量,市价单最小下单量,限价单最大下单量,限价单最小下单量,上市日期,到期日期,是否正在交易,现价,涨/跌停板,买开保证金%,卖开保证金%,保证金/每手(元),开仓手续费,平昨手续费,平今手续费,每手数量,每跳价差,每跳毛利/元,手续费(开+平)/元,每跳净利/元,手续费更新时间,备注\n沪铜2607,cu2607,SHFE,上海期货交易所,30,1,500,1,20250716,20260715,交易中,106870,117550/96180,12,12,64122,0.1元,0.1元,0.1元,5,10,50,0.2,49.8,,主力合约\n";
const LATEST_HTML_CU: &str = r#"
  <div>（手续费更新时间：2026-03-28 22:56:54，价格更新时间：2026-06-08 15:26:53。）</div>
  <table id="heyuetbl">
    <tr><td colspan="15" class="jysname">上海期货交易所</td></tr>
    <tr>
      <td class="heyuealink" title="手续费更新时间：2026-03-28 22:56:54"><a>沪铜2607 (<b>cu2607</b>)</a></td>
      <td class="fee_hide_obj">106870</td>
      <td class="fee_hide_obj">117550/96180</td>
      <td>12%</td>
      <td class="fee_hide_obj">12%</td>
      <td>64122元</td>
      <td>0.2元<br><nobr class="js_single_fee">(0.2元)</nobr></td>
      <td>0.1元<br><nobr class="js_single_fee">(0.1元)</nobr></td>
      <td>0.1元<br><nobr class="js_single_fee">(0.1元)</nobr></td>
      <td class="fee_hide_obj">50</td>
      <td class="fee_hide_obj">0.3元</td>
      <td>49.7</td>
      <td class="fee_hide_obj">主力合约</td>
    </tr>
    <tr>
      <td class="heyuealink" title="手续费更新时间：2026-03-28 22:56:54"><a>沪铝2607 (<b>al2607</b>)</a></td>
      <td></td><td></td>
      <td>10%</td><td>10%</td><td></td>
      <td>3元</td><td>3元</td><td>0元</td>
      <td></td><td></td><td></td><td></td>
    </tr>
  </table>
"#;

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
    upsert_allowed_rows(
        &mut history,
        &parse_csv(CSV_V1).unwrap(),
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
    upsert_allowed_rows(
        &mut history,
        &parse_csv(CSV_V1).unwrap(),
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
fn upsert_creates_new_fee_version_only_for_rule_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("nested").join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let rows_v1 = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &rows_v1, "2026-06-04T12:00:00+08:00").unwrap();
    upsert_allowed_rows(&mut conn, &rows_v1, "2026-06-04T13:00:00+08:00").unwrap();

    let contract_count: i64 = conn
        .query_row("select count(*) from contracts", [], |row| row.get(0))
        .unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    let only_last_seen_at: String = conn
        .query_row("select last_seen_at from fee_versions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(contract_count, 1);
    assert_eq!(fee_version_count, 1);
    assert_eq!(only_last_seen_at, "2026-06-04T13:00:00+08:00");

    let rows_v2 = parse_csv(CSV_V2).unwrap();
    upsert_allowed_rows(&mut conn, &rows_v2, "2026-06-04T14:00:00+08:00").unwrap();

    let contract_count: i64 = conn
        .query_row("select count(*) from contracts", [], |row| row.get(0))
        .unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    let closed_valid_to: String = conn
        .query_row(
            "select valid_to from fee_versions where valid_to is not null",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let closed_last_seen_at: String = conn
        .query_row(
            "select last_seen_at from fee_versions where valid_to is not null",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let open_count: i64 = conn
        .query_row(
            "select count(*) from fee_versions where valid_to is null",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(contract_count, 1);
    assert_eq!(fee_version_count, 2);
    assert_eq!(closed_valid_to, "2026-03-28T00:00:00+08:00");
    assert_eq!(closed_last_seen_at, "2026-06-04T13:00:00+08:00");
    assert_eq!(open_count, 1);
}

#[test]
fn upsert_does_not_make_fee_queryable_before_contract_listing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = parse_csv(CSV_V1).unwrap().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-19 22:56:54".to_owned());

    upsert_allowed_rows(&mut conn, &[row], "2026-03-20T12:00:00+08:00").unwrap();

    let valid_from: String = conn
        .query_row("select valid_from from fee_versions", [], |record| {
            record.get(0)
        })
        .unwrap();

    assert_eq!(valid_from, "2026-03-20T00:00:00+08:00");
}

#[test]
fn schema_repair_clamps_existing_fee_version_to_contract_listing() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut row = parse_csv(CSV_V1).unwrap().remove(0);
    row.listing_date = Some("20260320".to_owned());
    row.source_updated_at = Some("2026-03-20 22:56:54".to_owned());
    upsert_allowed_rows(&mut conn, &[row], "2026-03-20T23:00:00+08:00").unwrap();
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
    assert_eq!(valid_from, "2026-03-20T00:00:00+08:00");
}

#[test]
fn schema_accepts_official_fee_version_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

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
    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();
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

    let mut premature = parse_csv(CSV_V1).unwrap().remove(0);
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
    upsert_v11_baseline_rows(&mut conn, &[premature], "2026-06-05T00:00:00+08:00").unwrap();
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
    let seed = future_meta_daemon::parse::AllowedRow {
        symbol: "GFEX.ps2707".to_owned(),
        listing_date: None,
        expiry_date: None,
        trading_status: future_meta::model::TradingStatus::Unknown,
        buy_margin_rate: None,
        sell_margin_rate: None,
        open_fee: fee.clone(),
        close_yesterday_fee: fee.clone(),
        close_today_fee: close_today.clone(),
        lot_size: 3.0,
        tick_size: 5.0,
        source_updated_at: Some("2026-08-07T00:00:00+08:00".to_owned()),
        is_main_contract: false,
    };
    let metadata_source = future_meta_daemon::parse::AllowedRow {
        symbol: "GFEX.lc2708".to_owned(),
        lot_size: 1.0,
        tick_size: 50.0,
        ..seed.clone()
    };
    upsert_allowed_rows(
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
fn latest_upsert_skips_same_timestamp_conflicting_with_seeded_csv_rule() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-04T12:00:00+08:00").unwrap();

    let mut conflicting = seeded.clone();
    conflicting[0].open_fee.value = Some(0.2);
    conflicting[0].open_fee.raw_text = Some("0.2元".to_owned());
    conflicting[0].source_updated_at = Some("2026-03-27 22:56:54.503".to_owned());

    let skipped = upsert_latest_rows(&mut conn, &conflicting, "2026-06-04T13:00:00+08:00").unwrap();
    let (versions, open_fee): (i64, String) = conn
        .query_row(
            "select count(*), max(open_fee_json) from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
    assert!(open_fee.contains("0.1"));
}

#[test]
fn latest_upsert_rejects_fixed_tenth_placeholder_against_percentage_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap().remove(0);
    for fee in [
        &mut seeded.open_fee,
        &mut seeded.close_yesterday_fee,
        &mut seeded.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
        fee.value = Some(0.5);
        fee.raw_text = Some("0.5/万分之".to_owned());
    }
    seeded.source_updated_at = Some("2026-06-05 21:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[seeded.clone()], "2026-06-05T22:00:00+08:00").unwrap();

    let mut polluted = seeded;
    polluted.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.1),
        raw_text: Some("0.1元".to_owned()),
    };
    polluted.close_yesterday_fee = polluted.open_fee.clone();
    polluted.close_today_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::Zero,
        value: Some(0.0),
        raw_text: Some("0元".to_owned()),
    };
    polluted.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let skipped = upsert_latest_rows(&mut conn, &[polluted], "2026-06-06T22:00:00+08:00").unwrap();
    let (versions, percentage_count): (i64, i64) = conn
        .query_row(
            "select count(*), sum(json_extract(open_fee_json, '$.kind') = 'TurnoverRatePerTenThousand') from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
    assert_eq!(percentage_count, 1);
}

#[test]
fn latest_upsert_does_not_version_margin_or_main_contract_changes_when_fees_match() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut latest = seeded[0].clone();
    latest.buy_margin_rate = Some(99.0);
    latest.sell_margin_rate = Some(98.0);
    latest.is_main_contract = !latest.is_main_contract;
    latest.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let skipped = upsert_latest_rows(&mut conn, &[latest], "2026-06-06T22:00:00+08:00").unwrap();
    let versions: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();

    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
}

#[test]
fn latest_candidate_requires_matching_jin10_fee_tuple_before_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    candidate.open_fee.value = Some(0.2);
    candidate.open_fee.raw_text = Some("0.2元".to_owned());
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let accepted = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate.clone()],
    )
    .unwrap();
    let rejected =
        future_meta_daemon::db::cross_verify_latest_candidates(&conn, &[candidate], &seeded)
            .unwrap();

    assert_eq!(accepted.accepted.len(), 1);
    assert_eq!(accepted.unchanged, 0);
    assert_eq!(accepted.rejected.len(), 0);
    let admission = require_official_fee_change_admission(&accepted).unwrap_err();
    assert!(admission.to_string().contains("official evidence"));
    assert!(rejected.accepted.is_empty());
    assert_eq!(rejected.rejected.len(), 1);
}

#[test]
fn latest_candidate_diagnostic_includes_baseline_sources_and_rejection_reason() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    candidate.open_fee.value = Some(0.2);
    candidate.open_fee.raw_text = Some("0.2元".to_owned());
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let diagnostics = future_meta_daemon::db::diagnose_rejected_latest_candidates(
        &conn,
        &[candidate.clone()],
        &seeded,
    )
    .unwrap();

    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].symbol, candidate.symbol);
    assert_eq!(diagnostics[0].production[0].value, Some(0.1));
    assert_eq!(diagnostics[0].qihuo[0].value, Some(0.2));
    assert!(diagnostics[0].jin10.is_none());
    assert_eq!(
        diagnostics[0].rejection_reason,
        "no same-day Jin10 contract observation"
    );
}

#[test]
fn latest_candidate_treats_legacy_zero_rate_as_equivalent_to_zero_fee() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap();
    seeded[0].close_today_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(0.0),
        raw_text: Some("0/万分之".to_owned()),
    };
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    candidate.close_today_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::Zero,
        value: Some(0.0),
        raw_text: Some("0/万分之".to_owned()),
    };
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let verified =
        future_meta_daemon::db::cross_verify_latest_candidates(&conn, &[candidate], &[]).unwrap();

    assert_eq!(verified.unchanged, 1);
    assert!(verified.accepted.is_empty());
    assert!(verified.rejected.is_empty());
}

#[test]
fn latest_candidate_rejects_confirmed_fee_type_transition_without_official_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seeded = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    for fee in [
        &mut candidate.open_fee,
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
        fee.value = Some(0.5);
        fee.raw_text = Some("0.5/万分之".to_owned());
    }
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate],
    )
    .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(verified.rejected[0].reason.contains("fee type transition"));
}

#[test]
fn latest_candidate_labels_zero_fee_transition_before_fee_kind_transition() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap();
    let row = &mut seeded[0];
    for fee in [
        &mut row.open_fee,
        &mut row.close_yesterday_fee,
        &mut row.close_today_fee,
    ] {
        fee.value = Some(1.0);
        fee.raw_text = Some("1元".to_owned());
    }
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    for fee in [
        &mut candidate.open_fee,
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::Zero;
        fee.value = Some(0.0);
        fee.raw_text = Some("0元".to_owned());
    }
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate],
    )
    .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(verified.rejected[0].reason.contains("zero-fee transition"));
}

#[test]
fn latest_candidate_rejects_confirmed_close_fee_field_swap() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap();
    seeded[0].open_fee.value = Some(1.0);
    seeded[0].close_yesterday_fee.value = Some(1.0);
    seeded[0].close_today_fee.value = Some(3.0);
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    std::mem::swap(
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    );
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate],
    )
    .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(
        verified.rejected[0]
            .reason
            .contains("fee-field permutation")
    );
}

#[test]
fn latest_candidate_rejects_confirmed_multi_fold_change_without_official_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap();
    let row = &mut seeded[0];
    for fee in [
        &mut row.open_fee,
        &mut row.close_yesterday_fee,
        &mut row.close_today_fee,
    ] {
        fee.value = Some(1.0);
        fee.raw_text = Some("1元".to_owned());
    }
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    for fee in [
        &mut candidate.open_fee,
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    ] {
        fee.value = Some(3.0);
        fee.raw_text = Some("3元".to_owned());
    }
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate],
    )
    .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(
        verified.rejected[0]
            .reason
            .contains("multi-fold fee change")
    );
}

#[test]
fn latest_candidates_reject_unusually_large_confirmed_change_batch() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let template = parse_csv(CSV_V1).unwrap().remove(0);
    let mut seeded = Vec::new();
    for symbol in [
        "SHFE.cu2601",
        "SHFE.cu2602",
        "SHFE.cu2603",
        "SHFE.cu2604",
        "SHFE.cu2605",
        "SHFE.cu2606",
        "SHFE.cu2607",
        "SHFE.cu2608",
        "SHFE.cu2609",
        "SHFE.cu2610",
        "SHFE.cu2611",
        "SHFE.cu2612",
        "SHFE.cu2701",
    ] {
        let mut row = template.clone();
        row.symbol = symbol.to_owned();
        for fee in [
            &mut row.open_fee,
            &mut row.close_yesterday_fee,
            &mut row.close_today_fee,
        ] {
            fee.value = Some(1.0);
            fee.raw_text = Some("1元".to_owned());
        }
        seeded.push(row);
    }
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let candidates = seeded
        .into_iter()
        .map(|mut row| {
            for fee in [
                &mut row.open_fee,
                &mut row.close_yesterday_fee,
                &mut row.close_today_fee,
            ] {
                fee.value = Some(1.5);
                fee.raw_text = Some("1.5元".to_owned());
            }
            row.source_updated_at = Some("2026-06-06 21:00:00".to_owned());
            row
        })
        .collect::<Vec<_>>();

    let verified =
        future_meta_daemon::db::cross_verify_latest_candidates(&conn, &candidates, &candidates)
            .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 13);
    assert!(verified.rejected.iter().all(|rejection| {
        rejection
            .reason
            .contains("large fee-change batch requires staged official evidence")
    }));
}

#[test]
fn latest_upsert_rejects_fee_type_transition_without_cross_verification() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap();
    let row = &mut seeded[0];
    for fee in [
        &mut row.open_fee,
        &mut row.close_yesterday_fee,
        &mut row.close_today_fee,
    ] {
        fee.value = Some(1.0);
        fee.raw_text = Some("1元".to_owned());
    }
    upsert_allowed_rows(&mut conn, &seeded, "2026-06-05T22:00:00+08:00").unwrap();

    let mut candidate = seeded[0].clone();
    for fee in [
        &mut candidate.open_fee,
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
        fee.value = Some(0.5);
        fee.raw_text = Some("0.5/万分之".to_owned());
    }
    candidate.source_updated_at = Some("2026-06-06 21:00:00".to_owned());

    let skipped = upsert_latest_rows(&mut conn, &[candidate], "2026-06-06T22:00:00+08:00").unwrap();
    let versions: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();

    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
}

#[test]
fn imports_v11_baseline_into_an_empty_database_with_a_recorded_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let metadata_db = dir.path().join("metadata.sqlite");
    let mut metadata = connect(&metadata_db).unwrap();
    ensure_schema(&metadata).unwrap();
    upsert_allowed_rows(
        &mut metadata,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();
    drop(metadata);

    let input = dir.path().join("v11.tsv");
    std::fs::write(
        &input,
        concat!(
            "exchange\tproduct\tsymbol\tvalid_from\tvalid_to\topen_fee\tclose_yesterday_fee\tclose_today_fee\trecord_source\tconfidence\tnotes\n",
            "SHFE\tCU\tSHFE.cu2607\t2026-03-27T00:00:00+08:00\t2026-03-28T00:00:00+08:00\t{\"kind\":\"CnyPerLot\",\"value\":0.1,\"raw_text\":\"0.1元\"}\t{\"kind\":\"CnyPerLot\",\"value\":0.1,\"raw_text\":\"0.1元\"}\t{\"kind\":\"CnyPerLot\",\"value\":0.1,\"raw_text\":\"0.1元\"}\tofficial_verified\tofficial_verified\tfixture\n",
            "SHFE\tCU\tSHFE.cu2607\t2026-03-28T00:00:00+08:00\t\t{\"kind\":\"CnyPerLot\",\"value\":0.2,\"raw_text\":\"0.2元\"}\t{\"kind\":\"CnyPerLot\",\"value\":0.1,\"raw_text\":\"0.1元\"}\t{\"kind\":\"CnyPerLot\",\"value\":0.1,\"raw_text\":\"0.1元\"}\tofficial_verified\tofficial_verified\tfixture\n"
        ),
    )
    .unwrap();

    let baseline_db = dir.path().join("v11.sqlite");
    let result =
        future_meta_daemon::baseline::import_v11_baseline(&baseline_db, &input, &metadata_db)
            .unwrap();
    let conn = connect(&baseline_db).unwrap();
    future_meta_daemon::baseline::ensure_v11_baseline(&conn).unwrap();
    let (versions, source_kind): (i64, String) = conn
        .query_row(
            "select count(*), max(source_kind) from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();

    assert_eq!(result.rows, 2);
    assert_eq!(result.contracts, 1);
    assert_eq!(versions, 2);
    assert_eq!(source_kind, "v11_baseline");
}

#[test]
fn reviewed_baseline_patch_splits_the_matching_interval() {
    let dir = tempfile::tempdir().unwrap();
    let metadata_db = dir.path().join("metadata.sqlite");
    let mut metadata = connect(&metadata_db).unwrap();
    ensure_schema(&metadata).unwrap();
    upsert_allowed_rows(
        &mut metadata,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();
    drop(metadata);

    let input = dir.path().join("v11.tsv");
    std::fs::write(
        &input,
        concat!(
            "exchange\tproduct\tsymbol\tvalid_from\tvalid_to\topen_fee\tclose_yesterday_fee\tclose_today_fee\trecord_source\tconfidence\tnotes\n",
            "SHFE\tCU\tSHFE.cu2607\t2026-03-27T00:00:00+08:00\t\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\tfixture\tfixture\tfixture\n"
        ),
    )
    .unwrap();
    let patch = dir.path().join("patch.tsv");
    std::fs::write(
        &patch,
        concat!(
            "symbol\tvalid_from\texpected_open_fee\texpected_close_yesterday_fee\texpected_close_today_fee\topen_fee\tclose_yesterday_fee\tclose_today_fee\n",
            "SHFE.cu2607\t2026-06-24T00:00:00+08:00\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\t{\"kind\":\"CnyPerLot\",\"value\":0.1}\t{\"kind\":\"CnyPerLot\",\"value\":0.2}\t{\"kind\":\"CnyPerLot\",\"value\":0.2}\t{\"kind\":\"Zero\",\"value\":0.0}\n"
        ),
    )
    .unwrap();

    let baseline_db = dir.path().join("patched.sqlite");
    future_meta_daemon::baseline::import_v11_baseline_with_patches(
        &baseline_db,
        &input,
        &metadata_db,
        &patch,
    )
    .unwrap();
    let conn = connect(&baseline_db).unwrap();
    let rows = conn
        .prepare("select valid_from, valid_to, close_today_fee_json from fee_versions order by valid_from")
        .unwrap()
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?, row.get::<_, String>(2)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1.as_deref(), Some("2026-06-24T00:00:00+08:00"));
    assert!(rows[1].2.contains("Zero"));
}

#[test]
fn reviewed_baseline_patch_can_retime_an_existing_successor() {
    let dir = tempfile::tempdir().unwrap();
    let metadata_db = dir.path().join("metadata.sqlite");
    let mut metadata = connect(&metadata_db).unwrap();
    ensure_schema(&metadata).unwrap();
    upsert_allowed_rows(
        &mut metadata,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();
    drop(metadata);

    let input = dir.path().join("v11.tsv");
    std::fs::write(
        &input,
        concat!(
            "exchange\tproduct\tsymbol\tvalid_from\tvalid_to\topen_fee\tclose_yesterday_fee\tclose_today_fee\trecord_source\tconfidence\tnotes\n",
            "SHFE\tCU\tSHFE.cu2607\t2026-03-27T00:00:00+08:00\t2026-08-14T00:00:00+08:00\t{\"kind\":\"CnyPerLot\",\"value\":10}\t{\"kind\":\"CnyPerLot\",\"value\":10}\t{\"kind\":\"Zero\",\"value\":0}\tfixture\tfixture\tfixture\n",
            "SHFE\tCU\tSHFE.cu2607\t2026-08-14T00:00:00+08:00\t\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"Zero\",\"value\":0}\tfixture\tfixture\tfixture\n"
        ),
    )
    .unwrap();
    let patch = dir.path().join("patch.tsv");
    std::fs::write(
        &patch,
        concat!(
            "symbol\tvalid_from\tsource_valid_from\texpected_open_fee\texpected_close_yesterday_fee\texpected_close_today_fee\topen_fee\tclose_yesterday_fee\tclose_today_fee\n",
            "SHFE.cu2607\t2026-08-03T00:00:00+08:00\t2026-08-14T00:00:00+08:00\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"Zero\",\"value\":0}\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"CnyPerLot\",\"value\":20}\t{\"kind\":\"Zero\",\"value\":0}\n"
        ),
    )
    .unwrap();

    let baseline_db = dir.path().join("retimed.sqlite");
    future_meta_daemon::baseline::import_v11_baseline_with_patches(
        &baseline_db,
        &input,
        &metadata_db,
        &patch,
    )
    .unwrap();
    let conn = connect(&baseline_db).unwrap();
    let rows = conn
        .prepare("select valid_from, valid_to, open_fee_json from fee_versions order by valid_from")
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].1.as_deref(), Some("2026-08-03T00:00:00+08:00"));
    assert_eq!(rows[1].0, "2026-08-03T00:00:00+08:00");
    assert!(rows[1].2.contains("20"));
}

#[test]
fn latest_observation_activates_a_v11_contract_without_creating_a_fee_version() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("v11.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut baseline = parse_csv(CSV_V1).unwrap();
    baseline[0].is_main_contract = false;
    future_meta_daemon::db::upsert_v11_baseline_rows(
        &mut conn,
        &baseline,
        "2026-06-05T22:00:00+08:00",
    )
    .unwrap();

    let mut latest = baseline.clone();
    latest[0].is_main_contract = true;
    future_meta_daemon::db::mark_latest_contracts_seen(
        &mut conn,
        &latest,
        "2026-06-06T22:00:00+08:00",
    )
    .unwrap();
    let (active, versions, is_main_contract): (i64, i64, i64) = conn
        .query_row(
            "select c.active, (select count(*) from fee_versions), v.is_main_contract
             from contracts c join fee_versions v on v.contract_id = c.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(active, 1);
    assert_eq!(versions, 1);
    assert_eq!(is_main_contract, 1);
}

#[test]
fn latest_upsert_does_not_rewrite_isolated_tenth_terminal_state_from_product_consensus() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seed = parse_csv(CSV_V1).unwrap().remove(0);
    let mut rows = Vec::new();
    for month in 1..=6 {
        let mut row = seed.clone();
        row.symbol = format!("DCE.fb270{month}");
        row.source_updated_at = Some("2026-08-07 21:00:00".to_owned());
        if month != 7 {
            for fee in [
                &mut row.open_fee,
                &mut row.close_yesterday_fee,
                &mut row.close_today_fee,
            ] {
                fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
                fee.value = Some(1.0);
                fee.raw_text = Some("1/万分之".to_owned());
            }
        }
        rows.push(row);
    }
    // Reproduce the polluted terminal state on fb2706 while the rest of the
    // product has the percentage rule.
    {
        let target = rows.last_mut().unwrap();
        target.open_fee = future_meta::model::FeeSpec {
            kind: future_meta::model::FeeKind::CnyPerLot,
            value: Some(0.1),
            raw_text: Some("0.1元".to_owned()),
        };
        target.close_yesterday_fee = target.open_fee.clone();
        target.close_today_fee = target.open_fee.clone();
    }
    upsert_allowed_rows(&mut conn, &rows, "2026-08-08T12:00:00+08:00").unwrap();

    let mut current = rows.last().unwrap().clone();
    for fee in [
        &mut current.open_fee,
        &mut current.close_yesterday_fee,
        &mut current.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
        fee.value = Some(1.0);
        fee.raw_text = Some("1/万分之".to_owned());
    }
    current.source_updated_at = Some("2026-08-18 21:00:00".to_owned());

    let skipped = upsert_latest_rows(&mut conn, &[current], "2026-08-18T22:00:00+08:00").unwrap();
    let (versions, kind, value): (i64, String, f64) = conn
        .query_row(
            "select count(*), json_extract(open_fee_json, '$.kind'),
                    json_extract(open_fee_json, '$.value')
             from fee_versions join contracts on contracts.id = fee_versions.contract_id
             where contracts.symbol = 'DCE.fb2706'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
    assert_eq!(kind, "CnyPerLot");
    assert_eq!(value.to_bits(), 0.1_f64.to_bits());
}

#[test]
fn latest_candidate_requires_official_evidence_for_isolated_tenth_fixed_fee() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seed = parse_csv(CSV_V1).unwrap().remove(0);
    let mut rows = Vec::new();
    for month in 1..=6 {
        let mut row = seed.clone();
        row.symbol = format!("DCE.fb270{month}");
        row.source_updated_at = Some("2026-08-07 21:00:00".to_owned());
        for fee in [
            &mut row.open_fee,
            &mut row.close_yesterday_fee,
            &mut row.close_today_fee,
        ] {
            fee.kind = future_meta::model::FeeKind::TurnoverRatePerTenThousand;
            fee.value = Some(1.0);
            fee.raw_text = Some("1/万分之".to_owned());
        }
        rows.push(row);
    }
    let target = rows.last_mut().unwrap();
    target.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.2),
        raw_text: Some("0.2元".to_owned()),
    };
    target.close_yesterday_fee = target.open_fee.clone();
    target.close_today_fee = target.open_fee.clone();
    upsert_allowed_rows(&mut conn, &rows, "2026-08-08T12:00:00+08:00").unwrap();

    let mut candidate = rows.last().unwrap().clone();
    for fee in [
        &mut candidate.open_fee,
        &mut candidate.close_yesterday_fee,
        &mut candidate.close_today_fee,
    ] {
        fee.value = Some(0.1);
        fee.raw_text = Some("0.1元".to_owned());
    }
    candidate.source_updated_at = Some("2026-08-18 21:00:00".to_owned());

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        &[candidate.clone()],
        &[candidate],
    )
    .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(
        verified.rejected[0]
            .reason
            .contains("isolated 0.1 CNY candidate requires official evidence")
    );
}

#[test]
fn latest_candidate_requires_official_evidence_for_uniform_fixed_collection_offset() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut seeded = parse_csv(CSV_V1).unwrap().remove(0);
    for fee in [
        &mut seeded.open_fee,
        &mut seeded.close_yesterday_fee,
        &mut seeded.close_today_fee,
    ] {
        fee.kind = future_meta::model::FeeKind::CnyPerLot;
        fee.value = Some(5.0);
        fee.raw_text = Some("5元".to_owned());
    }
    seeded.source_updated_at = Some("2026-06-05 21:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[seeded.clone()], "2026-06-05T22:00:00+08:00").unwrap();

    let mut offset = seeded;
    for fee in [
        &mut offset.open_fee,
        &mut offset.close_yesterday_fee,
        &mut offset.close_today_fee,
    ] {
        fee.value = Some(5.1);
        fee.raw_text = Some("5.1元".to_owned());
    }
    offset.source_updated_at = Some("2026-06-06 21:00:00".to_owned());
    let verified =
        future_meta_daemon::db::cross_verify_latest_candidates(&conn, &[offset.clone()], &[offset])
            .unwrap();

    assert!(verified.accepted.is_empty());
    assert_eq!(verified.rejected.len(), 1);
    assert!(
        verified.rejected[0]
            .reason
            .contains("fixed-fee offset requires official evidence")
    );
}

#[test]
fn latest_upsert_rejects_stale_snapshot_that_would_rewrite_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let current = parse_csv(CSV_V2).unwrap();
    upsert_allowed_rows(&mut conn, &current, "2026-06-06T22:00:00+08:00").unwrap();

    let mut stale = current[0].clone();
    stale.open_fee.value = Some(0.3);
    stale.open_fee.raw_text = Some("0.3元".to_owned());
    stale.source_updated_at = Some("2026-03-27 22:00:00".to_owned());
    let skipped = upsert_latest_rows(&mut conn, &[stale], "2026-06-07T22:00:00+08:00").unwrap();

    let (versions, open_fee): (i64, String) = conn
        .query_row(
            "select count(*), max(open_fee_json) from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(skipped, 1);
    assert_eq!(versions, 1);
    assert!(open_fee.contains("0.2"));
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
fn empty_database_is_not_a_seeded_update_base() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let seed_err = ensure_seeded(&conn).unwrap_err();
    let refresh_err = refresh_with_options(
        &db_path,
        RefreshOptions {
            force_full: false,
            require_seed: true,
        },
    )
    .unwrap_err();
    let latest_err = update_latest(&db_path, true).unwrap_err();

    assert!(seed_err.to_string().contains("seeded daemon database"));
    assert!(
        refresh_err
            .to_string()
            .contains("9qihuo single-variety CSV history refresh is retired")
    );
    assert!(latest_err.to_string().contains("seeded daemon database"));
}

#[test]
fn populated_database_is_a_seeded_update_base() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    ensure_seeded(&conn).unwrap();
}

#[test]
fn latest_rows_complete_from_seed_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let latest = parse_latest_html(LATEST_HTML_CU).unwrap();
    let completion = complete_latest_rows(&conn, &latest.rows).unwrap();

    assert_eq!(completion.rows.len(), 1);
    assert_eq!(completion.skipped_missing_metadata, 1);
    assert_eq!(completion.missing_metadata_symbols, ["SHFE.al2607"]);
    let row = &completion.rows[0];
    assert_eq!(row.symbol, "SHFE.cu2607");
    assert_eq!(row.listing_date.as_deref(), Some("20250716"));
    assert_eq!(row.expiry_date.as_deref(), Some("20260715"));
    assert_eq!(row.lot_size.to_bits(), 5.0_f64.to_bits());
    assert_eq!(row.tick_size.to_bits(), 10.0_f64.to_bits());
    assert_eq!(row.open_fee.value, Some(0.2));
    assert!(row.is_main_contract);

    upsert_allowed_rows(&mut conn, &completion.rows, "2026-06-04T13:00:00+08:00").unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fee_version_count, 2);
}

#[test]
fn latest_publish_refuses_any_missing_contract_metadata() {
    let completion = LatestCompletion {
        rows: Vec::new(),
        skipped_missing_metadata: 1,
        missing_metadata_symbols: vec!["SHFE.al2607".to_owned()],
    };

    let error = require_complete_latest_metadata(&completion).unwrap_err();

    assert!(error.to_string().contains('1'));
    assert!(error.to_string().contains("refusing to publish"));
}

#[test]
fn new_contract_metadata_requires_matching_three_source_tick_value() {
    let latest = parse_latest_html(LATEST_HTML_CU).unwrap();
    // The historical CSV can still carry a stale 0.1-CNY placeholder. It is
    // admitted only as static metadata; current fees come from total+Jin10.
    let csv_rows = parse_csv(CSV_V1).unwrap();
    let jin10_rows = csv_rows.clone();

    let enriched =
        corroborate_new_contract_metadata(&latest.rows[..1], &csv_rows, &jin10_rows).unwrap();

    assert_eq!(enriched[0].lot_size, Some(5.0));
    assert_eq!(enriched[0].tick_size, Some(10.0));
    assert_eq!(enriched[0].listing_date.as_deref(), Some("20250716"));
    assert_eq!(enriched[0].expiry_date.as_deref(), Some("20260715"));
}

#[test]
fn corroborated_new_contract_is_not_treated_as_a_fee_change() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let candidate = parse_csv(CSV_V1).unwrap().remove(0);

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        std::slice::from_ref(&candidate),
        std::slice::from_ref(&candidate),
    )
    .unwrap();

    assert_eq!(verified.new_contracts, [candidate]);
    assert!(verified.accepted.is_empty());
    assert!(verified.rejected.is_empty());
}

#[test]
fn new_contract_can_use_explicitly_marked_product_level_jin10_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut candidate = parse_csv(CSV_V1).unwrap().remove(0);
    candidate.symbol = "SHFE.cu2708".to_owned();
    let representative = parse_csv(CSV_V1).unwrap().remove(0);

    let verified = future_meta_daemon::db::cross_verify_latest_candidates(
        &conn,
        std::slice::from_ref(&candidate),
        &[representative],
    )
    .unwrap();

    assert_eq!(verified.new_contracts, [candidate]);
    assert_eq!(verified.degraded_new_contracts, ["SHFE.cu2708"]);
    assert!(verified.rejected.is_empty());

    let mut conn = conn;
    upsert_allowed_rows(
        &mut conn,
        &verified.new_contracts,
        "2026-03-27T23:00:00+08:00",
    )
    .unwrap();
    record_new_contract_metadata_admissions(&conn, &verified, "2026-03-27T23:00:00+08:00").unwrap();
    let level: String = conn
        .query_row(
            "select verification_level from contract_metadata_admissions",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(level, "degraded_product");
}

#[test]
fn contract_static_metadata_changes_create_non_overlapping_versions() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let first = parse_csv(CSV_V1).unwrap().remove(0);
    upsert_allowed_rows(
        &mut conn,
        std::slice::from_ref(&first),
        "2026-03-27T23:00:00+08:00",
    )
    .unwrap();

    let mut changed = first;
    changed.tick_size = 5.0;
    changed.source_updated_at = Some("2026-04-10 00:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[changed], "2026-04-10T01:00:00+08:00").unwrap();

    let versions = conn
        .prepare(
            "select lot_size, tick_size, valid_from, valid_to
             from contract_spec_versions
             order by valid_from",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, f64>(0)?,
                row.get::<_, f64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].0.to_bits(), 5.0_f64.to_bits());
    assert_eq!(versions[0].1.to_bits(), 10.0_f64.to_bits());
    assert_eq!(versions[0].3.as_deref(), Some("2026-04-10T00:00:00+08:00"));
    assert_eq!(versions[1].0.to_bits(), 5.0_f64.to_bits());
    assert_eq!(versions[1].1.to_bits(), 5.0_f64.to_bits());
    assert_eq!(versions[1].2, "2026-04-10T00:00:00+08:00");
    assert_eq!(versions[1].3, None);
}

#[test]
fn known_official_spec_change_repairs_all_listed_contract_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut row = parse_csv(CSV_V1).unwrap().remove(0);
    row.symbol = "DCE.p2605".to_owned();
    row.listing_date = Some("20250519".to_owned());
    row.expiry_date = Some("20260525".to_owned());
    row.lot_size = 10.0;
    row.tick_size = 2.0;
    row.source_updated_at = Some("2025-05-19 00:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[row], "2025-05-19T01:00:00+08:00").unwrap();

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
    let mut row = parse_csv(CSV_V1).unwrap().remove(0);
    row.symbol = "DCE.p2405".to_owned();
    row.listing_date = Some("20230519".to_owned());
    row.expiry_date = None;
    row.lot_size = 10.0;
    row.tick_size = 2.0;
    row.source_updated_at = Some("2023-05-19 00:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[row], "2023-05-19T01:00:00+08:00").unwrap();

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
fn duplicate_symbol_with_distinct_source_dates_creates_history() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut rows = parse_csv(CSV_V1).unwrap();
    rows.push(parse_csv(CSV_V2).unwrap().remove(0));

    upsert_allowed_rows(&mut conn, &rows, "2026-06-04T12:00:00+08:00").unwrap();
    let contract_count: i64 = conn
        .query_row("select count(*) from contracts", [], |row| row.get(0))
        .unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    let closed_valid_to: String = conn
        .query_row(
            "select valid_to from fee_versions where valid_to is not null",
            [],
            |row| row.get(0),
        )
        .unwrap();

    assert_eq!(contract_count, 1);
    assert_eq!(fee_version_count, 2);
    assert_eq!(closed_valid_to, "2026-03-28T00:00:00+08:00");
}

#[test]
fn rejects_non_monotonic_observed_times() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let rows_v1 = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &rows_v1, "2026-06-04T13:00:00+08:00").unwrap();

    let stale_err =
        upsert_allowed_rows(&mut conn, &rows_v1, "2026-06-04T12:00:00+08:00").unwrap_err();
    let rows_v2 = parse_csv(CSV_V2).unwrap();
    upsert_allowed_rows(&mut conn, &rows_v2, "2026-06-04T13:00:00+08:00").unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();

    assert!(
        stale_err
            .to_string()
            .contains("older than current last_seen_at")
    );
    assert_eq!(fee_version_count, 2);
}

#[test]
fn rejects_conflicting_rules_on_same_effective_date() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut rows = parse_csv(CSV_V1).unwrap();
    let same_source_time = CSV_V2.replace("2026-03-28 22:56:54", "2026-03-27 23:30:00");
    rows.push(parse_csv(&same_source_time).unwrap().remove(0));

    let err = upsert_allowed_rows(&mut conn, &rows, "2026-06-04T13:00:00+08:00").unwrap_err();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();

    assert!(err.to_string().contains("conflicting rules"));
    assert_eq!(fee_version_count, 0);
}

#[test]
fn same_rule_updates_source_timestamp_without_new_version() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1_SOURCE_UPDATED).unwrap(),
        "2026-06-04T13:00:00+08:00",
    )
    .unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1_SOURCE_EMPTY).unwrap(),
        "2026-06-04T14:00:00+08:00",
    )
    .unwrap();

    let (fee_version_count, last_seen_at, source_updated_at): (i64, String, String) = conn
        .query_row(
            "select count(*), max(last_seen_at), max(source_updated_at) from fee_versions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();

    assert_eq!(fee_version_count, 1);
    assert_eq!(last_seen_at, "2026-06-04T14:00:00+08:00");
    assert_eq!(source_updated_at, "2026-03-28 22:56:54");
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
fn discovers_single_variety_sources_from_total_page_html() {
    let html = r#"
      <a href="/qihuoshouxufeisingle?heyue=cu">沪铜</a>
      <a href="https://www.9qihuo.com/qihuoshouxufeisingle?heyue=IF">沪深300</a>
      <a href="/qihuoshouxufeisingle?heyue=cu">duplicate</a>
    "#;

    let sources = discover_sources_from_html(html).unwrap();

    assert_eq!(sources.len(), 2);
    assert_eq!(sources[0].heyue, "IF");
    assert_eq!(
        sources[0].csv_url,
        "https://www.9qihuo.com/shouxufeixz?heyue=IF"
    );
    assert_eq!(sources[1].heyue, "cu");
    assert_eq!(
        sources[1].csv_url,
        "https://www.9qihuo.com/shouxufeixz?heyue=cu"
    );
}

#[test]
fn discovery_rejects_non_target_and_ambiguous_sources() {
    let html = r#"
      <a href="/qihuoshouxufeisingle?before=1&heyue=ag#section">ag</a>
      <a href="https://www.9qihuo.com/qihuoshouxufeisingle?heyue=">empty</a>
      <a href="https://evil.example/qihuoshouxufeisingle?heyue=cu">wrong host</a>
      <a href="https://www.9qihuo.com/not/qihuoshouxufeisingle?heyue=al">wrong path</a>
      <a href="/qihuoshouxufeisingle?heyue=cu&heyue=al">duplicate parameter</a>
      <a href="/qihuoshouxufeisingle?heyue=cu&heyue=">duplicate empty parameter</a>
      <a href="/qihuoshouxufeisingle?heyue=cu&heyue=bad%2Fvalue">duplicate invalid parameter</a>
      <a href="/qihuoshouxufeisingle?heyue=cu%2Fbad">encoded slash</a>
      <a href="/qihuoshouxufeisingle?heyue=%20cu%20">encoded whitespace</a>
    "#;

    let sources = discover_sources_from_html(html).unwrap();

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].heyue, "ag");
    assert_eq!(
        sources[0].detail_url,
        "https://www.9qihuo.com/qihuoshouxufeisingle?heyue=ag"
    );
}

#[test]
fn discovery_rejects_empty_source_list() {
    let html = r#"
      <html><body>
        <script>window.location.href = "/qihuoshouxufei";</script>
      </body></html>
    "#;

    let err = discover_sources_from_html(html).unwrap_err();

    assert!(err.to_string().contains("no 9qihuo variety sources"));
}

#[test]
fn exports_archive_loadable_by_client() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let out = dir.path().join("public");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    export_archive(&db_path, &out).unwrap();
    let manifest_text = std::fs::read_to_string(out.join("manifest.json")).unwrap();
    assert!(manifest_text.contains("latest.fmeta.zst"));

    let bytes = std::fs::read(out.join("latest.fmeta.zst")).unwrap();
    let archive = future_meta::archive::decode_archive_bytes(&bytes).unwrap();
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
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    conn.execute("update fee_versions set source_kind = 'jin10'", [])
        .unwrap();
    drop(conn);

    let err = export_archive(&db_path, &out).unwrap_err();

    assert!(
        err.to_string()
            .contains("refusing to export untrusted Jin10 fee versions")
    );
}

#[test]
fn source_state_tracks_last_successful_probe() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    assert_eq!(
        source_probe_hash(&conn, "https://www.9qihuo.com/shouxufeixz?heyue=cu").unwrap(),
        None
    );

    update_source_success(
        &conn,
        "https://www.9qihuo.com/shouxufeixz?heyue=cu",
        "probe-v1",
        "rules-v1",
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    update_source_success(
        &conn,
        "https://www.9qihuo.com/shouxufeixz?heyue=cu",
        "probe-v2",
        "rules-v2",
        "2026-06-04T13:00:00+08:00",
    )
    .unwrap();

    assert_eq!(
        source_probe_hash(&conn, "https://www.9qihuo.com/shouxufeixz?heyue=cu").unwrap(),
        Some("probe-v2".to_owned())
    );
    assert_eq!(
        source_rule_set_hash(&conn, "https://www.9qihuo.com/shouxufeixz?heyue=cu").unwrap(),
        Some("rules-v2".to_owned())
    );
    let (rule_set_hash, success_at): (String, String) = conn
        .query_row(
            "select last_rule_set_hash, last_success_at from source_state where source_url = ?1",
            ["https://www.9qihuo.com/shouxufeixz?heyue=cu"],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(rule_set_hash, "rules-v2");
    assert_eq!(success_at, "2026-06-04T13:00:00+08:00");
}

#[test]
fn source_rule_set_hash_treats_an_error_only_state_as_not_successful() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    update_source_error(
        &conn,
        "https://www.9qihuo.com/qihuoshouxufei",
        "2026-08-22T12:00:00+08:00",
        "refused unconfirmed candidate",
    )
    .unwrap();

    assert_eq!(
        source_rule_set_hash(&conn, "https://www.9qihuo.com/qihuoshouxufei").unwrap(),
        None
    );
}

#[test]
fn historical_fee_comparison_uses_the_rule_effective_on_the_snapshot_day() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut earlier = parse_csv(CSV_V1).unwrap().remove(0);
    earlier.source_updated_at = Some("2026-03-27 21:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[earlier.clone()], "2026-03-27T22:00:00+08:00").unwrap();

    let mut later = earlier.clone();
    later.open_fee.value = Some(0.2);
    later.open_fee.raw_text = Some("0.2元".to_owned());
    later.source_updated_at = Some("2026-03-29 21:00:00".to_owned());
    upsert_allowed_rows(&mut conn, &[later], "2026-03-29T22:00:00+08:00").unwrap();

    let (compared, differences) =
        compare_fee_rows_as_of(&conn, &[earlier], "2026-03-28T00:00:00+08:00").unwrap();

    assert_eq!(compared, 1);
    assert!(differences.is_empty());
}

#[test]
fn source_probe_hash_is_stable_and_source_specific() {
    let first = future_meta_daemon::hash::source_probe_hash(
        "https://www.9qihuo.com/shouxufeixz?heyue=cu",
        "https://www.9qihuo.com/qihuoshouxufeisingle?heyue=cu",
    );
    let same = future_meta_daemon::hash::source_probe_hash(
        "https://www.9qihuo.com/shouxufeixz?heyue=cu",
        "https://www.9qihuo.com/qihuoshouxufeisingle?heyue=cu",
    );
    let different = future_meta_daemon::hash::source_probe_hash(
        "https://www.9qihuo.com/shouxufeixz?heyue=al",
        "https://www.9qihuo.com/qihuoshouxufeisingle?heyue=al",
    );

    assert_eq!(first, same);
    assert_ne!(first, different);
}

#[test]
fn jin10_snapshot_uses_verified_close_field_order_and_static_metadata() {
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2025-03-15",
        "heyue_code": "cu2505",
        "pub_date_commission": "2025-03-14 23:16:31",
        "buy_ratio": "9%",
        "sell_ratio": "9%",
        "buy_commission": "0.5/万分之(20元)",
        "sell_cur_commission": "0.5/万分之(20元)",
        "sell_yesterday_commission": "1/万分之(40元)",
        "per_ratio": "50",
        "jys": "上海期货交易所",
        "status": 1
      }]
    }"#;
    let metadata = BTreeMap::from([(
        "SHFE.cu".to_owned(),
        future_meta_daemon::jin10::ContractStaticMetadata {
            lot_size: 5.0,
            tick_size: 10.0,
        },
    )]);

    let snapshot = future_meta_daemon::jin10::parse_snapshot(payload, &metadata).unwrap();

    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.skipped_missing_metadata, 0);
    let row = &snapshot.rows[0];
    assert_eq!(row.symbol, "SHFE.cu2505");
    assert_eq!(row.buy_margin_rate, Some(9.0));
    assert_eq!(row.open_fee.value, Some(0.5));
    assert_eq!(row.close_yesterday_fee.value, Some(0.5));
    assert_eq!(row.close_today_fee.value, Some(1.0));
    assert_eq!(
        row.source_updated_at.as_deref(),
        Some("2025-03-16 00:00:00")
    );

    let natural_payload = payload.replace("2025-03-15", "2025-10-30");
    let natural_snapshot =
        future_meta_daemon::jin10::parse_snapshot(&natural_payload, &metadata).unwrap();
    let natural_row = &natural_snapshot.rows[0];
    assert_eq!(natural_row.close_yesterday_fee.value, Some(1.0));
    assert_eq!(natural_row.close_today_fee.value, Some(0.5));
}

#[test]
fn jin10_snapshot_rejects_static_metadata_that_fails_tick_value_check() {
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2025-03-15",
        "heyue_code": "cu2505",
        "pub_date_commission": "2025-03-14 23:16:31",
        "buy_ratio": "9%",
        "sell_ratio": "9%",
        "buy_commission": "0.5/万分之(20元)",
        "sell_cur_commission": "0.5/万分之(20元)",
        "sell_yesterday_commission": "1/万分之(40元)",
        "per_ratio": "50",
        "jys": "上海期货交易所",
        "status": 1
      }]
    }"#;
    let metadata = BTreeMap::from([(
        "SHFE.cu".to_owned(),
        future_meta_daemon::jin10::ContractStaticMetadata {
            lot_size: 5.0,
            tick_size: 5.0,
        },
    )]);

    let err = future_meta_daemon::jin10::parse_snapshot(payload, &metadata).unwrap_err();

    assert!(err.to_string().contains("per_ratio"));
}

#[test]
fn jin10_snapshot_skips_monthly_average_contracts() {
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2025-10-29", "heyue_code": "l2602F",
        "buy_ratio": "7%", "sell_ratio": "7%",
        "buy_commission": "1元", "sell_cur_commission": "1元",
        "sell_yesterday_commission": "1元", "per_ratio": "5",
        "jys": "大连商品交易所", "status": 1
      }]
    }"#;

    let snapshot = future_meta_daemon::jin10::parse_snapshot(payload, &BTreeMap::new()).unwrap();

    assert!(snapshot.rows.is_empty());
    assert_eq!(snapshot.skipped_missing_metadata, 0);
    assert_eq!(snapshot.skipped_invalid_symbols, 1);
}

#[test]
fn product_static_metadata_is_derived_from_seeded_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata(&conn).unwrap();

    assert_eq!(
        metadata.get("SHFE.cu"),
        Some(&future_meta_daemon::jin10::ContractStaticMetadata {
            lot_size: 5.0,
            tick_size: 10.0,
        })
    );
}

#[test]
fn product_static_metadata_candidates_retain_verified_tick_size_changes() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let mut old_tick = parse_csv(CSV_V1).unwrap().remove(0);
    old_tick.symbol = "INE.ec2604".to_owned();
    old_tick.lot_size = 50.0;
    old_tick.tick_size = 0.1;
    let mut new_tick = old_tick.clone();
    new_tick.symbol = "INE.ec2605".to_owned();
    new_tick.tick_size = 0.5;
    upsert_allowed_rows(
        &mut conn,
        &[old_tick, new_tick],
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();

    assert_eq!(
        metadata.get("INE.ec"),
        Some(&vec![
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 0.1,
            },
            future_meta_daemon::jin10::ContractStaticMetadata {
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
    let mut palm_oil = parse_csv(CSV_V1).unwrap().remove(0);
    palm_oil.symbol = "DCE.p2605".to_owned();
    palm_oil.lot_size = 10.0;
    palm_oil.tick_size = 1.0;
    let mut soybean_oil = palm_oil.clone();
    soybean_oil.symbol = "DCE.y2605".to_owned();
    let mut lithium_carbonate = palm_oil.clone();
    lithium_carbonate.symbol = "GFEX.lc2605".to_owned();
    lithium_carbonate.lot_size = 1.0;
    lithium_carbonate.tick_size = 20.0;
    upsert_allowed_rows(
        &mut conn,
        &[palm_oil, soybean_oil, lithium_carbonate],
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();

    let metadata = future_meta_daemon::db::product_static_metadata_candidates(&conn).unwrap();

    assert_eq!(
        metadata.get("DCE.p"),
        Some(&vec![
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ])
    );
    assert_eq!(
        metadata.get("DCE.y"),
        Some(&vec![
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 1.0,
            },
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 10.0,
                tick_size: 2.0,
            },
        ])
    );
    assert_eq!(
        metadata.get("GFEX.lc"),
        Some(&vec![
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 1.0,
                tick_size: 20.0,
            },
            future_meta_daemon::jin10::ContractStaticMetadata {
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
        Some(&vec![future_meta_daemon::jin10::ContractStaticMetadata {
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
            Some(&vec![future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size,
                tick_size,
            }]),
            "{product}"
        );
    }
}

#[test]
fn historical_backfill_inserts_before_live_data_without_regressing_contract_state() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    let current = parse_csv(CSV_V1).unwrap();
    upsert_allowed_rows(&mut conn, &current, "2026-06-04T12:00:00+08:00").unwrap();

    let mut historical = current[0].clone();
    historical.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::CnyPerLot,
        value: Some(0.2),
        raw_text: Some("0.2元".to_owned()),
    };
    historical.source_updated_at = Some("2026-03-20 22:56:54".to_owned());
    future_meta_daemon::db::backfill_allowed_rows(
        &mut conn,
        &[historical],
        "2026-03-21T12:00:00+08:00",
    )
    .unwrap();

    let (versions, first_seen_at, last_seen_at, active): (i64, String, String, i64) = conn
        .query_row(
            "select count(v.id), c.first_seen_at, c.last_seen_at, c.active
             from contracts c
             join fee_versions v on v.contract_id = c.id
             group by c.id",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .unwrap();
    assert_eq!(versions, 2);
    assert_eq!(first_seen_at, "2026-03-21T12:00:00+08:00");
    assert_eq!(last_seen_at, "2026-06-04T12:00:00+08:00");
    assert_eq!(active, 1);
}

#[test]
fn historical_backfill_keeps_later_same_day_source_update() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut live = parse_csv(CSV_V1).unwrap().remove(0);
    live.source_updated_at = Some("2026-03-27 21:11:40".to_owned());
    upsert_allowed_rows(&mut conn, &[live.clone()], "2026-03-27T22:00:00+08:00").unwrap();

    let mut historical = live;
    historical.source_updated_at = Some("2026-03-27 00:00:00".to_owned());
    historical.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(0.23),
        raw_text: Some("0.23/万分之".to_owned()),
    };
    future_meta_daemon::db::backfill_allowed_rows(
        &mut conn,
        &[historical],
        "2026-03-27T00:00:00+08:00",
    )
    .unwrap();

    let valid_from = conn
        .prepare(
            "select valid_from from fee_versions
             order by valid_from",
        )
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        valid_from,
        vec![
            "2026-03-27T00:00:00+08:00".to_owned(),
            "2026-03-27T21:11:40+08:00".to_owned(),
        ]
    );
}

#[test]
fn historical_backfill_prefers_trading_status_for_same_source_second() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut live = parse_csv(CSV_V1).unwrap().remove(0);
    live.source_updated_at = Some("2026-03-27 21:11:40".to_owned());
    upsert_allowed_rows(&mut conn, &[live.clone()], "2026-03-27T22:00:00+08:00").unwrap();

    let mut not_trading = live.clone();
    not_trading.trading_status = future_meta::model::TradingStatus::NotTrading;
    not_trading.is_main_contract = false;
    not_trading.buy_margin_rate = Some(20.0);
    not_trading.sell_margin_rate = Some(20.0);
    conn.execute(
        "update fee_versions
         set rule_hash = ?1, buy_margin_rate = 20, sell_margin_rate = 20,
             trading_status = 'NotTrading', is_main_contract = 0,
             valid_to = '2026-03-27T21:11:40+08:00'",
        [future_meta_daemon::hash::row_rule_hash(&not_trading)],
    )
    .unwrap();
    conn.execute(
        "insert into fee_versions(
             contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
             open_fee_json, close_yesterday_fee_json, close_today_fee_json,
             trading_status, is_main_contract, source_updated_at,
             valid_from, valid_to, first_seen_at, last_seen_at
         )
         select contract_id, ?1, 12, 12,
                open_fee_json, close_yesterday_fee_json, close_today_fee_json,
                'Trading', 1, source_updated_at,
                '2026-03-27T21:11:40+08:00', null, first_seen_at, last_seen_at
         from fee_versions",
        [future_meta_daemon::hash::row_rule_hash(&live)],
    )
    .unwrap();

    let mut historical = live;
    historical.source_updated_at = Some("2026-03-27 00:00:00".to_owned());
    historical.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(0.23),
        raw_text: Some("0.23/万分之".to_owned()),
    };
    future_meta_daemon::db::backfill_allowed_rows(
        &mut conn,
        &[historical],
        "2026-03-27T00:00:00+08:00",
    )
    .unwrap();

    let versions = conn
        .prepare(
            "select valid_from, trading_status, is_main_contract
             from fee_versions order by valid_from",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        versions,
        vec![
            (
                "2026-03-27T00:00:00+08:00".to_owned(),
                "Trading".to_owned(),
                1,
            ),
            (
                "2026-03-27T21:11:40+08:00".to_owned(),
                "Trading".to_owned(),
                1,
            ),
        ]
    );
}

#[test]
fn historical_backfill_moves_later_9q_observation_and_audits_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut live = parse_csv(CSV_V1).unwrap().remove(0);
    live.source_updated_at = Some("2026-04-02 21:19:00".to_owned());
    upsert_allowed_rows(&mut conn, &[live.clone()], "2026-04-02T22:00:00+08:00").unwrap();
    conn.execute(
        "update fee_versions set valid_from = '2026-03-27T00:00:00+08:00'",
        [],
    )
    .unwrap();

    let mut historical = live;
    historical.source_updated_at = Some("2026-03-27 00:00:00".to_owned());
    historical.open_fee = future_meta::model::FeeSpec {
        kind: future_meta::model::FeeKind::TurnoverRatePerTenThousand,
        value: Some(5.0),
        raw_text: Some("5/万分之".to_owned()),
    };
    future_meta_daemon::db::backfill_allowed_rows(
        &mut conn,
        &[historical],
        "2026-03-27T00:00:00+08:00",
    )
    .unwrap();

    let valid_from = conn
        .prepare("select valid_from from fee_versions order by valid_from")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        valid_from,
        vec![
            "2026-03-27T00:00:00+08:00".to_owned(),
            "2026-04-02T21:19:00+08:00".to_owned(),
        ]
    );

    let conflict: (String, String, String) = conn
        .query_row(
            "select incumbent_source, contender_source, selected_source
             from fee_rule_conflicts",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        conflict,
        ("9qihuo".to_owned(), "jin10".to_owned(), "jin10".to_owned())
    );
}

#[test]
fn jin10_response_groups_rows_by_source_snapshot_date() {
    let payload = r#"{
      "status": 200,
      "data": [
        {
          "date": "2025-03-14", "heyue_code": "cu2505",
          "pub_date_commission": "2025-03-13 23:16:31",
          "buy_ratio": "9%", "sell_ratio": "9%",
          "buy_commission": "0.5/万分之(20元)",
          "sell_cur_commission": "0.5/万分之(20元)",
          "sell_yesterday_commission": "1/万分之(40元)",
          "per_ratio": "50", "jys": "上海期货交易所", "status": 1
        },
        {
          "date": "2025-03-15", "heyue_code": "cu2505",
          "pub_date_commission": "2025-03-14 23:16:31",
          "buy_ratio": "9%", "sell_ratio": "9%",
          "buy_commission": "0.5/万分之(20元)",
          "sell_cur_commission": "0.5/万分之(20元)",
          "sell_yesterday_commission": "1/万分之(40元)",
          "per_ratio": "50", "jys": "上海期货交易所", "status": 1
        }
      ]
    }"#;
    let metadata = BTreeMap::from([(
        "SHFE.cu".to_owned(),
        future_meta_daemon::jin10::ContractStaticMetadata {
            lot_size: 5.0,
            tick_size: 10.0,
        },
    )]);

    let snapshots = future_meta_daemon::jin10::parse_snapshots(payload, &metadata).unwrap();

    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].observed_at, "2025-03-15T00:00:00+08:00");
    assert_eq!(snapshots[1].observed_at, "2025-03-16T00:00:00+08:00");
    assert_eq!(snapshots[0].snapshot.rows.len(), 1);
    assert_eq!(snapshots[1].snapshot.rows.len(), 1);
    assert_eq!(
        snapshots[0].snapshot.rows[0].source_updated_at.as_deref(),
        Some("2025-03-15 00:00:00")
    );
    assert_eq!(
        snapshots[1].snapshot.rows[0].source_updated_at.as_deref(),
        Some("2025-03-16 00:00:00")
    );
}

#[test]
fn jin10_snapshot_selects_unique_static_candidate_from_per_tick_value() {
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2025-03-15", "heyue_code": "ec2506",
        "pub_date_commission": "2025-03-14 23:16:31",
        "buy_ratio": "12%", "sell_ratio": "12%",
        "buy_commission": "1元", "sell_cur_commission": "1元",
        "sell_yesterday_commission": "0元", "per_ratio": "5",
        "jys": "上海国际能源交易中心", "status": 1
      }]
    }"#;
    let metadata = BTreeMap::from([(
        "INE.ec".to_owned(),
        vec![
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 0.1,
            },
            future_meta_daemon::jin10::ContractStaticMetadata {
                lot_size: 50.0,
                tick_size: 0.5,
            },
        ],
    )]);

    let snapshot =
        future_meta_daemon::jin10::parse_snapshot_with_candidates(payload, &metadata).unwrap();

    assert_eq!(snapshot.rows[0].lot_size.to_bits(), 50.0_f64.to_bits());
    assert_eq!(snapshot.rows[0].tick_size.to_bits(), 0.1_f64.to_bits());
}

#[test]
fn jin10_payload_does_not_mutate_a_seeded_database() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    drop(conn);
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2026-03-20", "heyue_code": "cu2607",
        "pub_date_commission": "2026-03-20 22:56:54",
        "buy_ratio": "12%", "sell_ratio": "12%",
        "buy_commission": "0.2元", "sell_cur_commission": "0.1元",
        "sell_yesterday_commission": "0.1元", "per_ratio": "50",
        "jys": "上海期货交易所", "status": 1
      }]
    }"#;

    let err = future_meta_daemon::refresh::backfill_jin10_payload(&db_path, payload).unwrap_err();
    assert!(
        err.to_string()
            .contains("Jin10 historical backfill is retired and cannot be used")
    );

    let conn = connect(&db_path).unwrap();
    let versions: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(versions, 1);
}

#[test]
fn jin10_backfill_is_retired_even_for_a_seeded_database() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    drop(conn);

    let err = future_meta_daemon::refresh::backfill_jin10_payload(&db_path, "{}").unwrap_err();

    assert!(
        err.to_string()
            .contains("Jin10 historical backfill is retired and cannot be used")
    );
}

#[test]
fn jin10_range_backfill_is_retired_before_network_or_input_validation() {
    let db_path = tempfile::tempdir()
        .unwrap()
        .path()
        .join("future-meta.sqlite");

    let err =
        future_meta_daemon::refresh::backfill_jin10(&db_path, "not-a-date", "also-not-a-date")
            .unwrap_err();

    assert!(
        err.to_string()
            .contains("Jin10 historical backfill is retired and cannot be used")
    );
}

#[test]
fn nineqihuo_csv_history_refresh_is_retired_before_creating_a_database() {
    let db_path = tempfile::tempdir()
        .unwrap()
        .path()
        .join("future-meta.sqlite");

    let err = refresh_with_options(
        &db_path,
        RefreshOptions {
            force_full: false,
            require_seed: true,
        },
    )
    .unwrap_err();

    assert!(
        err.to_string()
            .contains("9qihuo single-variety CSV history refresh is retired")
    );
    assert!(!db_path.exists());
}

#[test]
fn jin10_backfill_does_not_record_source_snapshot_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    drop(conn);
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2026-03-20", "heyue_code": "cu2607",
        "buy_ratio": "12%", "sell_ratio": "12%",
        "buy_commission": "0.2元", "sell_cur_commission": "0.1元",
        "sell_yesterday_commission": "0.1元", "per_ratio": "50",
        "jys": "上海期货交易所", "status": 1
      }]
    }"#;

    let err = future_meta_daemon::refresh::backfill_jin10_payload(&db_path, payload).unwrap_err();
    assert!(
        err.to_string()
            .contains("Jin10 historical backfill is retired and cannot be used")
    );

    let conn = connect(&db_path).unwrap();
    let snapshots: i64 = conn
        .query_row("select count(*) from jin10_source_snapshots", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(snapshots, 0);
}

#[test]
fn jin10_payload_does_not_add_historical_only_contracts() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();
    upsert_allowed_rows(
        &mut conn,
        &parse_csv(CSV_V1).unwrap(),
        "2026-06-04T12:00:00+08:00",
    )
    .unwrap();
    drop(conn);
    let payload = r#"{
      "status": 200,
      "data": [{
        "date": "2025-03-15", "heyue_code": "cu2505",
        "pub_date_commission": "2025-03-14 23:16:31",
        "buy_ratio": "9%", "sell_ratio": "9%",
        "buy_commission": "0.5/万分之(20元)",
        "sell_cur_commission": "0.5/万分之(20元)",
        "sell_yesterday_commission": "1/万分之(40元)",
        "per_ratio": "50", "jys": "上海期货交易所", "status": 1
      }]
    }"#;

    let err = future_meta_daemon::refresh::backfill_jin10_payload(&db_path, payload).unwrap_err();
    assert!(
        err.to_string()
            .contains("Jin10 historical backfill is retired and cannot be used")
    );

    let conn = connect(&db_path).unwrap();
    let versions: i64 = conn
        .query_row(
            "select count(*) from fee_versions v
             join contracts c on c.id = v.contract_id
             where c.symbol = 'SHFE.cu2505'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(versions, 0);
}

#[test]
fn jin10_snapshot_keeps_latest_duplicate_contract_rule_for_next_day() {
    let payload = r#"{
      "status": 200,
      "data": [
        {
          "date": "2024-11-20", "heyue_code": "IC2412",
          "pub_date_commission": "2024-11-19 22:44:34",
          "buy_ratio": "12%", "sell_ratio": "12%",
          "buy_commission": "0.01元", "sell_cur_commission": "0.01元",
          "sell_yesterday_commission": "0.01元", "per_ratio": "40",
          "jys": "中国金融期货交易所", "status": 1,
          "updated_at": "2024-11-20T05:23:05.000Z"
        },
        {
          "date": "2024-11-20", "heyue_code": "IC2412",
          "pub_date_commission": "2024-11-20 23:48:18",
          "buy_ratio": "12%", "sell_ratio": "12%",
          "buy_commission": "0.23/万分之(27元)",
          "sell_cur_commission": "0.23/万分之(27元)",
          "sell_yesterday_commission": "2.3/万分之(270.1元)",
          "per_ratio": "40", "jys": "中国金融期货交易所", "status": 1,
          "updated_at": "2024-11-20T15:59:02.000Z"
        }
      ]
    }"#;
    let metadata = BTreeMap::from([(
        "CFFEX.IC".to_owned(),
        future_meta_daemon::jin10::ContractStaticMetadata {
            lot_size: 200.0,
            tick_size: 0.2,
        },
    )]);

    let snapshot = future_meta_daemon::jin10::parse_snapshot(payload, &metadata).unwrap();

    assert_eq!(snapshot.rows.len(), 1);
    assert_eq!(snapshot.rows[0].open_fee.value, Some(0.23));
    assert_eq!(
        snapshot.rows[0].source_updated_at.as_deref(),
        Some("2024-11-21 00:00:00")
    );
}

#[test]
fn jin10_range_url_uses_exact_source_date_filter() {
    let url = future_meta_daemon::jin10::range_url("2024-06-11", "2024-06-30").unwrap();
    let query = url.query_pairs().collect::<BTreeMap<_, _>>();

    assert_eq!(
        query.get("tb_name").map(std::convert::AsRef::<str>::as_ref),
        Some("_vir_26")
    );
    assert_eq!(
        query.get("search").map(std::convert::AsRef::<str>::as_ref),
        Some(r#"{"range,date":"2024-06-11,2024-06-30","status":1}"#)
    );
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
    assert!(report.findings.is_empty());
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
