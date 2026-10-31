//! Append verified official collector observations to a collected seed.

use crate::{compact_storage, db, rebuild};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
};
use time::{Date, format_description};

/// Merge into a separate compact database; input files are never modified.
///
/// # Errors
/// Rejects changed historical observations, incomplete provenance, gaps, or an
/// existing output. Failed imports remove their private work file.
pub fn update_collected(seed: &Path, delta: &Path, output: &Path) -> Result<Value> {
    ensure!(
        !output.exists(),
        "output already exists: {}",
        output.display()
    );
    rebuild::verify(seed).context("verify seed")?;
    rebuild::verify(delta).context("verify delta")?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let work = parent.join(format!(".update-collected-{}.sqlite", std::process::id()));
    ensure!(!work.exists(), "temporary update output already exists");
    let result = merge(seed, delta, &work).and_then(|report| {
        rebuild::verify(&work)?;
        // A hard link publishes without replacing an output created concurrently.
        std::fs::hard_link(&work, output)?;
        Ok(report)
    });
    let _ = std::fs::remove_file(&work);
    result
}

fn summary(conn: &Connection, schema: &str) -> Result<Value> {
    let text: String = conn.query_row(
        &format!("select report_json from {schema}.rebuild_summary"),
        [],
        |r| r.get(0),
    )?;
    Ok(serde_json::from_str(&text)?)
}

