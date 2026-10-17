//! Fresh, offline rebuild from reviewed collector outputs. Never opens a seed database.

use crate::{
    db,
    hash::{row_rule_hash, row_state_hash},
    official::validate_official_canonical_url,
    parse::AllowedRow,
};
use anyhow::{Context, Result, anyhow, bail, ensure};
use future_meta::model::{FeeKind, FeeSpec, TradingStatus};
use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};
use time::{Date, OffsetDateTime, UtcOffset, format_description::well_known::Rfc3339};

type Row = BTreeMap<String, String>;

/// Only collector outputs and retained source documents are accepted as input.
pub struct RebuildOptions {
    pub metadata_dir: PathBuf,
    pub settle_dir: PathBuf,
    pub settlement_sources: PathBuf,
    pub out: PathBuf,
}

#[derive(Deserialize)]
struct Sources {
    from: String,
    through: String,
    csv_files: BTreeMap<String, String>,
    documents: Vec<Document>,
}

#[derive(Deserialize)]
struct Document {
    exchange: String,
    date: Option<String>,
    canonical_url: String,
    sha256: String,
    raw_path: PathBuf,
}

struct ContractInfo {
    id: i64,
    listing: String,
    expiry: String,
    lot: f64,
    tick: f64,
}

struct State {
    hash: String,
    version_id: i64,
    last_day: String,
}

/// Construct a new database and publish it only after all invariants hold.
///
/// # Errors
/// Fails on existing output, changed retained bytes, missing provenance, invalid
/// rows, duplicate observations, or incomplete specification coverage.
pub fn rebuild(options: &RebuildOptions) -> Result<Value> {
    ensure!(
        !options.out.exists(),
        "output already exists: {}",
        options.out.display()
    );
    let parent = options
        .out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".rebuild-{}.sqlite", std::process::id()));
    ensure!(!temporary.exists(), "temporary output already exists");
    let result = build(options, &temporary);
    match result {
        Ok(report) => {
            std::fs::rename(&temporary, &options.out)?;
            Ok(report)
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error)
        }
    }
}

