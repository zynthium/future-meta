//! Shared storage with the original evidence and provenance read interfaces.

use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OptionalExtension, params_from_iter};
use serde_json::value::RawValue;
use std::collections::{BTreeMap, HashMap};

const EVIDENCE: [(&str, &[&str]); 3] = [
    (
        "fee_version_evidence",
        &["contract_id", "valid_from", "rule_hash"],
    ),
    ("contract_spec_evidence", &["contract_id", "valid_from"]),
    ("contract_lifecycle_evidence", &["contract_id"]),
];

pub(crate) fn is_table(conn: &Connection, name: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "select type from sqlite_schema where name=?1",
            [name],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .as_deref()
        == Some("table"))
}

/// Normalize only maintenance metadata; historical versions and observations stay intact.
pub(crate) fn normalize(conn: &Connection) -> Result<()> {
    let mut values = HashMap::new();
    if is_table(conn, "provenance_json_values")? {
        let mut statement = conn.prepare("select value, id from provenance_json_values")?;
        for row in statement.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (value, id) = row?;
            values.insert(value, id);
        }
    }
    for (table, id, column, parent) in [
        (
            "rebuild_contract_provenance",
            "contract_id",
            "provenance_json",
            "contracts",
        ),
        (
            "rebuild_spec_provenance",
            "spec_id",
            "source_json",
            "contract_spec_versions",
        ),
    ] {
        normalize_provenance(conn, table, id, column, parent, &mut values)?;
    }
    conn.execute_batch(
        "create table if not exists evidence_documents(
        id integer primary key,
        canonical_url text not null,
        body_sha256 text not null check(length(body_sha256)=64),
        unique(canonical_url,body_sha256)
    );",
    )?;
    for (table, keys) in EVIDENCE {
        normalize_evidence(conn, table, keys)?;
    }
    Ok(())
}

fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn assert_same(conn: &Connection, table: &str, select: &str) -> Result<()> {
    let count: i64 = conn.query_row(&format!("select count(*) from {table}"), [], |r| r.get(0))?;
    let copied: i64 = conn.query_row(&format!("select count(*) from ({select})"), [], |r| {
        r.get(0)
    })?;
    let different: bool = conn.query_row(
        &format!(
            "select exists(select * from {table} except {select})
             or exists({select} except select * from {table})"
        ),
        [],
        |r| r.get(0),
    )?;
    ensure!(
        count == copied && !different,
        "shared storage changed {table} records"
    );
    Ok(())
}

fn normalize_provenance(
    conn: &Connection,
    table: &str,
    id: &str,
    column: &str,
    parent: &str,
    values: &mut HashMap<String, i64>,
) -> Result<()> {
    if !is_table(conn, table)? {
        return Ok(());
    }
    let rows = conn
        .prepare(&format!("select {id},{column} from {table} order by {id}"))?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let Some((_, first)) = rows.first() else {
        return Ok(());
    };
    let first: BTreeMap<String, Box<RawValue>> = serde_json::from_str(first)?;
    let keys: Vec<_> = first.keys().cloned().collect();
    ensure!(!keys.is_empty(), "empty provenance object in {table}");
    conn.execute_batch(
        "create table if not exists provenance_json_values(
        id integer primary key, value text not null check(json_valid(value))
    );",
    )?;
    let backing = format!("{table}_parts");
    let columns = (0..keys.len())
        .map(|i| format!("p{i} integer not null references provenance_json_values(id)"))
        .collect::<Vec<_>>()
        .join(",");
    conn.execute_batch(&format!(
        "create table {backing}({id} integer primary key references {parent}(id),{columns});"
    ))?;
    let placeholders = vec!["?"; keys.len() + 1].join(",");
    let mut insert = conn.prepare(&format!("insert into {backing} values({placeholders})"))?;
    for (owner, body) in rows {
        let parts: BTreeMap<String, Box<RawValue>> = serde_json::from_str(&body)?;
        ensure!(
            parts.keys().eq(keys.iter()),
            "inconsistent provenance keys in {table}"
        );
        let mut rebuilt = Vec::new();
        let mut ids = vec![owner];
        for (key, part) in parts {
            let value = part.get();
            rebuilt.push(format!("{}:{value}", serde_json::to_string(&key)?));
            let identity = if let Some(identity) = values.get(value) {
                *identity
            } else {
                conn.execute(
                    "insert into provenance_json_values(value) values(?1)",
                    [value],
                )?;
                let identity = conn.last_insert_rowid();
                values.insert(value.to_owned(), identity);
                identity
            };
            ids.push(identity);
        }
        ensure!(
            format!("{{{}}}", rebuilt.join(",")) == body,
            "noncanonical provenance JSON in {table}; input remains unchanged"
        );
        insert.execute(params_from_iter(ids))?;
    }
    let mut fragments = Vec::new();
    let mut joins = Vec::new();
    for (i, key) in keys.iter().enumerate() {
        let prefix = format!(
            "{}{}:",
            if i == 0 { "{" } else { "," },
            serde_json::to_string(key)?
        );
        fragments.push(format!("{} || v{i}.value", literal(&prefix)));
        joins.push(format!(
            "join provenance_json_values v{i} on v{i}.id=p.p{i}"
        ));
    }
    let select = format!(
        "select p.{id}, {} || '}}' as {column} from {backing} p {}",
        fragments.join(" || "),
        joins.join(" ")
    );
    assert_same(conn, table, &select)?;
    conn.execute_batch(&format!(
        "drop table {table}; create view {table} as {select};"
    ))?;
    Ok(())
}