#[allow(clippy::too_many_lines)]
fn merge(seed: &Path, delta: &Path, work: &Path) -> Result<Value> {
    let source = db::connect_readonly(seed)?;
    source.execute(
        "vacuum main into ?1",
        [work.to_str().context("invalid output path")?],
    )?;
    drop(source);
    let mut conn = db::connect(work)?;
    conn.execute(
        "attach database ?1 as delta",
        [delta.to_str().context("invalid delta path")?],
    )?;
    let mut report = summary(&conn, "main")?;
    let incoming = summary(&conn, "delta")?;
    let old_through = report["through"]
        .as_str()
        .context("missing seed through")?
        .to_owned();
    let first = incoming["from"].as_str().context("missing delta from")?;
    let through = incoming["through"]
        .as_str()
        .context("missing delta through")?;
    let format = format_description::parse("[year]-[month]-[day]")?;
    let next = Date::parse(&old_through, &format)?
        .next_day()
        .context("coverage overflow")?;
    ensure!(
        Date::parse(first, &format)? <= next,
        "delta interval leaves a coverage gap after {old_through}"
    );
    let trading_days = validate_delta(&conn, first, through)?;
    let calendars: i64 = conn.query_row(
        "select count(*) from delta.retained_parameter_documents where exchange='CALENDAR'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        calendars > 0,
        "delta missing retained trading calendar provenance"
    );
    for exchange in ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"] {
        ensure!(
            incoming["per_exchange"][exchange].as_u64().unwrap_or(0) > 0,
            "delta missing exchange observations: {exchange}"
        );
    }
    let tx = conn.transaction()?;
    compact_storage::expand_provenance(&tx)?;
    tx.execute_batch(
        "create temp table contract_map(old integer primary key,new integer not null);
        create temp table input_map(old integer primary key,new integer not null);
        create temp table document_map(old integer primary key,new integer not null);
        create temp table version_map(old integer primary key,new integer not null);",
    )?;
    let contract_rows = tx.prepare("select id,symbol,listing_date,expiry_date,lot_size,tick_size from delta.contracts order by id")?
        .query_map([], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?,r.get::<_,f64>(4)?,r.get::<_,f64>(5)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (old, symbol, listing, expiry, lot, tick) in contract_rows {
        let complete: bool = tx.query_row("select exists(select 1 from delta.rebuild_contract_provenance where contract_id=?1) and exists(select 1 from delta.contract_lifecycle_evidence where contract_id=?1) and exists(select 1 from delta.contract_spec_versions s join delta.rebuild_spec_provenance p on p.spec_id=s.id join delta.contract_spec_evidence e on e.contract_id=s.contract_id and e.valid_from=s.valid_from where s.contract_id=?1 and s.source_kind='official')", [old], |r| r.get(0))?;
        ensure!(complete, "incomplete official contract metadata: {symbol}");
        let existing: Option<(i64,String,String,f64,f64)> = tx.query_row("select id,listing_date,expiry_date,lot_size,tick_size from contracts where symbol=?1", [&symbol], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional()?;
        let id = if let Some((id, l, e, a, b)) = existing {
            ensure!(
                (l, e, a, b) == (listing, expiry, lot, tick),
                "changed existing lifecycle or specification: {symbol}"
            );
            id
        } else {
            ensure!(
                through > old_through.as_str(),
                "overlapped replay introduces new contract: {symbol}"
            );
            tx.execute("insert into contracts(symbol,listing_date,expiry_date,lot_size,tick_size,first_seen_at,last_seen_at,active) select symbol,listing_date,expiry_date,lot_size,tick_size,first_seen_at,last_seen_at,active from delta.contracts where id=?1", [old])?;
            let id = tx.last_insert_rowid();
            tx.execute("insert into rebuild_contract_provenance select ?1,provenance_json from delta.rebuild_contract_provenance where contract_id=?2", params![id,old])?;
            id
        };
        tx.execute("insert into contract_map values(?1,?2)", params![old, id])?;
    }
    copy_metadata(&tx)?;
    for (table, map, fields) in [
        ("rebuild_inputs", "input_map", "path,sha256"),
        (
            "retained_parameter_documents",
            "document_map",
            "exchange,trading_date,canonical_url,body_sha256,retained_path",
        ),
    ] {
        let ids = tx
            .prepare(&format!("select id from delta.{table} order by id"))?
            .query_map([], |r| r.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for old in ids {
            let identity = fields
                .split(',')
                .map(|field| format!("a.{field} is b.{field}"))
                .collect::<Vec<_>>()
                .join(" and ");
            let existing: Option<i64> = tx.query_row(&format!("select a.id from {table} a join delta.{table} b on {identity} where b.id=?1 limit 1"),[old],|r|r.get(0)).optional()?;
            let mapped = if let Some(id) = existing {
                id
            } else {
                tx.execute(&format!("insert into {table}({fields}) select {fields} from delta.{table} where id=?1"), [old])?;
                tx.last_insert_rowid()
            };
            tx.execute(
                &format!("insert into {map} values(?1,?2)"),
                params![old, mapped],
            )?;
        }
    }
    let mut version_map = HashMap::new();
    let observations = tx.prepare("select o.contract_id,m.new,o.trade_date,o.version_id,o.input_id,o.row_number,o.fee_document_id,o.margin_document_id from delta.official_observations o join contract_map m on m.old=o.contract_id order by m.new,o.trade_date")?
        .query_map([], |r| Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?,r.get::<_,i64>(5)?,r.get::<_,i64>(6)?,r.get::<_,i64>(7)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (_old_contract, contract, day, old_version, input, row, fee_doc, margin_doc) in observations
    {
        let prior: Option<i64> = tx.query_row("select version_id from official_observations where contract_id=?1 and trade_date=?2", params![contract,day], |r|r.get(0)).optional()?;
        let version = if let Some(prior) = prior {
            ensure!(
                same_state(&tx, prior, old_version)?,
                "changed historical observation: contract {contract} {day}"
            );
            prior
        } else {
            ensure!(
                day > old_through,
                "delta adds previously unknown historical observation: contract {contract} {day}"
            );
            let latest: Option<i64> = tx.query_row("select id from fee_versions where contract_id=?1 order by julianday(valid_from) desc limit 1", [contract], |r|r.get(0)).optional()?;
            let unchanged = latest
                .map(|v| same_state(&tx, v, old_version))
                .transpose()?
                .unwrap_or(false);
            if let Some(latest) = latest.filter(|_| unchanged) {
                tx.execute(
                    "update fee_versions set last_seen_at=?1 where id=?2",
                    params![format!("{day}T00:00:00+08:00"), latest],
                )?;
                latest
            } else {
                let boundary = format!("{day}T00:00:00+08:00");
                if let Some(previous) = latest {
                    let old_end: Option<String> = tx.query_row(
                        "select valid_to from fee_versions where id=?1",
                        [previous],
                        |r| r.get(0),
                    )?;
                    ensure!(
                        old_end.is_none()
                            || old_end.as_deref().is_some_and(|v| v >= boundary.as_str()),
                        "cannot reopen expired fee history: contract {contract}"
                    );
                    tx.execute(
                        "update fee_versions set valid_to=?1 where id=?2",
                        params![boundary, previous],
                    )?;
                }
                tx.execute("insert into fee_versions(contract_id,rule_hash,buy_margin_rate,sell_margin_rate,open_fee_json,close_yesterday_fee_json,close_today_fee_json,intraday_open_fee_json,trading_status,is_main_contract,source_kind,source_updated_at,valid_from,valid_to,first_seen_at,last_seen_at) select ?1,rule_hash,buy_margin_rate,sell_margin_rate,open_fee_json,close_yesterday_fee_json,close_today_fee_json,intraday_open_fee_json,trading_status,is_main_contract,source_kind,source_updated_at,?2,null,?2,?2 from delta.fee_versions where id=?3", params![contract,boundary,old_version])?;
                tx.last_insert_rowid()
            }
        };
        if let Some(mapped) = version_map.insert(old_version, version) {
            ensure!(
                mapped == version,
                "delta state spans inconsistent existing versions"
            );
        }
        if prior.is_none() {
            tx.execute("insert into official_observations select ?1,?2,?3,i.new,?4,f.new,m.new from input_map i,document_map f,document_map m where i.old=?5 and f.old=?6 and m.old=?7", params![contract,day,version,row,input,fee_doc,margin_doc])?;
        }
    }
    for (old, new) in version_map {
        tx.execute("insert into version_map values(?1,?2)", params![old, new])?;
    }
    tx.execute_batch("insert or ignore into fee_version_evidence select m.new,v.valid_from,v.rule_hash,e.evidence_level,e.canonical_url,e.body_sha256,e.recorded_at from delta.fee_version_evidence e join delta.fee_versions d on d.contract_id=e.contract_id and d.valid_from=e.valid_from join version_map vm on vm.old=d.id join fee_versions v on v.id=vm.new join contract_map m on m.old=e.contract_id;")?;
    let updated_through = through.max(old_through.as_str());
    tx.execute("update contracts set active=exists(select 1 from official_observations o where o.contract_id=contracts.id and o.trade_date=(select max(trade_date) from official_observations) and replace(contracts.expiry_date,'-','')>=replace(?1,'-','')),last_seen_at=coalesce((select max(v.last_seen_at) from fee_versions v where v.contract_id=contracts.id),last_seen_at)",[updated_through])?;
    // Expiry intervals supplied by the validated delta remain exact.
    tx.execute_batch("update fee_versions set valid_to=(select d.valid_to from delta.fee_versions d join version_map m on m.old=d.id where m.new=fee_versions.id and d.valid_to is not null order by d.valid_from desc limit 1) where valid_to is null and exists(select 1 from delta.fee_versions d join version_map m on m.old=d.id where m.new=fee_versions.id and d.valid_to is not null and not exists(select 1 from delta.fee_versions n where n.contract_id=d.contract_id and n.valid_from>d.valid_from));")?;
    report["through"] = Value::from(updated_through);
    let assumed = tx.prepare("select c.symbol,p.provenance_json from contracts c join rebuild_contract_provenance p on p.contract_id=c.id order by c.symbol")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut assumed_symbols = Vec::new();
    for (symbol, body) in assumed {
        let parsed: Value = serde_json::from_str(&body)?;
        if parsed["official_fields"]["expiry_date"]["calendar_assumption"].is_object() {
            assumed_symbols.push(Value::from(symbol));
        }
    }
    report["assumed_expiry_symbols"] = Value::from(assumed_symbols.len());
    for (key, table) in [
        ("contracts", "contracts"),
        ("specification_versions", "contract_spec_versions"),
        ("observations", "official_observations"),
        ("fee_versions", "fee_versions"),
    ] {
        let count: i64 =
            tx.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))?;
        report[key] = Value::from(count);
    }
    report["observed_contracts"] = Value::from(tx.query_row(
        "select count(distinct contract_id) from official_observations",
        [],
        |r| r.get::<_, i64>(0),
    )?);
    let per_exchange=tx.prepare("select substr(c.symbol,1,instr(c.symbol,'.')-1),count(*) from official_observations o join contracts c on c.id=o.contract_id group by 1")?.query_map([],|r|Ok((r.get::<_,String>(0)?,Value::from(r.get::<_,i64>(1)?))))?.collect::<rusqlite::Result<serde_json::Map<_,_>>>()?;
    report["per_exchange"] = Value::Object(per_exchange);
    tx.execute(
        "update rebuild_summary set report_json=?1",
        [serde_json::to_string(&report)?],
    )?;
    validate_symbol_coverage(&tx, &trading_days, &old_through)?;
    compact_storage::normalize(&tx)?;
    tx.commit()?;
    conn.execute_batch("detach database delta;")?;
    Ok(report)
}

fn same_state(conn: &Connection, current: i64, incoming: i64) -> Result<bool> {
    let mut conditions = vec![
        "a.buy_margin_rate is b.buy_margin_rate".to_owned(),
        "a.sell_margin_rate is b.sell_margin_rate".to_owned(),
    ];
    for fee in [
        "open_fee_json",
        "close_yesterday_fee_json",
        "close_today_fee_json",
        "intraday_open_fee_json",
    ] {
        conditions.push(format!("(a.{fee} is null)=(b.{fee} is null)"));
        for field in ["kind", "value"] {
            conditions.push(format!(
                "json_extract(a.{fee},'$.{field}') is json_extract(b.{fee},'$.{field}')"
            ));
        }
    }
    Ok(conn.query_row(
        &format!(
            "select {} from fee_versions a,delta.fee_versions b where a.id=?1 and b.id=?2",
            conditions.join(" and ")
        ),
        params![current, incoming],
        |r| r.get(0),
    )?)
}

fn copy_metadata(conn: &Connection) -> Result<()> {
    conn.execute_batch("insert or ignore into contract_lifecycle_evidence select m.new,e.listing_date,e.expiry_date,e.canonical_url,e.body_sha256,e.recorded_at,e.provenance_kind from delta.contract_lifecycle_evidence e join contract_map m on m.old=e.contract_id;")?;
    let specs = conn.prepare("select s.id,m.new,s.lot_size,s.tick_size,s.valid_from,s.valid_to from delta.contract_spec_versions s join contract_map m on m.old=s.contract_id order by s.valid_from")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,f64>(2)?,r.get::<_,f64>(3)?,r.get::<_,String>(4)?,r.get::<_,Option<String>>(5)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (old, contract, lot, tick, from, to) in specs {
        let existing:Option<(i64,f64,f64,Option<String>)>=conn.query_row("select id,lot_size,tick_size,valid_to from contract_spec_versions where contract_id=?1 and valid_from=?2",params![contract,from],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
        if let Some((_id, a, b, end)) = existing {
            ensure!(
                (a, b, end) == (lot, tick, to),
                "changed historical specification: contract {contract}"
            );
        } else {
            let overlaps:bool=conn.query_row("select exists(select 1 from contract_spec_versions where contract_id=?1 and julianday(coalesce(valid_to,'9999-12-31'))>julianday(?2) and julianday(coalesce(?3,'9999-12-31'))>julianday(valid_from))",params![contract,from,to],|r|r.get(0))?;
            ensure!(
                !overlaps,
                "overlapping new specification: contract {contract}"
            );
            conn.execute("insert into contract_spec_versions(contract_id,lot_size,tick_size,valid_from,valid_to,source_kind,source_url,first_seen_at,last_seen_at) select ?1,lot_size,tick_size,valid_from,valid_to,source_kind,source_url,first_seen_at,last_seen_at from delta.contract_spec_versions where id=?2",params![contract,old])?;
            conn.execute("insert into rebuild_spec_provenance select ?1,source_json from delta.rebuild_spec_provenance where spec_id=?2",params![conn.last_insert_rowid(),old])?;
        }
    }
    conn.execute_batch("insert or ignore into contract_spec_evidence select m.new,e.valid_from,e.canonical_url,e.body_sha256,e.recorded_at from delta.contract_spec_evidence e join contract_map m on m.old=e.contract_id;")?;
    Ok(())
}

/// Validate actual retained bytes and the per-exchange trading-day coverage.
fn validate_delta(conn: &Connection, first: &str, through: &str) -> Result<BTreeSet<String>> {
    use quick_xml::{Reader, events::Event};
    let format = format_description::parse("[year]-[month]-[day]")?;
    let mut holidays = BTreeSet::new();
    let mut months = BTreeSet::new();
    let documents = conn.prepare("select exchange,trading_date,body_sha256,retained_path from delta.retained_parameter_documents")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,String>(2)?,r.get::<_,String>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (exchange, _day, sha, path) in documents {
        let body =
            std::fs::read(&path).with_context(|| format!("read delta retained document {path}"))?;
        ensure!(
            hex::encode(Sha256::digest(&body)) == sha,
            "delta retained SHA-256 mismatch: {path}"
        );
        if exchange != "CALENDAR" {
            continue;
        }
        let mut reader = Reader::from_reader(body.as_slice());
        let mut date = String::new();
        let mut title = String::new();
        loop {
            match reader.read_event()? {
                Event::Start(e) if e.name().as_ref() == b"doc" => {
                    date.clear();
                    title.clear();
                }
                Event::Start(e) if e.name().as_ref() == b"pubdate" => {
                    xml_text(&mut reader, b"pubdate")?
                        .trim()
                        .clone_into(&mut date);
                }
                Event::Start(e) if e.name().as_ref() == b"title" => {
                    xml_text(&mut reader, b"title")?
                        .trim()
                        .clone_into(&mut title);
                }
                Event::End(e) if e.name().as_ref() == b"doc" => {
                    let parsed =
                        Date::parse(&date, &format).context("invalid calendar event date")?;
                    months.insert((parsed.year(), parsed.month()));
                    if title == "假日休市" || title == "休市" {
                        holidays.insert(parsed);
                    }
                }
                Event::Eof => break,
                _ => {}
            }
        }
    }
    let mut day = Date::parse(first, &format)?;
    let end = Date::parse(through, &format)?;
    let mut expected = BTreeSet::new();
    while day <= end {
        ensure!(
            months.contains(&(day.year(), day.month())),
            "delta lacks calendar provenance for {}-{:02}",
            day.year(),
            u8::from(day.month())
        );
        if !matches!(
            day.weekday(),
            time::Weekday::Saturday | time::Weekday::Sunday
        ) && !holidays.contains(&day)
        {
            expected.insert(day.to_string());
        }
        day = day.next_day().context("date overflow")?;
    }
    ensure!(
        !expected.is_empty(),
        "delta interval contains no official trading days"
    );
    for exchange in ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"] {
        let observed=conn.prepare("select distinct o.trade_date from delta.official_observations o join delta.contracts c on c.id=o.contract_id where c.symbol like ?1")?
            .query_map([format!("{exchange}.%")],|r|r.get::<_,String>(0))?.collect::<rusqlite::Result<BTreeSet<_>>>()?;
        ensure!(
            observed == expected,
            "delta trading-day coverage differs from official calendar: {exchange}"
        );
    }
    validate_metadata(conn)?;
    let invalid:i64=conn.query_row("select count(*) from delta.official_observations o join delta.contracts c on c.id=o.contract_id join delta.retained_parameter_documents f on f.id=o.fee_document_id join delta.retained_parameter_documents m on m.id=o.margin_document_id join delta.rebuild_inputs i on i.id=o.input_id where length(i.sha256)!=64 or i.path='' or o.row_number<2 or f.exchange != case when c.symbol like 'INE.%' and o.trade_date>='2018-10-24' then 'SHFE' else substr(c.symbol,1,instr(c.symbol,'.')-1) end or m.exchange != substr(c.symbol,1,instr(c.symbol,'.')-1) or f.trading_date is null or m.trading_date is null or (c.symbol like 'CFFEX.%' and (f.trading_date>o.trade_date or m.trading_date>o.trade_date)) or (c.symbol not like 'CFFEX.%' and (f.trading_date!=o.trade_date or m.trading_date!=o.trade_date))",[],|r|r.get(0))?;
    ensure!(
        invalid == 0,
        "delta observation has invalid original source provenance: {invalid}"
    );
    Ok(expected)
}

fn validate_symbol_coverage(
    conn: &Connection,
    trading_days: &BTreeSet<String>,
    seed_through: &str,
) -> Result<()> {
    conn.execute_batch(
        "create temp table appended_trading_days(day text primary key) without rowid;",
    )?;
    for day in trading_days.range((
        std::ops::Bound::Excluded(seed_through.to_owned()),
        std::ops::Bound::Unbounded,
    )) {
        conn.execute("insert into appended_trading_days values(?1)", [day])?;
    }
    let missing:Option<(String,String)>=conn.query_row(
        "select c.symbol,d.day from contracts c
         join (select contract_id,min(trade_date) first_observed from official_observations group by contract_id) f on f.contract_id=c.id
         join appended_trading_days d on d.day>=f.first_observed
          and replace(d.day,'-','')>=replace(c.listing_date,'-','')
          and replace(d.day,'-','')<=replace(c.expiry_date,'-','')
         where not exists(select 1 from official_observations o where o.contract_id=c.id and o.trade_date=d.day)
         order by d.day,c.symbol limit 1",[],|r|Ok((r.get(0)?,r.get(1)?))).optional()?;
    ensure!(
        missing.is_none(),
        "missing official observation for known contract: {} {}",
        missing.as_ref().map_or("", |v| v.0.as_str()),
        missing.as_ref().map_or("", |v| v.1.as_str())
    );
    Ok(())
}

fn validate_metadata(conn: &Connection) -> Result<()> {
    let provenance = conn.prepare("select c.symbol,c.listing_date,c.expiry_date,p.provenance_json from delta.contracts c left join delta.rebuild_contract_provenance p on p.contract_id=c.id")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,String>(2)?,r.get::<_,Option<String>>(3)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (symbol, listing, expiry, body) in provenance {
        let body: Value = serde_json::from_str(
            body.as_deref()
                .with_context(|| format!("missing contract provenance: {symbol}"))?,
        )?;
        for (field, expected) in [("listing_date", listing), ("expiry_date", expiry)] {
            let item = &body["official_fields"][field];
            ensure!(
                item["value"]
                    .as_str()
                    .is_some_and(|v| v.replace('-', "") == expected.replace('-', ""))
                    && item["evidence_id"]
                        .as_str()
                        .is_some_and(|v| hex::decode(v).is_ok_and(|b| b.len() == 32)),
                "incomplete official lifecycle provenance: {symbol} {field}"
            );
        }
    }
    let evidence=conn.prepare("select canonical_url,body_sha256,0 from delta.contract_lifecycle_evidence union select canonical_url,body_sha256,0 from delta.contract_spec_evidence union select canonical_url,body_sha256,1 from delta.fee_version_evidence")?
        .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?,r.get::<_,bool>(2)?)))?.collect::<rusqlite::Result<Vec<_>>>()?;
    for (url, sha, fee) in evidence {
        ensure!(
            hex::decode(&sha).is_ok_and(|b| b.len() == 32),
            "invalid retained evidence digest"
        );
        ensure!(
            if fee {
                valid_parameter_url(&url)
            } else {
                valid_metadata_url(&url)
            },
            "nonofficial metadata evidence URL: {url}"
        );
    }
    let missing_specs:i64=conn.query_row("select count(*) from delta.official_observations o where not exists(select 1 from delta.contract_spec_versions s join delta.rebuild_spec_provenance p on p.spec_id=s.id join delta.contract_spec_evidence e on e.contract_id=s.contract_id and e.valid_from=s.valid_from where s.contract_id=o.contract_id and s.source_kind='official' and julianday(o.trade_date)>=julianday(substr(s.valid_from,1,10)) and (s.valid_to is null or julianday(o.trade_date)<julianday(substr(s.valid_to,1,10))))",[],|r|r.get(0))?;
    ensure!(
        missing_specs == 0,
        "observations lack complete official specification provenance: {missing_specs}"
    );
    Ok(())
}