#[allow(clippy::too_many_lines)]
fn build(options: &RebuildOptions, temporary: &Path) -> Result<Value> {
    let provenance: Value = serde_json::from_slice(&std::fs::read(
        options.metadata_dir.join("contracts_meta_provenance.json"),
    )?)?;
    let evidence = provenance["evidence"]
        .as_object()
        .context("missing metadata evidence")?;
    for (digest, entry) in evidence {
        verify_file(
            Path::new(
                entry["raw_path"]
                    .as_str()
                    .context("missing retained path")?,
            ),
            digest,
        )?;
    }
    let sources: Sources = serde_json::from_slice(&std::fs::read(&options.settlement_sources)?)?;
    let first = parse_date(&sources.from)?;
    let through = parse_date(&sources.through)?;
    ensure!(first <= through, "reversed source coverage");
    for (name, digest) in &sources.csv_files {
        verify_file(&options.settle_dir.join(name), digest)?;
    }
    for document in &sources.documents {
        let exchange = if document.exchange == "CALENDAR" {
            "CFFEX"
        } else {
            &document.exchange
        };
        validate_official_canonical_url(exchange, &document.canonical_url)?;
        verify_file(&document.raw_path, &document.sha256)?;
    }
    eprintln!(
        "Verified {} retained parameter documents",
        sources.documents.len()
    );
    let mut conn = db::connect(temporary)?;
    db::ensure_schema(&conn)?;
    conn.execute_batch("pragma journal_mode=delete; pragma synchronous=full;")?;
    let tx = conn.transaction()?;
    tx.execute_batch("create table rebuild_inputs(id integer primary key, path text not null, sha256 text not null);
        create table rebuild_contract_provenance(contract_id integer primary key references contracts(id), provenance_json text not null check(json_valid(provenance_json)));
        create table rebuild_spec_provenance(spec_id integer primary key references contract_spec_versions(id), source_json text not null check(json_valid(source_json)));
        create table retained_parameter_documents(id integer primary key, exchange text not null, trading_date text, canonical_url text not null, body_sha256 text not null, retained_path text not null);
        create table official_observations(contract_id integer not null references contracts(id), trade_date text not null,
          version_id integer not null references fee_versions(id), input_id integer not null references rebuild_inputs(id),
          row_number integer not null, fee_document_id integer not null references retained_parameter_documents(id),
          margin_document_id integer not null references retained_parameter_documents(id),
          primary key(contract_id,trade_date)) without rowid;
        create table rebuild_summary(report_json text not null check(json_valid(report_json)));")?;
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let mut contracts = HashMap::new();
    let metadata_path = options.metadata_dir.join("contracts_meta.csv");
    retain_input(&tx, &metadata_path)?;
    for result in csv::Reader::from_path(&metadata_path)?.deserialize::<Row>() {
        let row = result?;
        let symbol = required(&row, "full_symbol")?;
        let parsed = future_meta::symbol::parse_symbol(symbol)?;
        ensure!(
            parsed.kind == future_meta::symbol::SymbolKind::Futures,
            "not concrete futures: {symbol}"
        );
        let listing = required(&row, "listing_date")?;
        let expiry = required(&row, "expiry_date")?;
        ensure!(
            parse_date(listing)? <= parse_date(expiry)?,
            "reversed lifecycle: {symbol}"
        );
        let info = &provenance["contracts"][symbol];
        ensure!(info.is_object(), "missing contract provenance: {symbol}");
        let assumed = info["official_fields"]["expiry_date"]["calendar_assumption"].is_object();
        if assumed {
            ensure!(
                info["official_fields"]["expiry_date"]["source"] == "user_assumed_holiday_calendar",
                "unlabelled assumed expiry"
            );
        }
        for field in ["listing_date", "expiry_date"] {
            let item = &info["official_fields"][field];
            ensure!(
                item["value"].as_str() == row.get(field).map(String::as_str),
                "metadata value differs from provenance: {symbol} {field}"
            );
            ensure!(
                evidence.contains_key(
                    item["evidence_id"]
                        .as_str()
                        .context("missing lifecycle evidence")?
                ),
                "unknown lifecycle evidence"
            );
        }
        let lot = number(&row, "lot_size")?.context("missing lot")?;
        let tick = number(&row, "tick_size")?.context("missing tick")?;
        ensure!(lot > 0.0 && tick > 0.0, "nonpositive specification");
        tx.execute("insert into contracts(symbol,listing_date,expiry_date,lot_size,tick_size,first_seen_at,last_seen_at,active) values(?1,?2,?3,?4,?5,?6,?6,0)", params![symbol, listing.replace('-',""), expiry.replace('-',""),lot,tick,now])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "insert into rebuild_contract_provenance values(?1,?2)",
            params![id, serde_json::to_string(info)?],
        )?;
        let entry = &evidence[info["official_fields"]["listing_date"]["evidence_id"]
            .as_str()
            .unwrap()];
        tx.execute("insert into contract_lifecycle_evidence(contract_id,listing_date,expiry_date,canonical_url,body_sha256,recorded_at,provenance_kind) values(?1,?2,?3,?4,?5,?6,?7)", params![id,listing.replace('-',""),expiry.replace('-',""),entry["urls"][0].as_str().unwrap(),info["official_fields"]["listing_date"]["evidence_id"].as_str().unwrap(),now, if assumed { "user_assumed_holiday_calendar" } else { "official" }])?;
        ensure!(
            contracts
                .insert(
                    symbol.to_owned(),
                    ContractInfo {
                        id,
                        listing: listing.to_owned(),
                        expiry: expiry.to_owned(),
                        lot,
                        tick
                    }
                )
                .is_none(),
            "duplicate metadata symbol"
        );
    }
    let specs_path = options.metadata_dir.join("contract_spec_versions.csv");
    retain_input(&tx, &specs_path)?;
    let mut specs_count = 0usize;
    for result in csv::Reader::from_path(specs_path)?.deserialize::<Row>() {
        let row = result?;
        let symbol = required(&row, "full_symbol")?;
        let contract = contracts
            .get(symbol)
            .context("specification has no metadata")?;
        let from = spec_boundary(&row, "valid_from")?;
        let to = spec_boundary(&row, "valid_to")?;
        ensure!(
            OffsetDateTime::parse(&from, &Rfc3339)? < OffsetDateTime::parse(&to, &Rfc3339)?,
            "invalid specification range: {symbol}"
        );
        let ids = required(&row, "evidence_ids")?
            .split(';')
            .collect::<Vec<_>>();
        ensure!(
            ids.iter().all(|id| evidence.contains_key(*id)),
            "unretained specification evidence"
        );
        let source = &evidence[ids[0]];
        tx.execute("insert into contract_spec_versions(contract_id,lot_size,tick_size,valid_from,valid_to,source_kind,source_url,first_seen_at,last_seen_at) values(?1,?2,?3,?4,?5,'official',?6,?7,?7)", params![contract.id,number(&row,"lot_size")?,number(&row,"tick_size")?,from,to,source["urls"][0].as_str().unwrap(),now])?;
        let id = tx.last_insert_rowid();
        tx.execute(
            "insert into rebuild_spec_provenance values(?1,?2)",
            params![id, serde_json::to_string(&row)?],
        )?;
        for digest in ids {
            tx.execute(
                "insert into contract_spec_evidence values(?1,?2,?3,?4,?5)",
                params![
                    contract.id,
                    from,
                    evidence[digest]["urls"][0].as_str().unwrap(),
                    digest,
                    now
                ],
            )?;
        }
        specs_count += 1;
    }
    let mut documents: BTreeMap<(String, String), Vec<i64>> = BTreeMap::new();
    for document in &sources.documents {
        tx.execute("insert into retained_parameter_documents(exchange,trading_date,canonical_url,body_sha256,retained_path) values(?1,?2,?3,?4,?5)",params![document.exchange,document.date,document.canonical_url,document.sha256,document.raw_path.to_str()])?;
        if let Some(day) = &document.date {
            documents
                .entry((document.exchange.clone(), day.clone()))
                .or_default()
                .push(tx.last_insert_rowid());
        }
    }
    let mut states: HashMap<i64, State> = HashMap::new();
    let mut observations = 0usize;
    let mut versions = 0usize;
    let mut per_exchange = BTreeMap::<String, usize>::new();
    for exchange in ["CFFEX", "CZCE", "DCE", "GFEX", "INE", "SHFE"] {
        let name = format!(
            "{}_settle_params{}.csv",
            exchange.to_lowercase(),
            if exchange == "CFFEX" { "_daily" } else { "" }
        );
        ensure!(
            sources.csv_files.contains_key(&name),
            "unverified CSV {name}"
        );
        let path = options.settle_dir.join(name);
        let input_id = retain_input(&tx, &path)?;
        for (index, result) in csv::Reader::from_path(path)?
            .deserialize::<Row>()
            .enumerate()
        {
            let row = result?;
            ensure!(
                required(&row, "exchange")? == exchange,
                "CSV exchange mismatch"
            );
            let local = required(&row, "symbol")?;
            let symbol = format!("{exchange}.{local}");
            let parsed = future_meta::symbol::parse_symbol(&symbol)?;
            ensure!(
                parsed.kind == future_meta::symbol::SymbolKind::Futures,
                "non-futures parameter row"
            );
            let info = contracts
                .get(&symbol)
                .with_context(|| format!("parameter row without metadata: {symbol}"))?;
            let day = required(&row, "date")?;
            let date = parse_date(day)?;
            ensure!(
                date >= first && date <= through,
                "observation outside source coverage"
            );
            ensure!(
                day >= info.listing.as_str() && day <= info.expiry.as_str(),
                "observation outside lifecycle: {symbol} {day}"
            );
            let fee_doc = find_document(
                &documents,
                if exchange == "INE" { "SHFE" } else { exchange },
                day,
                exchange == "CFFEX",
            )?;
            let margin_doc = if exchange == "INE" {
                find_document(&documents, "INE", day, false)?
            } else {
                fee_doc
            };
            let from = midnight(date)?;
            let allowed = AllowedRow {
                symbol: symbol.clone(),
                listing_date: Some(info.listing.clone()),
                expiry_date: Some(info.expiry.clone()),
                trading_status: TradingStatus::Unknown,
                buy_margin_rate: number(&row, "long_margin_ratio")?,
                sell_margin_rate: number(&row, "short_margin_ratio")?,
                open_fee: fee(&row, "open_fee")?,
                close_yesterday_fee: fee(&row, "offset_fee")?,
                close_today_fee: fee(&row, "close_today_fee")?,
                intraday_open_fee: number(&row, "intraday_open_fee")?
                    .map(|_| fee(&row, "intraday_open_fee"))
                    .transpose()?,
                lot_size: info.lot,
                tick_size: info.tick,
                source_updated_at: None,
                is_main_contract: false,
            };
            let hash = row_rule_hash(&allowed);
            let state_hash = row_state_hash(&allowed);
            if let Some(previous) = states.get(&info.id) {
                ensure!(
                    day > previous.last_day.as_str(),
                    "duplicate or unordered observation: {symbol} {day}"
                );
            }
            let changed = states.get(&info.id).is_none_or(|s| s.hash != state_hash);
            if changed {
                if let Some(previous) = states.get(&info.id) {
                    tx.execute(
                        "update fee_versions set valid_to=?1,last_seen_at=?2 where id=?3",
                        params![
                            from,
                            midnight(parse_date(&previous.last_day)?)?,
                            previous.version_id
                        ],
                    )?;
                }
                tx.execute("insert into fee_versions(contract_id,rule_hash,buy_margin_rate,sell_margin_rate,open_fee_json,close_yesterday_fee_json,close_today_fee_json,intraday_open_fee_json,trading_status,is_main_contract,source_kind,source_updated_at,valid_from,valid_to,first_seen_at,last_seen_at) values(?1,?2,?3,?4,?5,?6,?7,?8,'Unknown',0,'official',null,?9,null,?9,?9)",params![info.id,hash,allowed.buy_margin_rate,allowed.sell_margin_rate,serde_json::to_string(&allowed.open_fee)?,serde_json::to_string(&allowed.close_yesterday_fee)?,serde_json::to_string(&allowed.close_today_fee)?,allowed.intraday_open_fee.as_ref().map(serde_json::to_string).transpose()?,from])?;
                let version_id = tx.last_insert_rowid();
                for document in [fee_doc, margin_doc] {
                    tx.execute("insert or ignore into fee_version_evidence select ?1,?2,?3,'official_parameter',canonical_url,body_sha256,?4 from retained_parameter_documents where id=?5",params![info.id,from,hash,now,document])?;
                }
                states.insert(
                    info.id,
                    State {
                        hash: state_hash,
                        version_id,
                        last_day: day.to_owned(),
                    },
                );
                versions += 1;
            } else {
                day.clone_into(&mut states.get_mut(&info.id).unwrap().last_day);
            }
            let state = &states[&info.id];
            tx.prepare_cached("insert into official_observations values(?1,?2,?3,?4,?5,?6,?7)")?
                .execute(params![
                    info.id,
                    day,
                    state.version_id,
                    input_id,
                    index + 2,
                    fee_doc,
                    margin_doc
                ])?;
            observations += 1;
            *per_exchange.entry(exchange.to_owned()).or_default() += 1;
        }
    }
    for info in contracts.values() {
        if let Some(state) = states.get(&info.id) {
            let expiry = parse_date(&info.expiry)?;
            let end = if expiry <= through {
                Some(midnight(expiry.next_day().context("expiry overflow")?)?)
            } else {
                None
            };
            tx.execute(
                "update fee_versions set valid_to=?1,last_seen_at=?2 where id=?3",
                params![
                    end,
                    midnight(parse_date(&state.last_day)?)?,
                    state.version_id
                ],
            )?;
            tx.execute(
                "update contracts set active=?1 where id=?2",
                params![state.last_day == sources.through, info.id],
            )?;
        }
    }
    let report = serde_json::json!({"contracts":contracts.len(),"specification_versions":specs_count,"observations":observations,"fee_versions":versions,"observed_contracts":states.len(),"per_exchange":per_exchange,"from":sources.from,"through":sources.through,"assumed_expiry_symbols":provenance["summary"]["assumed_expiry_symbols"],"source":"fresh_collector_outputs_no_seed"});
    tx.execute(
        "insert into rebuild_summary values(?1)",
        params![serde_json::to_string(&report)?],
    )?;
    tx.commit()?;
    db::validate_fee_history_integrity(&conn)?;
    let missing:i64=conn.query_row("select count(*) from official_observations o join fee_versions v on v.id=o.version_id where julianday(o.trade_date)<julianday(substr(v.valid_from,1,10)) or julianday(o.trade_date)>=julianday(substr(v.valid_to,1,10))",[],|r|r.get(0))?;
    ensure!(missing == 0, "observations outside persisted fee intervals");
    let integrity: String = conn.query_row("pragma integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "SQLite integrity check: {integrity}");
    Ok(report)
}

/// Validate the retained collector lineage and observed history without a legacy seed.
/// Missing observations before a contract's first source row remain explicit unknowns.
///
/// # Errors
/// Returns an error on missing lineage, broken evidence, intervals, or foreign keys.
pub fn verify(path: &Path) -> Result<Value> {
    let conn = db::connect_readonly(path)?;
    let summary: String =
        conn.query_row("select report_json from rebuild_summary", [], |r| r.get(0))?;
    let summary: Value = serde_json::from_str(&summary)?;
    ensure!(
        summary["source"] == "fresh_collector_outputs_no_seed",
        "not a collected-only seed"
    );
    db::validate_fee_history_integrity(&conn)?;
    let integrity: String = conn.query_row("pragma integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "SQLite integrity check: {integrity}");
    ensure!(
        !conn.prepare("pragma foreign_key_check")?.exists([])?,
        "foreign key violation"
    );
    let observations: i64 =
        conn.query_row("select count(*) from official_observations", [], |r| {
            r.get(0)
        })?;
    ensure!(
        Some(observations) == summary["observations"].as_i64(),
        "observation count differs from rebuild"
    );
    let invalid: i64 = conn.query_row(
        "select count(*) from official_observations o join fee_versions v on v.id=o.version_id
         where o.contract_id != v.contract_id or julianday(o.trade_date)<julianday(substr(v.valid_from,1,10))
         or julianday(o.trade_date)>=julianday(substr(v.valid_to,1,10))
         or v.buy_margin_rate is null or v.sell_margin_rate is null", [], |r| r.get(0))?;
    ensure!(invalid == 0, "invalid observed parameter states: {invalid}");
    let nonofficial: i64 = conn.query_row(
        "select count(*) from fee_versions where source_kind != 'official'",
        [],
        |r| r.get(0),
    )?;
    ensure!(
        nonofficial == 0,
        "nonofficial parameter versions: {nonofficial}"
    );
    let mut statement =
        conn.prepare("select exchange,canonical_url from retained_parameter_documents")?;
    for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
        let (exchange, url) = row?;
        validate_official_canonical_url(
            if exchange == "CALENDAR" {
                "CFFEX"
            } else {
                &exchange
            },
            &url,
        )?;
    }
    let (from, through): (String, String) = conn.query_row(
        "select min(trade_date),max(trade_date) from official_observations",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    let mut gaps = conn.prepare(
        "select c.symbol,c.listing_date,min(o.trade_date) from contracts c join official_observations o on o.contract_id=c.id
         group by c.id having replace(min(o.trade_date),'-','') > max(c.listing_date,replace(?1,'-','')) order by c.symbol")?;
    let prefixes = gaps.query_map([&from], |r| Ok(serde_json::json!({"symbol":r.get::<_,String>(0)?,"listing_date":r.get::<_,String>(1)?,"first_observation":r.get::<_,String>(2)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let unobserved: i64 = conn.query_row("select count(*) from contracts c where not exists(select 1 from official_observations o where o.contract_id=c.id)", [], |r| r.get(0))?;
    Ok(
        serde_json::json!({"source":summary["source"],"observations":observations,"observed_from":from,"observed_through":through,"contracts_without_observations":unobserved,"unobserved_prefixes":prefixes,"assumed_expiry_symbols":summary["assumed_expiry_symbols"],"integrity":"ok"}),
    )
}

fn required<'a>(row: &'a Row, key: &str) -> Result<&'a str> {
    row.get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .with_context(|| format!("missing {key}"))
}

fn number(row: &Row, key: &str) -> Result<Option<f64>> {
    let Some(value) = row.get(key).filter(|v| !v.is_empty()) else {
        return Ok(None);
    };
    let value: f64 = value
        .parse()
        .with_context(|| format!("invalid {key}: {value}"))?;
    ensure!(value.is_finite() && value >= 0.0, "invalid {key}: {value}");
    Ok(Some(value))
}

fn fee(row: &Row, key: &str) -> Result<FeeSpec> {
    let kind = match required(row, "fee_type")? {
        "cny_per_lot" => FeeKind::CnyPerLot,
        "rate_per_10k" => FeeKind::TurnoverRatePerTenThousand,
        other => bail!("unsupported fee type {other}"),
    };
    let value = number(row, key)?.with_context(|| format!("missing fee {key}"))?;
    Ok(FeeSpec {
        kind,
        value: Some(value),
        raw_text: None,
    })
}

fn parse_date(value: &str) -> Result<Date> {
    Date::parse(
        value,
        &time::format_description::parse("[year]-[month]-[day]")?,
    )
    .map_err(Into::into)
}

fn midnight(value: Date) -> Result<String> {
    value
        .midnight()
        .assume_offset(UtcOffset::from_hms(8, 0, 0)?)
        .format(&Rfc3339)
        .map_err(Into::into)
}

fn spec_boundary(row: &Row, edge: &str) -> Result<String> {
    let phase = required(row, &format!("{edge}_phase"))?;
    let original = required(row, edge)?;
    if phase == "instant" {
        OffsetDateTime::parse(original, &Rfc3339)?;
        return Ok(original.to_owned());
    }
    let day = if phase == "settlement" {
        required(row, &format!("{edge}_trade_date"))?
    } else {
        original
    };
    midnight(parse_date(day)?)
}

fn verify_file(path: &Path, digest: &str) -> Result<()> {
    let actual = hex::encode(Sha256::digest(
        std::fs::read(path).with_context(|| format!("read {}", path.display()))?,
    ));
    ensure!(
        actual == digest,
        "retained SHA-256 mismatch: {}",
        path.display()
    );
    Ok(())
}

fn retain_input(conn: &Connection, path: &Path) -> Result<i64> {
    let sha = hex::encode(Sha256::digest(std::fs::read(path)?));
    conn.execute(
        "insert into rebuild_inputs(path,sha256) values(?1,?2)",
        params![path.to_str(), sha],
    )?;
    Ok(conn.last_insert_rowid())
}

fn find_document(
    docs: &BTreeMap<(String, String), Vec<i64>>,
    exchange: &str,
    day: &str,
    forward_fill: bool,
) -> Result<i64> {
    if !forward_fill {
        return docs
            .get(&(exchange.to_owned(), day.to_owned()))
            .and_then(|ids| ids.first())
            .copied()
            .with_context(|| format!("missing source document {exchange} {day}"));
    }
    docs.range((exchange.to_owned(), String::new())..=(exchange.to_owned(), day.to_owned()))
        .next_back()
        .and_then(|(_, ids)| ids.first())
        .copied()
        .ok_or_else(|| anyhow!("missing baseline source {exchange} {day}"))
}