struct Column {
    name: String,
    kind: String,
    required: bool,
    default: Option<String>,
}

fn evidence_columns(conn: &Connection, table: &str) -> Result<Vec<Column>> {
    Ok(conn
        .prepare(&format!("pragma table_info({table})"))?
        .query_map([], |r| {
            Ok(Column {
                name: r.get(1)?,
                kind: r.get(2)?,
                required: r.get(3)?,
                default: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?)
}

fn normalize_evidence(conn: &Connection, table: &str, keys: &[&str]) -> Result<()> {
    let columns = evidence_columns(conn, table)?;
    let backing = format!("{table}_records");
    let fields: Vec<_> = columns
        .iter()
        .filter(|c| !["canonical_url", "body_sha256"].contains(&c.name.as_str()))
        .collect();
    ensure!(!fields.is_empty(), "missing evidence schema: {table}");
    for c in &columns {
        ensure!(
            c.name
                .chars()
                .all(|v| v.is_ascii_alphanumeric() || v == '_'),
            "invalid evidence column"
        );
    }
    if is_table(conn, table)? {
        let mut definitions = Vec::new();
        for c in &fields {
            ensure!(
                ["TEXT", "INTEGER"].contains(&c.kind.to_uppercase().as_str()),
                "unexpected evidence column type"
            );
            let mut definition = format!("{} {}", c.name, c.kind);
            if c.required {
                definition.push_str(" not null");
            }
            if let Some(default) = &c.default {
                definition.push_str(" default ");
                definition.push_str(default);
            }
            if c.name == "evidence_level" {
                definition
                    .push_str(" check(evidence_level in ('paired_official','official_parameter'))");
            }
            definitions.push(definition);
        }
        definitions
            .push("document_id integer not null references evidence_documents(id)".to_owned());
        definitions.push(format!("primary key({},document_id)", keys.join(",")));
        definitions.push("foreign key(contract_id) references contracts(id)".to_owned());
        conn.execute_batch(&format!(
            "create table {backing}({}) without rowid;
             insert or ignore into evidence_documents(canonical_url,body_sha256)
                select distinct canonical_url,body_sha256 from {table};",
            definitions.join(",")
        ))?;
        let names = fields
            .iter()
            .map(|c| format!("e.{}", c.name))
            .collect::<Vec<_>>()
            .join(",");
        conn.execute_batch(&format!(
            "insert into {backing} select {names},d.id from {table} e
             join evidence_documents d on d.canonical_url=e.canonical_url and d.body_sha256=e.body_sha256;"
        ))?;
        let projection = columns
            .iter()
            .map(|c| {
                format!(
                    "{}.{}",
                    if ["canonical_url", "body_sha256"].contains(&c.name.as_str()) {
                        "d"
                    } else {
                        "r"
                    },
                    c.name
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let select = format!(
            "select {projection} from {backing} r join evidence_documents d on d.id=r.document_id"
        );
        assert_same(conn, table, &select)?;
        conn.execute_batch(&format!(
            "drop table {table}; create view {table} as {select};"
        ))?;
    }
    install_evidence_writes(conn, table, keys, &fields)
}

fn install_evidence_writes(
    conn: &Connection,
    table: &str,
    keys: &[&str],
    fields: &[&Column],
) -> Result<()> {
    let backing = format!("{table}_records");
    // Views have no column defaults. Preserve the lifecycle default for legacy writers.
    let new_value = |c: &&Column| {
        if c.name == "provenance_kind" {
            "coalesce(new.provenance_kind,'official')".to_owned()
        } else {
            format!("new.{}", c.name)
        }
    };
    let names = fields
        .iter()
        .map(|c| c.name.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let new_values = fields.iter().map(new_value).collect::<Vec<_>>().join(",");
    let document = |prefix: &str| {
        format!(
            "(select id from evidence_documents where canonical_url={prefix}.canonical_url and body_sha256={prefix}.body_sha256)"
        )
    };
    let old_key = keys
        .iter()
        .map(|key| format!("{key}=old.{key}"))
        .chain(std::iter::once(format!("document_id={}", document("old"))))
        .collect::<Vec<_>>()
        .join(" and ");
    let assignments = fields
        .iter()
        .map(|c| format!("{}={}", c.name, new_value(c)))
        .chain(std::iter::once(format!("document_id={}", document("new"))))
        .collect::<Vec<_>>()
        .join(",");
    let register = "insert or ignore into evidence_documents(canonical_url,body_sha256) values(new.canonical_url,new.body_sha256);";
    conn.execute_batch(&format!(
        "create trigger if not exists {table}_insert instead of insert on {table} begin
            {register} insert into {backing}({names},document_id) values({new_values},{}); end;
         create trigger if not exists {table}_update instead of update on {table} begin
            {register} update {backing} set {assignments} where {old_key}; end;
         create trigger if not exists {table}_delete instead of delete on {table} begin
            delete from {backing} where {old_key}; end;",
        document("new")
    ))
    .with_context(|| format!("install writable evidence interface for {table}"))?;
    Ok(())
}