fn xml_text(reader: &mut quick_xml::Reader<&[u8]>, tag: &[u8]) -> Result<String> {
    use quick_xml::events::Event;
    let mut text = String::new();
    loop {
        match reader.read_event()? {
            Event::Text(value) => text.push_str(&quick_xml::escape::unescape(&value.decode()?)?),
            Event::CData(value) => text.push_str(&value.decode()?),
            Event::End(value) if value.name().as_ref() == tag => return Ok(text),
            Event::Eof => anyhow::bail!("unterminated calendar element"),
            _ => {}
        }
    }
}

// Metadata collectors retain historical HTTP originals and both official SHFE
// domains. Parameter documents and fee evidence keep their stricter validator.
fn valid_metadata_url(value: &str) -> bool {
    const DOMAINS: [&str; 7] = [
        "shfe.com.cn",
        "shfe.cn",
        "ine.cn",
        "dce.com.cn",
        "czce.com.cn",
        "gfex.com.cn",
        "cffex.com.cn",
    ];
    reqwest::Url::parse(value).is_ok_and(|url| {
        let Some(host) = url.host_str() else {
            return false;
        };
        let host = host.strip_prefix("www.").unwrap_or(host);
        matches!(url.scheme(), "http" | "https")
            && DOMAINS.contains(&host)
            && url.username().is_empty()
            && url.password().is_none()
            && url.port().is_none()
    })
}

fn valid_parameter_url(value: &str) -> bool {
    ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"]
        .iter()
        .any(|exchange| crate::official::validate_official_canonical_url(exchange, value).is_ok())
}
