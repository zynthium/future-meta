//! Archive and manifest export.

use anyhow::Result;
use future_meta::archive::{encode_archive_bytes, sha256_hex};
use future_meta::model::{
    Contract, ContractFee, FeeArchiveV1, FeeSpec, Manifest, SCHEMA_VERSION, TradingStatus,
};
use rusqlite::{Connection, Row};
use std::path::Path;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Export an archive from the database.
///
/// # Errors
///
/// Returns an error if archive export fails.
pub fn export_archive(db: &Path, out: &Path) -> Result<()> {
    std::fs::create_dir_all(out.join("artifacts"))?;
    let conn = Connection::open(db)?;
    let archive = load_archive(&conn)?;
    let bytes = encode_archive_bytes(&archive)?;
    let sha = sha256_hex(&bytes);
    let data_version = archive
        .generated_at
        .replace([':', '+'], "")
        .replace('-', "");
    let artifact_name = format!("artifacts/future-meta-fees-v1-{data_version}.fmeta.zst");

    std::fs::write(out.join("latest.fmeta.zst"), &bytes)?;
    std::fs::write(out.join(&artifact_name), &bytes)?;

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        data_version: archive.generated_at.clone(),
        generated_at: archive.generated_at.clone(),
        history_start: archive.history_start.clone(),
        history_end: archive.history_end.clone(),
        artifact: "latest.fmeta.zst".to_owned(),
        sha256: sha,
        size: bytes.len() as u64,
        mirrors: vec!["https://future-meta.pages.dev/latest.fmeta.zst".to_owned()],
    };
    std::fs::write(
        out.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest)?,
    )?;
    Ok(())
}

fn load_archive(conn: &Connection) -> Result<FeeArchiveV1> {
    let mut contracts_stmt = conn.prepare(
        "select id, symbol, listing_date, expiry_date, lot_size, tick_size, active
         from contracts
         order by id",
    )?;
    let contracts = contracts_stmt
        .query_map([], |row| {
            Ok(Contract {
                id: read_u32(row, 0)?,
                symbol: row.get(1)?,
                listing_date: row.get(2)?,
                expiry_date: row.get(3)?,
                lot_size: row.get(4)?,
                tick_size: row.get(5)?,
                active: row.get::<_, i64>(6)? != 0,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut fee_stmt = conn.prepare(
        "select contract_id, rule_hash, buy_margin_rate, sell_margin_rate,
                open_fee_json, close_yesterday_fee_json, close_today_fee_json,
                trading_status, is_main_contract, source_updated_at,
                valid_from, valid_to
         from fee_versions
         order by contract_id, valid_from, id",
    )?;
    let fee_versions = fee_stmt
        .query_map([], |row| {
            let trading_status_text: String = row.get(7)?;
            let open_fee_json: String = row.get(4)?;
            let close_yesterday_fee_json: String = row.get(5)?;
            let close_today_fee_json: String = row.get(6)?;
            Ok(ContractFee {
                contract_id: read_u32(row, 0)?,
                rule_hash: row.get(1)?,
                buy_margin_rate: row.get(2)?,
                sell_margin_rate: row.get(3)?,
                open_fee: parse_fee_json(&open_fee_json)?,
                close_yesterday_fee: parse_fee_json(&close_yesterday_fee_json)?,
                close_today_fee: parse_fee_json(&close_today_fee_json)?,
                trading_status: parse_status(&trading_status_text, 7)?,
                is_main_contract: row.get::<_, i64>(8)? != 0,
                source_updated_at: row.get(9)?,
                valid_from: row.get(10)?,
                valid_to: row.get(11)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let generated_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
    let history_start = fee_versions
        .iter()
        .map(|version| version.valid_from.clone())
        .min()
        .unwrap_or_else(|| generated_at.clone());

    Ok(FeeArchiveV1 {
        schema_version: SCHEMA_VERSION,
        generated_at: generated_at.clone(),
        history_start,
        history_end: generated_at,
        contracts,
        fee_versions,
    })
}

fn parse_fee_json(json: &str) -> rusqlite::Result<FeeSpec> {
    serde_json::from_str(json).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
    })
}

fn read_u32(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let value = row.get::<_, i64>(index)?;
    u32::try_from(value).map_err(|err| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(err),
        )
    })
}

fn parse_status(text: &str, index: usize) -> rusqlite::Result<TradingStatus> {
    match text {
        "Trading" => Ok(TradingStatus::Trading),
        "NotTrading" => Ok(TradingStatus::NotTrading),
        "Unknown" => Ok(TradingStatus::Unknown),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("unknown trading status: {text}"),
            )),
        )),
    }
}
