use future_meta_daemon::{compact, db, rebuild, update_collected::update_collected};
use rusqlite::{Connection, params};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::Path;

const EXCHANGES: [&str; 6] = ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"];
fn i(value: usize) -> i64 {
    i64::try_from(value).unwrap()
}
fn fixture(path: &Path, days: &[&str], changed: bool, extra: bool) {
    let conn = db::connect(path).unwrap();
    db::ensure_schema(&conn).unwrap();
    conn.execute_batch("create table rebuild_inputs(id integer primary key,path text not null,sha256 text not null);
    create table rebuild_contract_provenance(contract_id integer primary key references contracts(id),provenance_json text not null);
    create table rebuild_spec_provenance(spec_id integer primary key references contract_spec_versions(id),source_json text not null);
    create table retained_parameter_documents(id integer primary key,exchange text,trading_date text,canonical_url text,body_sha256 text,retained_path text);
    create table official_observations(contract_id integer references contracts(id),trade_date text,version_id integer references fee_versions(id),input_id integer references rebuild_inputs(id),row_number integer,fee_document_id integer references retained_parameter_documents(id),margin_document_id integer references retained_parameter_documents(id),primary key(contract_id,trade_date)) without rowid;
    create table rebuild_summary(report_json text not null);").unwrap();
    let body = b"<docs><doc><pubdate>2026-10-01</pubdate><title>holiday</title></doc></docs>";
    let raw = path.with_extension("xml");
    std::fs::write(&raw, body).unwrap();
    let sha = hex::encode(Sha256::digest(body));
    conn.execute(
        "insert into rebuild_inputs values(1,'original.csv',?1)",
        [&sha],
    )
    .unwrap();
    conn.execute("insert into retained_parameter_documents values(1,'CALENDAR',null,'http://www.cffex.com.cn/sj/jyrl/202610/index_6782.xml',?1,?2)",params![sha,raw.to_str()]).unwrap();
    let mut counts = serde_json::Map::new();
    for (index, exchange) in EXCHANGES.iter().enumerate() {
        let url = match *exchange {
            "CFFEX" => "http://www.cffex.com.cn/sj/",
            "CZCE" => "https://www.czce.com.cn/cn/",
            "DCE" => "http://www.dce.com.cn/dce/",
            "GFEX" => "https://www.gfex.com.cn/gfex/",
            "INE" => "https://www.ine.cn/data/",
            _ => "https://www.shfe.com.cn/data/",
        };
        for (d, day) in days.iter().enumerate() {
            conn.execute(
                "insert into retained_parameter_documents values(?1,?2,?3,?4,?5,?6)",
                params![
                    2 + i(index * days.len() + d),
                    exchange,
                    day,
                    url,
                    sha,
                    raw.to_str()
                ],
            )
            .unwrap();
        }
        add_contract(
            &conn,
            i(index) + 1,
            &format!(
                "{exchange}.{}",
                if *exchange == "CZCE" {
                    "SR701"
                } else {
                    "cu2701"
                }
            ),
            days,
            changed && *exchange == "SHFE",
            url,
            &sha,
            2 + i(index * days.len()),
        );
        counts.insert((*exchange).to_owned(), json!(days.len()));
    }
    if extra {
        add_contract(
            &conn,
            7,
            "SHFE.cu2702",
            days,
            false,
            "https://www.shfe.com.cn/data/",
            &sha,
            2 + i(5 * days.len()),
        );
        counts.insert("SHFE".to_owned(), json!(days.len() * 2));
    }
    for (index, _) in EXCHANGES.iter().enumerate() {
        for (d, day) in days.iter().enumerate() {
            let margin = 2 + i(index * days.len() + d);
            let fee = if index == 4 {
                2 + i(5 * days.len() + d)
            } else {
                margin
            };
            conn.execute("update official_observations set fee_document_id=?1,margin_document_id=?2 where contract_id=?3 and trade_date=?4",params![fee,margin,i(index)+1,day]).unwrap();
        }
    }
    if extra {
        for (d, day) in days.iter().enumerate() {
            conn.execute("update official_observations set fee_document_id=?1,margin_document_id=?1 where contract_id=7 and trade_date=?2",params![2+i(5*days.len()+d),day]).unwrap();
        }
    }
    conn.execute("insert into rebuild_summary values(?1)",[json!({"source":"fresh_collector_outputs_no_seed","from":days[0],"through":days[days.len()-1],"observations":days.len()*(6+usize::from(extra)),"per_exchange":counts,"assumed_expiry_symbols":0}).to_string()]).unwrap();
    rebuild::verify(path).unwrap();
}
#[allow(clippy::too_many_arguments)]
fn add_contract(
    conn: &Connection,
    id: i64,
    symbol: &str,
    days: &[&str],
    changed: bool,
    url: &str,
    sha: &str,
    doc: i64,
) {
    let from = format!("{}T00:00:00+08:00", days[0]);
    let fee =
        json!({"kind":"CnyPerLot","value":if changed {2.0}else{1.0},"raw_text":null}).to_string();
    conn.execute(
        "insert into contracts values(?1,?2,'20261001','20270115',5,1,?3,?3,1)",
        params![id, symbol, from],
    )
    .unwrap();
    conn.execute("insert into rebuild_contract_provenance values(?1,?2)",params![id,json!({"official_fields":{"listing_date":{"value":"2026-10-01","evidence_id":sha},"expiry_date":{"value":"2027-01-15","evidence_id":sha}}}).to_string()]).unwrap();
    conn.execute("insert into contract_lifecycle_evidence values(?1,'20261001','20270115',?2,?3,?4,'official')",params![id,url,sha,from]).unwrap();
    conn.execute("insert into contract_spec_versions values(?1,?1,5,1,'2026-10-01T00:00:00+08:00','2027-01-16T00:00:00+08:00','official',?2,?3,?3)",params![id,url,from]).unwrap();
    conn.execute(
        "insert into rebuild_spec_provenance values(?1,'{\"evidence_ids\":\"original-sha\"}')",
        [id],
    )
    .unwrap();
    conn.execute(
        "insert into contract_spec_evidence values(?1,'2026-10-01T00:00:00+08:00',?2,?3,?4)",
        params![id, url, sha, from],
    )
    .unwrap();
    conn.execute("insert into fee_versions(id,contract_id,rule_hash,buy_margin_rate,sell_margin_rate,open_fee_json,close_yesterday_fee_json,close_today_fee_json,trading_status,is_main_contract,source_kind,valid_from,first_seen_at,last_seen_at,intraday_open_fee_json) values(?1,?1,?2,0.1,0.12,?3,?3,?3,'Unknown',0,'official',?4,?4,?5,?6)",params![id,if changed {"changed"}else{"original"},fee,from,format!("{}T00:00:00+08:00",days[days.len()-1]),json!({"kind":"CnyPerLot","value":9.0,"raw_text":null}).to_string()]).unwrap();
    conn.execute(
        "insert into fee_version_evidence values(?1,?2,?3,'official_parameter',?4,?5,?2)",
        params![
            id,
            from,
            if changed { "changed" } else { "original" },
            url,
            sha
        ],
    )
    .unwrap();
    for day in days {
        conn.execute(
            "insert into official_observations values(?1,?2,?1,1,2,?3,?3)",
            params![id, day, doc],
        )
        .unwrap();
    }
}
fn count(path: &Path, table: &str) -> i64 {
    db::connect_readonly(path)
        .unwrap()
        .query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))
        .unwrap()
}

