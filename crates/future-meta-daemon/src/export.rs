//! Archive and manifest export.

use anyhow::{Result, anyhow};
use future_meta::archive::{encode_archive_bytes, sha256_hex};
use future_meta::model::{
    Contract, ContractFee, ContractSpecVersion, FeeArchiveV2, FeeSpec, Manifest, SCHEMA_VERSION,
    TradingStatus,
};
use rusqlite::{Connection, Row};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::{Date, OffsetDateTime, UtcOffset};

/// Export an archive from the database.
///
/// # Errors
///
/// Returns an error if archive export fails.
pub fn export_archive(db: &Path, out: &Path) -> Result<()> {
    let conn = crate::db::connect_readonly(db)?;
    ensure_only_official_fee_versions(&conn)?;
    crate::db::validate_fee_history_integrity(&conn)?;
    std::fs::create_dir_all(out.join("artifacts"))?;
    let archive = load_archive(&conn, OffsetDateTime::now_utc())?;
    let bytes = encode_archive_bytes(&archive)?;
    let sha = sha256_hex(&bytes);
    let data_version = archive
        .generated_at
        .replace([':', '+'], "")
        .replace('-', "");
    let artifact_name = format!("artifacts/future-meta-fees-v2-{data_version}.fmeta.zst");

    std::fs::write(out.join("latest.fmeta.zst"), &bytes)?;
    std::fs::write(out.join(&artifact_name), &bytes)?;

    let manifest = Manifest {
        schema_version: SCHEMA_VERSION,
        data_version: archive.generated_at.clone(),
        generated_at: archive.generated_at.clone(),
        history_start: archive.history_start.clone(),
        history_end: archive.history_end.clone(),
        fee_effective_from: Some(archive.history_end.clone()),
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

fn ensure_only_official_fee_versions(conn: &Connection) -> Result<()> {
    let untrusted_versions: i64 = conn.query_row(
        "select count(*) from fee_versions where source_kind <> 'official'",
        [],
        |row| row.get(0),
    )?;
    if untrusted_versions > 0 {
        return Err(anyhow!(
            "refusing to export non-official fee versions: {untrusted_versions} present"
        ));
    }
    Ok(())
}

fn load_archive(conn: &Connection, generated_at: OffsetDateTime) -> Result<FeeArchiveV2> {
    let generated_on = generated_at.to_offset(UtcOffset::from_hms(8, 0, 0)?).date();
    let mut contracts_stmt = conn.prepare(
        "select contracts.id, contracts.symbol, contracts.listing_date, contracts.expiry_date,
                contracts.lot_size, contracts.tick_size,
                exists(
                  select 1 from contract_lifecycle_evidence evidence
                  where evidence.contract_id = contracts.id
                    and evidence.listing_date = contracts.listing_date
                    and evidence.expiry_date = contracts.expiry_date
                )
         from contracts
         order by id",
    )?;
    let contracts = contracts_stmt
        .query_map([], |row| {
            let listing_date: Option<String> = row.get(2)?;
            let expiry_date: Option<String> = row.get(3)?;
            Ok(Contract {
                id: read_u32(row, 0)?,
                symbol: row.get(1)?,
                listing_date: listing_date.clone(),
                expiry_date: expiry_date.clone(),
                lot_size: row.get(4)?,
                tick_size: row.get(5)?,
                active: contract_is_active_on(
                    listing_date.as_deref(),
                    expiry_date.as_deref(),
                    row.get::<_, i64>(6)? != 0,
                    generated_on,
                ),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    let mut spec_stmt = conn.prepare(
        "select contract_id, lot_size, tick_size, valid_from, valid_to
         from contract_spec_versions
         order by contract_id, valid_from, id",
    )?;
    let contract_spec_versions = spec_stmt
        .query_map([], |row| {
            Ok(ContractSpecVersion {
                contract_id: read_u32(row, 0)?,
                lot_size: row.get(1)?,
                tick_size: row.get(2)?,
                valid_from: row.get(3)?,
                valid_to: row.get(4)?,
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

    let generated_at = generated_at.format(&Rfc3339)?;
    let history_start = fee_versions
        .iter()
        .map(|version| version.valid_from.clone())
        .min()
        .unwrap_or_else(|| generated_at.clone());
    let history_end = fee_versions
        .iter()
        .map(|version| version.valid_from.clone())
        .max()
        .unwrap_or_else(|| generated_at.clone());

    Ok(FeeArchiveV2 {
        schema_version: SCHEMA_VERSION,
        generated_at: generated_at.clone(),
        history_start,
        history_end,
        contracts,
        contract_spec_versions,
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

fn contract_is_active_on(
    listing_date: Option<&str>,
    expiry_date: Option<&str>,
    lifecycle_is_reviewed: bool,
    generated_on: Date,
) -> bool {
    if !lifecycle_is_reviewed {
        return false;
    }
    let (Some(listing_date), Some(expiry_date)) = (listing_date, expiry_date) else {
        return false;
    };
    let (Some(listing_date), Some(expiry_date)) = (
        parse_contract_lifecycle_date(listing_date),
        parse_contract_lifecycle_date(expiry_date),
    ) else {
        return false;
    };

    listing_date <= generated_on && generated_on <= expiry_date
}

fn parse_contract_lifecycle_date(value: &str) -> Option<Date> {
    let value = value.trim();
    let format = if value.len() == 8 {
        "[year][month][day]"
    } else {
        "[year]-[month]-[day]"
    };
    let format = time::format_description::parse(format).ok()?;
    Date::parse(value, &format).ok()
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

#[cfg(test)]
mod tests {
    use super::contract_is_active_on;
    use time::{Date, Month};

    #[test]
    fn active_contract_requires_reviewed_lifecycle_and_inclusive_dates() {
        let generated_on = Date::from_calendar_date(2026, Month::August, 27).unwrap();

        assert!(contract_is_active_on(
            Some("20260827"),
            Some("20260827"),
            true,
            generated_on,
        ));
        assert!(!contract_is_active_on(
            Some("20260828"),
            Some("20260901"),
            true,
            generated_on,
        ));
        assert!(!contract_is_active_on(
            Some("20260801"),
            Some("20260826"),
            true,
            generated_on,
        ));
        assert!(!contract_is_active_on(
            Some("20260801"),
            Some("20260901"),
            false,
            generated_on,
        ));
        assert!(!contract_is_active_on(
            None,
            Some("20260901"),
            true,
            generated_on,
        ));
        assert!(!contract_is_active_on(
            Some("invalid"),
            Some("20260901"),
            true,
            generated_on,
        ));
    }
}
