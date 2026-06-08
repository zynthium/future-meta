use future_meta::query::FutureMeta;
use future_meta_daemon::db::{
    complete_latest_rows, connect, ensure_schema, ensure_seeded, source_probe_hash,
    source_rule_set_hash, update_source_success, upsert_allowed_rows,
};
use future_meta_daemon::export::export_archive;
use future_meta_daemon::latest::parse_latest_html;
use future_meta_daemon::parse::parse_csv;
use future_meta_daemon::refresh::{RefreshOptions, refresh_with_options, update_latest};
use future_meta_daemon::source::discover_sources_from_html;

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
    assert_eq!(closed_valid_to, "2026-03-28T22:56:54+08:00");
    assert_eq!(closed_last_seen_at, "2026-06-04T13:00:00+08:00");
    assert_eq!(open_count, 1);
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
    assert!(refresh_err.to_string().contains("seeded daemon database"));
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
    let row = &completion.rows[0];
    assert_eq!(row.symbol, "SHFE.cu2607");
    assert_eq!(row.listing_date.as_deref(), Some("20250716"));
    assert_eq!(row.expiry_date.as_deref(), Some("20260715"));
    assert_eq!(row.lot_size, 5.0);
    assert_eq!(row.tick_size, 10.0);
    assert_eq!(row.open_fee.value, Some(0.2));
    assert!(row.is_main_contract);

    upsert_allowed_rows(&mut conn, &completion.rows, "2026-06-04T13:00:00+08:00").unwrap();
    let fee_version_count: i64 = conn
        .query_row("select count(*) from fee_versions", [], |row| row.get(0))
        .unwrap();
    assert_eq!(fee_version_count, 2);
}

#[test]
fn duplicate_symbol_with_distinct_source_times_creates_history() {
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
    assert_eq!(closed_valid_to, "2026-03-28T22:56:54+08:00");
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
fn rejects_conflicting_rules_at_same_source_timestamp() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("future-meta.sqlite");
    let mut conn = connect(&db_path).unwrap();
    ensure_schema(&conn).unwrap();

    let mut rows = parse_csv(CSV_V1).unwrap();
    let same_source_time = CSV_V2.replace("2026-03-28 22:56:54", "2026-03-27 22:56:54");
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