#[test]
fn unchanged_extends_observations_and_changed_closes_previous_version() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(&delta, &["2026-10-27", "2026-10-28"], true, false);
    let report = update_collected(&seed, &delta, &out).unwrap();
    assert_eq!(report["observations"], 18);
    assert_eq!(report["fee_versions"], 7);
    assert_eq!(count(&seed, "official_observations"), 6);
    let conn = db::connect_readonly(&out).unwrap();
    let (end, last): (String, String) = conn
        .query_row(
            "select valid_to,last_seen_at from fee_versions where contract_id=6 and id=6",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(end, "2026-10-27T00:00:00+08:00");
    assert_eq!(last, "2026-10-26T00:00:00+08:00");
    let (last, intraday): (String, String) = conn
        .query_row(
            "select last_seen_at,intraday_open_fee_json from fee_versions where contract_id=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(last, "2026-10-28T00:00:00+08:00");
    assert!(intraday.contains("9.0"));
    assert!(
        update_collected(&seed, &delta, &out)
            .unwrap_err()
            .to_string()
            .contains("already exists")
    );
}
#[test]
fn identical_overlap_is_idempotent_and_changed_overlap_is_rejected() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    let bad = t.path().join("bad");
    let rejected = t.path().join("rejected");
    fixture(&seed, &["2026-10-26", "2026-10-27"], false, false);
    fixture(&delta, &["2026-10-27", "2026-10-28"], false, false);
    update_collected(&seed, &delta, &out).unwrap();
    let again = t.path().join("again");
    update_collected(&out, &delta, &again).unwrap();
    assert_eq!(count(&again, "official_observations"), 18);
    assert_eq!(count(&again, "fee_versions"), 6);
    fixture(&bad, &["2026-10-27", "2026-10-28"], true, false);
    assert!(
        update_collected(&seed, &bad, &rejected)
            .unwrap_err()
            .to_string()
            .contains("changed historical")
    );
    assert!(!rejected.exists());
    assert_eq!(count(&seed, "official_observations"), 12);
}
#[test]
fn imports_complete_new_contract_and_preserves_original_snapshot_shas() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(&delta, &["2026-10-27"], false, true);
    let report = update_collected(&seed, &delta, &out).unwrap();
    assert_eq!(report["contracts"], 7);
    let conn = db::connect_readonly(&out).unwrap();
    let original:String=conn.query_row("select d.body_sha256 from official_observations o join contracts c on c.id=o.contract_id join retained_parameter_documents d on d.id=o.fee_document_id where c.symbol='SHFE.cu2702'",[],|r|r.get(0)).unwrap();
    assert_eq!(
        original,
        hex::encode(sha2::Sha256::digest(
            std::fs::read(delta.with_extension("xml")).unwrap()
        ))
    );
}
#[test]
fn rejects_gaps_and_missing_calendar_or_contract_provenance() {
    for damage in ["gap", "calendar", "contract"] {
        let t = tempfile::tempdir().unwrap();
        let seed = t.path().join("seed");
        let delta = t.path().join("delta");
        let out = t.path().join("out");
        fixture(&seed, &["2026-10-26"], false, false);
        fixture(
            &delta,
            &[if damage == "gap" {
                "2026-10-28"
            } else {
                "2026-10-27"
            }],
            false,
            true,
        );
        let conn = db::connect(&delta).unwrap();
        if damage == "calendar" {
            conn.execute(
                "delete from retained_parameter_documents where exchange='CALENDAR'",
                [],
            )
            .unwrap();
        }
        if damage == "contract" {
            conn.execute(
                "delete from rebuild_contract_provenance where contract_id=7",
                [],
            )
            .unwrap();
        }
        drop(conn);
        assert!(update_collected(&seed, &delta, &out).is_err(), "{damage}");
        assert!(!out.exists());
    }
}
#[test]
fn compact_seed_retains_unresolved_candidates_and_snapshots() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let packed = t.path().join("packed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(&delta, &["2026-10-27"], false, false);
    let conn = db::connect(&seed).unwrap();
    conn.execute_batch("insert into announcement_documents values('citics','123','title','2026-10-26','https://broker/123','now','now');insert into announcement_candidates values('citics','123','[]','[]','now',null);insert into announcement_document_snapshots values(1,'citics','123','aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa','html','text','[]','now');").unwrap();
    drop(conn);
    compact::compact_seed(&seed, &packed).unwrap();
    update_collected(&packed, &delta, &out).unwrap();
    assert_eq!(count(&out, "announcement_document_snapshots"), 1);
    let conn = db::connect_readonly(&out).unwrap();
    let resolved: Option<String> = conn
        .query_row("select resolved_at from announcement_candidates", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert!(resolved.is_none());
}

#[test]
fn margin_and_intraday_open_changes_remain_separate_from_close_today() {
    for field in [
        "buy_margin_rate",
        "sell_margin_rate",
        "intraday_open_fee_json",
    ] {
        let t = tempfile::tempdir().unwrap();
        let seed = t.path().join("seed");
        let delta = t.path().join("delta");
        let out = t.path().join("out");
        fixture(&seed, &["2026-10-26"], false, false);
        fixture(&delta, &["2026-10-27"], false, false);
        let conn = db::connect(&delta).unwrap();
        if field == "intraday_open_fee_json" {
            conn.execute("update fee_versions set intraday_open_fee_json=json_set(intraday_open_fee_json,'$.value',10.0) where id=6",[]).unwrap();
        } else {
            conn.execute(
                &format!("update fee_versions set {field}=0.15 where id=6"),
                [],
            )
            .unwrap();
        }
        drop(conn);
        let report = update_collected(&seed, &delta, &out).unwrap();
        assert_eq!(report["fee_versions"], 7, "{field}");
        let conn = db::connect_readonly(&out).unwrap();
        let close:f64=conn.query_row("select json_extract(close_today_fee_json,'$.value') from fee_versions where contract_id=6 and valid_to is null",[],|r|r.get(0)).unwrap();
        assert_eq!(close, 1.0);
    }
}

#[test]
fn rejects_changed_retained_bytes_and_incomplete_specification_lineage() {
    for damage in ["bytes", "spec", "date"] {
        let t = tempfile::tempdir().unwrap();
        let seed = t.path().join("seed");
        let delta = t.path().join("delta");
        let out = t.path().join("out");
        fixture(&seed, &["2026-10-26"], false, false);
        fixture(&delta, &["2026-10-27"], false, true);
        let conn = db::connect(&delta).unwrap();
        match damage {
            "bytes" => std::fs::write(delta.with_extension("xml"), "changed").unwrap(),
            "spec" => {
                conn.execute("delete from contract_spec_evidence where contract_id=7", [])
                    .unwrap();
            }
            _ => {
                conn.execute("update retained_parameter_documents set trading_date='2026-10-26' where exchange='SHFE'",[]).unwrap();
            }
        }
        drop(conn);
        assert!(update_collected(&seed, &delta, &out).is_err(), "{damage}");
        assert!(!out.exists());
    }
}

#[test]
fn nontrading_interval_endpoint_uses_last_actual_trading_day() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-22"], false, false);
    fixture(&delta, &["2026-10-23"], false, false);
    let conn = db::connect(&delta).unwrap();
    conn.execute(
        "update rebuild_summary set report_json=json_set(report_json,'$.through','2026-10-25')",
        [],
    )
    .unwrap();
    drop(conn);
    let report = update_collected(&seed, &delta, &out).unwrap();
    assert_eq!(report["through"], "2026-10-25");
    let conn = db::connect_readonly(&out).unwrap();
    let active: i64 = conn
        .query_row("select count(*) from contracts where active=1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(active, 6);
}

#[test]
fn weekend_bridge_requires_declared_and_evidenced_full_interval() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-23"], false, false);
    fixture(&delta, &["2026-10-26"], false, false);
    assert!(
        update_collected(&seed, &delta, &out)
            .unwrap_err()
            .to_string()
            .contains("coverage gap")
    );
    let conn = db::connect(&delta).unwrap();
    conn.execute(
        "update rebuild_summary set report_json=json_set(report_json,'$.from','2026-10-24')",
        [],
    )
    .unwrap();
    drop(conn);
    let report = update_collected(&seed, &delta, &out).unwrap();
    assert_eq!(report["through"], "2026-10-26");
    assert_eq!(report["observations"], 12);
}

