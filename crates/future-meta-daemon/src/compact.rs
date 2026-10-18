//! Lossless physical compaction of the daemon seed.

use crate::db;
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::path::Path;

const EVIDENCE_TABLES: [&str; 3] = [
    "fee_version_evidence",
    "contract_spec_evidence",
    "contract_lifecycle_evidence",
];

/// Build a compact seed copy without changing the input or any logical rows.
///
/// # Errors
/// Returns an error on an existing output, invalid history, or failed migration.
pub fn compact_seed(input: &Path, output: &Path) -> Result<Value> {
    ensure!(
        !output.exists(),
        "output already exists: {}",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let work = parent.join(format!(".compact-work-{}.sqlite", std::process::id()));
    let packed = parent.join(format!(".compact-packed-{}.sqlite", std::process::id()));
    ensure!(
        !work.exists() && !packed.exists(),
        "temporary compact output already exists"
    );
    let result = build(input, &work, &packed).and_then(|report| {
        std::fs::rename(&packed, output)?;
        Ok(report)
    });
    let _ = std::fs::remove_file(&work);
    let _ = std::fs::remove_file(&packed);
    result
}

fn build(input: &Path, work: &Path, packed: &Path) -> Result<Value> {
    let source = db::connect_readonly(input)?;
    db::validate_fee_history_integrity(&source)?;
    source.execute(
        "vacuum main into ?1",
        [work.to_str().context("invalid work path")?],
    )?;
    drop(source);

    let mut conn = db::connect(work)?;
    let tx = conn.transaction()?;
    let mut counts = serde_json::Map::new();
    for table in EVIDENCE_TABLES {
        let without_rowid: bool = tx.query_row(
            "select wr from pragma_table_list where schema='main' and name=?1",
            [table],
            |r| r.get(0),
        )?;
        if !without_rowid {
            let sql: String = tx.query_row(
                "select sql from sqlite_schema where type='table' and name=?1",
                [table],
                |r| r.get(0),
            )?;
            let (_, definition) = sql.split_once('(').context("missing table definition")?;
            let definition = definition.trim_end().trim_end_matches(';');
            let replacement = format!("{table}_compact");
            tx.execute_batch(&format!(
                "create table {replacement} ({definition} without rowid;
                 insert into {replacement} select * from {table};"
            ))?;
            let differences: i64 = tx.query_row(&format!(
                "select count(*) from (select * from {table} except select * from {replacement})"
            ), [], |r| r.get(0))?;
            let original_count: i64 =
                tx.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))?;
            let replacement_count: i64 =
                tx.query_row(&format!("select count(*) from {replacement}"), [], |r| {
                    r.get(0)
                })?;
            ensure!(
                differences == 0 && original_count == replacement_count,
                "evidence rows differ: {table}"
            );
            tx.execute_batch(&format!(
                "drop table {table}; alter table {replacement} rename to {table};"
            ))?;
        }
        let count: i64 =
            tx.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))?;
        counts.insert(table.to_owned(), Value::from(count));
    }
    tx.commit()?;
    db::validate_fee_history_integrity(&conn)?;
    ensure!(
        !conn.prepare("pragma foreign_key_check")?.exists([])?,
        "foreign key violation"
    );
    conn.execute(
        "vacuum main into ?1",
        [packed.to_str().context("invalid packed path")?],
    )?;
    drop(conn);

    let verified = db::connect_readonly(packed)?;
    let integrity: String = verified.query_row("pragma integrity_check", [], |r| r.get(0))?;
    ensure!(integrity == "ok", "SQLite integrity check: {integrity}");
    Ok(serde_json::json!({
        "input_bytes": std::fs::metadata(input)?.len(),
        "output_bytes": std::fs::metadata(packed)?.len(),
        "evidence_rows": counts,
        "integrity": integrity,
    }))
}