#[test]
fn historical_czce_http_metadata_accepts_only_exact_primary_hosts() {
    for host in [
        "www.czce.com.cn",
        "czce.com.cn",
        "www.czce.com.cn.attacker.example",
        "attacker.example@www.czce.com.cn",
    ] {
        let t = tempfile::tempdir().unwrap();
        let seed = t.path().join("seed");
        let delta = t.path().join("delta");
        let out = t.path().join("out");
        fixture(&seed, &["2026-10-26"], false, false);
        fixture(&delta, &["2026-10-27"], false, false);
        let conn = db::connect(&delta).unwrap();
        let url =
            format!("http://{host}/cn/gyjys/jysdt/ggytz/webinfo/2020/07/1593526781274908.htm");
        conn.execute(
            "update contract_lifecycle_evidence set canonical_url=?1 where contract_id=2",
            [url],
        )
        .unwrap();
        drop(conn);
        let result = update_collected(&seed, &delta, &out);
        assert_eq!(
            result.is_ok(),
            host == "www.czce.com.cn" || host == "czce.com.cn",
            "{host}: {result:?}"
        );
    }
}

#[test]
fn metadata_accepts_all_collector_official_domains_without_weakening_fee_sources() {
    for domain in [
        "shfe.com.cn",
        "shfe.cn",
        "ine.cn",
        "dce.com.cn",
        "czce.com.cn",
        "gfex.com.cn",
        "cffex.com.cn",
    ] {
        for scheme in ["http", "https"] {
            let t = tempfile::tempdir().unwrap();
            let seed = t.path().join("seed");
            let delta = t.path().join("delta");
            let out = t.path().join("out");
            fixture(&seed, &["2026-10-26"], false, false);
            fixture(&delta, &["2026-10-27"], false, false);
            let conn = db::connect(&delta).unwrap();
            let url = format!(
                "{scheme}://www.{domain}/products/futures/energyandchemical/br_f/standard_br_f/202312/t20231205_327333.html"
            );
            conn.execute(
                "update contract_spec_evidence set canonical_url=?1 where contract_id=6",
                [&url],
            )
            .unwrap();
            drop(conn);
            assert!(update_collected(&seed, &delta, &out).is_ok(), "{url}");
        }
    }
    for url in [
        "https://www.shfe.cn.attacker.example/doc",
        "https://attacker.example@www.shfe.cn/doc",
        "https://www.shfe.cn:8443/doc",
        "ftp://www.shfe.cn/doc",
    ] {
        let t = tempfile::tempdir().unwrap();
        let seed = t.path().join("seed");
        let delta = t.path().join("delta");
        let out = t.path().join("out");
        fixture(&seed, &["2026-10-26"], false, false);
        fixture(&delta, &["2026-10-27"], false, false);
        let conn = db::connect(&delta).unwrap();
        conn.execute(
            "update contract_spec_evidence set canonical_url=?1 where contract_id=6",
            [url],
        )
        .unwrap();
        drop(conn);
        assert!(update_collected(&seed, &delta, &out).is_err(), "{url}");
    }
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(&delta, &["2026-10-27"], false, false);
    let conn = db::connect(&delta).unwrap();
    conn.execute("update fee_version_evidence set canonical_url='https://www.shfe.cn/doc' where contract_id=6",[]).unwrap();
    drop(conn);
    assert!(update_collected(&seed, &delta, &out).is_err());
}

fn remove_observation(path: &Path, contract: i64, day: &str) {
    let conn = db::connect(path).unwrap();
    conn.execute(
        "delete from official_observations where contract_id=?1 and trade_date=?2",
        params![contract, day],
    )
    .unwrap();
    conn.execute("update fee_versions set last_seen_at=(select max(trade_date)||'T00:00:00+08:00' from official_observations where contract_id=?1) where contract_id=?1",[contract]).unwrap();
    conn.execute("update rebuild_summary set report_json=json_set(report_json,'$.observations',(select count(*) from official_observations))",[]).unwrap();
    rebuild::verify(path).unwrap();
}

#[test]
fn missing_known_contract_day_cannot_hide_behind_another_contract_on_same_exchange() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, true);
    fixture(&delta, &["2026-10-27", "2026-10-28"], false, true);
    remove_observation(&delta, 6, "2026-10-28");
    let seed_before = std::fs::read(&seed).unwrap();
    let error = update_collected(&seed, &delta, &out)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("SHFE.cu2701") && error.contains("2026-10-28"),
        "{error}"
    );
    assert!(!out.exists());
    assert_eq!(std::fs::read(&seed).unwrap(), seed_before);
}

#[test]
fn new_contract_first_observed_midinterval_keeps_earlier_prefix_unknown() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(
        &delta,
        &["2026-10-27", "2026-10-28", "2026-10-29"],
        false,
        true,
    );
    remove_observation(&delta, 7, "2026-10-27");
    let conn = db::connect(&delta).unwrap();
    conn.execute("update fee_versions set valid_from='2026-10-28T00:00:00+08:00',first_seen_at='2026-10-28T00:00:00+08:00' where contract_id=7",[]).unwrap();
    conn.execute("update fee_version_evidence set valid_from='2026-10-28T00:00:00+08:00' where contract_id=7",[]).unwrap();
    drop(conn);
    update_collected(&seed, &delta, &out).unwrap();
    let conn = db::connect_readonly(&out).unwrap();
    let days:i64=conn.query_row("select count(*) from official_observations o join contracts c on c.id=o.contract_id where c.symbol='SHFE.cu2702'",[],|r|r.get(0)).unwrap();
    assert_eq!(days, 2);
}

#[test]
fn expired_contract_may_be_absent_after_its_final_trading_date() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, true);
    fixture(&delta, &["2026-10-27", "2026-10-28"], false, true);
    remove_observation(&delta, 7, "2026-10-28");
    for path in [&seed, &delta] {
        let conn = db::connect(path).unwrap();
        conn.execute("update contracts set expiry_date='20261027' where id=7", [])
            .unwrap();
        conn.execute("update rebuild_contract_provenance set provenance_json=json_set(provenance_json,'$.official_fields.expiry_date.value','2026-10-27') where contract_id=7",[]).unwrap();
        conn.execute(
            "update contract_lifecycle_evidence set expiry_date='20261027' where contract_id=7",
            [],
        )
        .unwrap();
    }
    let conn = db::connect(&delta).unwrap();
    conn.execute(
        "update fee_versions set valid_to='2026-10-28T00:00:00+08:00' where contract_id=7",
        [],
    )
    .unwrap();
    drop(conn);
    update_collected(&seed, &delta, &out).unwrap();
    let conn = db::connect_readonly(&out).unwrap();
    let active: bool = conn
        .query_row(
            "select active from contracts where symbol='SHFE.cu2702'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!active);
}

#[test]
fn newly_observed_contract_requires_every_subsequent_trading_day() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(
        &delta,
        &["2026-10-27", "2026-10-28", "2026-10-29"],
        false,
        true,
    );
    remove_observation(&delta, 7, "2026-10-28");
    let error = update_collected(&seed, &delta, &out)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("SHFE.cu2702") && error.contains("2026-10-28"),
        "{error}"
    );
    assert!(!out.exists());
}

#[test]
fn assumed_expiry_summary_remains_a_count_and_includes_new_contracts() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26"], false, false);
    fixture(&delta, &["2026-10-27"], false, true);
    let conn = db::connect(&delta).unwrap();
    conn.execute("update rebuild_contract_provenance set provenance_json=json_set(provenance_json,'$.official_fields.expiry_date.calendar_assumption',json('{}'),'$.official_fields.expiry_date.source','user_assumed_holiday_calendar') where contract_id=7",[]).unwrap();
    drop(conn);
    let report = update_collected(&seed, &delta, &out).unwrap();
    assert_eq!(report["assumed_expiry_symbols"], 1);
    assert_eq!(rebuild::verify(&out).unwrap()["assumed_expiry_symbols"], 1);
}

#[test]
fn reviewed_seed_gaps_and_never_observed_contracts_remain_explicit_unknowns() {
    let t = tempfile::tempdir().unwrap();
    let seed = t.path().join("seed");
    let delta = t.path().join("delta");
    let out = t.path().join("out");
    fixture(&seed, &["2026-10-26", "2026-10-28"], false, true);
    fixture(&delta, &["2026-10-29"], false, true);
    for path in [&seed, &delta] {
        let conn = db::connect(path).unwrap();
        conn.execute("delete from official_observations where contract_id=7", [])
            .unwrap();
        conn.execute("delete from fee_version_evidence where contract_id=7", [])
            .unwrap();
        conn.execute("delete from fee_versions where contract_id=7", [])
            .unwrap();
        conn.execute("update rebuild_summary set report_json=json_set(report_json,'$.observations',(select count(*) from official_observations))",[]).unwrap();
        drop(conn);
        rebuild::verify(path).unwrap();
    }
    update_collected(&seed, &delta, &out).unwrap();
    let conn = db::connect_readonly(&out).unwrap();
    let unknown:i64=conn.query_row("select count(*) from official_observations o join contracts c on c.id=o.contract_id where o.trade_date='2026-10-27' or c.symbol='SHFE.cu2702'",[],|r|r.get(0)).unwrap();
    assert_eq!(unknown, 0);
}
