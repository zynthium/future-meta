//! Import CFFEX contract lifecycle and specification history from retained official evidence.

use crate::db;
use crate::official::validate_official_canonical_url;
use anyhow::{Context, Result, anyhow, bail};
use future_meta::symbol::{SymbolKind, parse_symbol};
use quick_xml::Reader;
use quick_xml::events::Event;
use rusqlite::{OptionalExtension, Transaction, params};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, Weekday};

const LAST_TRADING_DAY_SUFFIX: &str = "最后交易日";

/// Inputs for one offline, hash-verified CFFEX metadata import.
#[derive(Debug, Clone)]
pub struct CffexMetadataImportOptions {
    /// Existing history database.
    pub history_db: PathBuf,
    /// Reviewed product specification history TSV.
    pub product_manifest: PathBuf,
    /// Retained CFFEX trading-calendar XML manifest TSV.
    pub calendar_manifest: PathBuf,
    /// Directory containing files named by their SHA-256 digest.
    pub snapshot_dir: PathBuf,
    /// Audit timestamp recorded on imported rows.
    pub observed_at: String,
}

/// Counts returned after a successful atomic import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CffexMetadataImportResult {
    /// Concrete CFFEX contracts updated.
    pub contracts: usize,
    /// Official specification intervals written.
    pub specification_versions: usize,
}

#[derive(Debug, Clone, Deserialize)]
struct ProductManifestRow {
    product: String,
    valid_from: String,
    valid_to: String,
    lot_size: f64,
    tick_size: f64,
    expiry_rule: String,
    specification_url: String,
    specification_sha256: String,
}

#[derive(Debug, Clone)]
struct ProductRule {
    source: ProductManifestRow,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
    expiry_rule: ExpiryRule,
}

#[derive(Debug, Clone, Deserialize)]
struct CalendarManifestRow {
    month: String,
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct EvidenceRef {
    canonical_url: String,
    sha256: String,
}

#[derive(Debug, Clone)]
struct ExpiryEvent {
    date: Date,
    evidence: EvidenceRef,
}

#[derive(Debug)]
struct CalendarEvidence {
    months: BTreeSet<(i32, Month)>,
    expiry_events: BTreeMap<String, ExpiryEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpiryRule {
    SecondFriday,
    ThirdFriday,
}

#[derive(Debug)]
struct ContractRow {
    id: i64,
    symbol: String,
    local: String,
    product: String,
    listing: Date,
}

/// Replace CFFEX lifecycle and specification histories from official product pages,
/// notices, listing evidence already paired to fees, and exchange calendar XML.
///
/// Exact calendar events take precedence. Contracts outside retained calendar months
/// use the official product rule, which is sufficient for already-expired pre-audit
/// contracts and not-yet-reached distant contracts.
///
/// # Errors
///
/// Returns an error for malformed manifests, missing retained bytes, conflicting
/// calendar events, incomplete product histories, missing listing evidence, or DB errors.
pub fn import_contract_metadata(
    options: &CffexMetadataImportOptions,
) -> Result<CffexMetadataImportResult> {
    OffsetDateTime::parse(&options.observed_at, &Rfc3339)
        .context("invalid CFFEX metadata observed_at")?;
    let product_rules = load_product_rules(&options.product_manifest, &options.snapshot_dir)?;
    let calendar = load_calendar_events(&options.calendar_manifest, &options.snapshot_dir)?;

    let mut connection = db::connect(&options.history_db)?;
    db::ensure_schema(&connection)?;
    let contracts = load_contracts(&connection)?;
    if contracts.is_empty() {
        bail!("CFFEX metadata import found no contracts");
    }

    let transaction = connection.transaction()?;
    let mut specification_versions = 0;
    for contract in &contracts {
        let rules = product_rules
            .get(&contract.product)
            .ok_or_else(|| anyhow!("missing CFFEX product metadata: {}", contract.product))?;
        let expiry = resolve_expiry(contract, rules, &calendar.months, &calendar.expiry_events)?;
        specification_versions += import_contract(
            &transaction,
            contract,
            expiry,
            rules,
            calendar.expiry_events.get(&contract.local),
            &options.snapshot_dir,
            &options.observed_at,
        )?;
    }
    transaction.commit()?;

    Ok(CffexMetadataImportResult {
        contracts: contracts.len(),
        specification_versions,
    })
}

fn load_product_rules(
    manifest: &Path,
    snapshot_dir: &Path,
) -> Result<BTreeMap<String, Vec<ProductRule>>> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(manifest)?;
    let mut by_product = BTreeMap::<String, Vec<ProductRule>>::new();
    for source in reader.deserialize::<ProductManifestRow>() {
        let source = source?;
        if source.product.is_empty()
            || !source
                .product
                .chars()
                .all(|character| character.is_ascii_uppercase())
        {
            bail!("invalid CFFEX product code: {}", source.product);
        }
        if !source.lot_size.is_finite() || source.lot_size <= 0.0 {
            bail!("invalid CFFEX lot size: {}", source.product);
        }
        if !source.tick_size.is_finite() || source.tick_size <= 0.0 {
            bail!("invalid CFFEX tick size: {}", source.product);
        }
        validate_official_canonical_url("CFFEX", &source.specification_url)?;
        verify_retained_evidence(snapshot_dir, &source.specification_sha256)?;
        let valid_from = OffsetDateTime::parse(&source.valid_from, &Rfc3339)
            .with_context(|| format!("invalid CFFEX product valid_from: {}", source.product))?;
        let valid_to = nonempty(&source.valid_to)
            .map(|value| OffsetDateTime::parse(value, &Rfc3339))
            .transpose()
            .with_context(|| format!("invalid CFFEX product valid_to: {}", source.product))?;
        if valid_to.is_some_and(|value| value <= valid_from) {
            bail!("invalid CFFEX product interval: {}", source.product);
        }
        let expiry_rule = match source.expiry_rule.as_str() {
            "second_friday" => ExpiryRule::SecondFriday,
            "third_friday" => ExpiryRule::ThirdFriday,
            value => bail!("unsupported CFFEX expiry rule: {value}"),
        };
        by_product
            .entry(source.product.clone())
            .or_default()
            .push(ProductRule {
                source,
                valid_from,
                valid_to,
                expiry_rule,
            });
    }
    if by_product.is_empty() {
        bail!("CFFEX product metadata manifest is empty");
    }
    for (product, rules) in &mut by_product {
        rules.sort_by_key(|rule| rule.valid_from);
        for pair in rules.windows(2) {
            if pair[0].valid_to != Some(pair[1].valid_from) {
                bail!("CFFEX product specification intervals not contiguous: {product}");
            }
            if pair[0].expiry_rule != pair[1].expiry_rule {
                bail!("CFFEX product expiry rule changes unexpectedly: {product}");
            }
        }
    }
    Ok(by_product)
}

fn load_calendar_events(manifest: &Path, snapshot_dir: &Path) -> Result<CalendarEvidence> {
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(b'\t')
        .from_path(manifest)?;
    let mut months = BTreeSet::new();
    let mut events = BTreeMap::<String, ExpiryEvent>::new();
    for row in reader.deserialize::<CalendarManifestRow>() {
        let row = row?;
        let month = parse_year_month(&row.month)?;
        validate_calendar_url(&row, month)?;
        verify_retained_evidence(snapshot_dir, &row.sha256)?;
        months.insert(month);
        let bytes = read_retained_evidence(snapshot_dir, &row.sha256)?;
        let body = String::from_utf8(bytes).context("CFFEX calendar XML is not UTF-8")?;
        for (date_text, title) in parse_calendar_documents(&body)? {
            let title = title.trim();
            let Some(local) = title.strip_suffix(LAST_TRADING_DAY_SUFFIX) else {
                continue;
            };
            if !is_cffex_contract_local(local) {
                continue;
            }
            let date = parse_date(date_text.trim())?;
            let event = ExpiryEvent {
                date,
                evidence: EvidenceRef {
                    canonical_url: row.canonical_url.clone(),
                    sha256: row.sha256.clone(),
                },
            };
            if let Some(previous) = events.get(local) {
                if previous.date != date {
                    bail!("conflicting CFFEX calendar expiry events: {local}");
                }
            } else {
                events.insert(local.to_owned(), event);
            }
        }
    }
    if months.is_empty() {
        bail!("CFFEX calendar manifest is empty");
    }
    Ok(CalendarEvidence {
        months,
        expiry_events: events,
    })
}

fn parse_calendar_documents(body: &str) -> Result<Vec<(String, String)>> {
    #[derive(Clone, Copy)]
    enum Field {
        Date,
        Title,
    }

    let mut reader = Reader::from_str(body);
    reader.config_mut().trim_text(true);
    let mut documents = Vec::new();
    let mut date = String::new();
    let mut title = String::new();
    let mut field = None;
    loop {
        match reader.read_event()? {
            Event::Start(element) if element.name().as_ref() == b"pubdate" => {
                field = Some(Field::Date);
            }
            Event::Start(element) if element.name().as_ref() == b"title" => {
                field = Some(Field::Title);
            }
            Event::Text(text) => match field {
                Some(Field::Date) => date.push_str(&text.decode()?),
                Some(Field::Title) => title.push_str(&text.decode()?),
                None => {}
            },
            Event::CData(text) => match field {
                Some(Field::Date) => date.push_str(&text.decode()?),
                Some(Field::Title) => title.push_str(&text.decode()?),
                None => {}
            },
            Event::End(element) if matches!(element.name().as_ref(), b"pubdate" | b"title") => {
                field = None;
            }
            Event::End(element) if element.name().as_ref() == b"doc" => {
                if !date.is_empty() && !title.is_empty() {
                    documents.push((std::mem::take(&mut date), std::mem::take(&mut title)));
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(documents)
}

fn load_contracts(connection: &rusqlite::Connection) -> Result<Vec<ContractRow>> {
    let mut statement = connection.prepare(
        "select id, symbol, listing_date from contracts
         where symbol like 'CFFEX.%' order by symbol",
    )?;
    statement
        .query_map([], |record| {
            Ok((
                record.get::<_, i64>(0)?,
                record.get::<_, String>(1)?,
                record.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|record| {
            let (id, symbol, listing_date) = record?;
            let parsed = parse_symbol(&symbol)?;
            if parsed.kind != SymbolKind::Futures || parsed.exchange != "CFFEX" {
                bail!("invalid CFFEX metadata symbol: {symbol}");
            }
            let product_len = parsed
                .local
                .chars()
                .take_while(char::is_ascii_alphabetic)
                .count();
            let listing_date = listing_date
                .ok_or_else(|| anyhow!("CFFEX contract lacks listing date: {symbol}"))?;
            Ok(ContractRow {
                id,
                symbol,
                local: parsed.local.clone(),
                product: parsed.local[..product_len].to_owned(),
                listing: parse_compact_date(&listing_date)?,
            })
        })
        .collect()
}

fn resolve_expiry(
    contract: &ContractRow,
    rules: &[ProductRule],
    calendar_months: &BTreeSet<(i32, Month)>,
    expiry_events: &BTreeMap<String, ExpiryEvent>,
) -> Result<Date> {
    if let Some(event) = expiry_events.get(&contract.local) {
        return Ok(event.date);
    }
    let (year, month) = contract_year_month(&contract.local)?;
    if calendar_months.contains(&(year, month)) {
        bail!(
            "CFFEX calendar month lacks contract expiry event: {}",
            contract.symbol
        );
    }
    let rule = rules
        .first()
        .ok_or_else(|| anyhow!("empty CFFEX product rule history: {}", contract.product))?
        .expiry_rule;
    nth_friday(year, month, rule)
}

fn import_contract(
    transaction: &Transaction<'_>,
    contract: &ContractRow,
    expiry: Date,
    rules: &[ProductRule],
    expiry_event: Option<&ExpiryEvent>,
    snapshot_dir: &Path,
    observed_at: &str,
) -> Result<usize> {
    if expiry < contract.listing {
        bail!("CFFEX expiry precedes listing: {}", contract.symbol);
    }
    let listing_start = exchange_day_start(contract.listing)?;
    let expiry_end = exchange_day_start(
        expiry
            .next_day()
            .ok_or_else(|| anyhow!("CFFEX expiry cannot advance: {}", contract.symbol))?,
    )?;
    let selected = intersect_rules(&contract.symbol, rules, listing_start, expiry_end)?;
    let latest = selected
        .last()
        .ok_or_else(|| anyhow!("missing CFFEX specification history: {}", contract.symbol))?;

    transaction.execute(
        "update contracts set expiry_date = ?1, lot_size = ?2, tick_size = ?3,
         last_seen_at = ?4 where id = ?5",
        params![
            compact_date(expiry),
            latest.0.source.lot_size,
            latest.0.source.tick_size,
            observed_at,
            contract.id
        ],
    )?;
    for table in [
        "contract_spec_evidence",
        "contract_spec_versions",
        "contract_lifecycle_evidence",
    ] {
        transaction.execute(
            &format!("delete from {table} where contract_id = ?1"),
            [contract.id],
        )?;
    }

    for (rule, valid_from, valid_to) in &selected {
        transaction.execute(
            "insert into contract_spec_versions(
               contract_id, lot_size, tick_size, valid_from, valid_to,
               source_kind, source_url, first_seen_at, last_seen_at
             ) values(?1, ?2, ?3, ?4, ?5, 'official', ?6, ?7, ?7)",
            params![
                contract.id,
                rule.source.lot_size,
                rule.source.tick_size,
                valid_from.format(&Rfc3339)?,
                valid_to.map(|value| value.format(&Rfc3339)).transpose()?,
                rule.source.specification_url,
                observed_at
            ],
        )?;
        transaction.execute(
            "insert into contract_spec_evidence(
               contract_id, valid_from, canonical_url, body_sha256, recorded_at
             ) values(?1, ?2, ?3, ?4, ?5)",
            params![
                contract.id,
                valid_from.format(&Rfc3339)?,
                rule.source.specification_url,
                rule.source.specification_sha256,
                observed_at
            ],
        )?;
    }

    let listing_evidence = load_listing_evidence(transaction, contract)?;
    verify_retained_evidence(snapshot_dir, &listing_evidence.sha256)?;
    insert_lifecycle_evidence(
        transaction,
        contract,
        expiry,
        &listing_evidence,
        observed_at,
    )?;
    let expiry_evidence = expiry_event.map_or_else(
        || EvidenceRef {
            canonical_url: rules[0].source.specification_url.clone(),
            sha256: rules[0].source.specification_sha256.clone(),
        },
        |event| event.evidence.clone(),
    );
    insert_lifecycle_evidence(transaction, contract, expiry, &expiry_evidence, observed_at)?;
    Ok(selected.len())
}

fn intersect_rules<'a>(
    symbol: &str,
    rules: &'a [ProductRule],
    listing_start: OffsetDateTime,
    expiry_end: OffsetDateTime,
) -> Result<Vec<(&'a ProductRule, OffsetDateTime, Option<OffsetDateTime>)>> {
    let mut selected = Vec::new();
    for rule in rules {
        let rule_end = rule.valid_to.unwrap_or(expiry_end);
        let start = rule.valid_from.max(listing_start);
        let end = rule_end.min(expiry_end);
        if start >= end {
            continue;
        }
        selected.push((rule, start, (end < expiry_end).then_some(end)));
    }
    if selected.first().map(|row| row.1) != Some(listing_start) {
        bail!("CFFEX specification history starts after listing: {symbol}");
    }
    for pair in selected.windows(2) {
        if pair[0].2 != Some(pair[1].1) {
            bail!("CFFEX specification history is not contiguous: {symbol}");
        }
    }
    if selected.last().is_some_and(|row| row.2.is_some()) {
        bail!("CFFEX specification history ends before expiry: {symbol}");
    }
    Ok(selected)
}

fn load_listing_evidence(
    transaction: &Transaction<'_>,
    contract: &ContractRow,
) -> Result<EvidenceRef> {
    let listing_prefix = format!("{}T%", contract.listing);
    transaction
        .query_row(
            "select canonical_url, body_sha256 from fee_version_evidence
             where contract_id = ?1 and valid_from like ?2
               and canonical_url like '%/cn/jystz/%.html%'
               and evidence_level = 'paired_official'
             order by canonical_url limit 1",
            params![contract.id, listing_prefix],
            |record| {
                Ok(EvidenceRef {
                    canonical_url: record.get(0)?,
                    sha256: record.get(1)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            anyhow!(
                "missing CFFEX official listing evidence: {}",
                contract.symbol
            )
        })
}

fn insert_lifecycle_evidence(
    transaction: &Transaction<'_>,
    contract: &ContractRow,
    expiry: Date,
    evidence: &EvidenceRef,
    observed_at: &str,
) -> Result<()> {
    validate_official_canonical_url("CFFEX", &evidence.canonical_url)?;
    transaction.execute(
        "insert or ignore into contract_lifecycle_evidence(
           contract_id, listing_date, expiry_date, canonical_url, body_sha256, recorded_at
         ) values(?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            contract.id,
            compact_date(contract.listing),
            compact_date(expiry),
            evidence.canonical_url,
            evidence.sha256,
            observed_at
        ],
    )?;
    Ok(())
}

fn validate_calendar_url(row: &CalendarManifestRow, month: (i32, Month)) -> Result<()> {
    validate_official_canonical_url("CFFEX", &row.canonical_url)?;
    let expected = format!(
        "/sj/jyrl/{:04}{:02}/index_6782.xml",
        month.0,
        u8::from(month.1)
    );
    let url = reqwest::Url::parse(&row.canonical_url)?;
    if url.path() != expected || url.query().is_some() || url.fragment().is_some() {
        bail!(
            "invalid CFFEX calendar canonical URL: {}",
            row.canonical_url
        );
    }
    Ok(())
}

fn parse_year_month(value: &str) -> Result<(i32, Month)> {
    let (year, month) = value
        .split_once('-')
        .ok_or_else(|| anyhow!("invalid CFFEX calendar month: {value}"))?;
    let year = year.parse::<i32>()?;
    let month = Month::try_from(month.parse::<u8>()?)?;
    if value != format!("{year:04}-{:02}", u8::from(month)) {
        bail!("invalid CFFEX calendar month: {value}");
    }
    Ok((year, month))
}

fn contract_year_month(local: &str) -> Result<(i32, Month)> {
    let product_len = local.chars().take_while(char::is_ascii_alphabetic).count();
    let digits = &local[product_len..];
    if digits.len() != 4 || !digits.chars().all(|character| character.is_ascii_digit()) {
        bail!("invalid CFFEX contract month: {local}");
    }
    let year = 2000 + digits[..2].parse::<i32>()?;
    let month = Month::try_from(digits[2..].parse::<u8>()?)?;
    Ok((year, month))
}

fn nth_friday(year: i32, month: Month, rule: ExpiryRule) -> Result<Date> {
    let target = match rule {
        ExpiryRule::SecondFriday => 2,
        ExpiryRule::ThirdFriday => 3,
    };
    let mut date = Date::from_calendar_date(year, month, 1)?;
    let mut seen = 0;
    loop {
        if date.weekday() == Weekday::Friday {
            seen += 1;
            if seen == target {
                return Ok(date);
            }
        }
        date = date
            .next_day()
            .ok_or_else(|| anyhow!("CFFEX expiry date cannot advance"))?;
    }
}

fn is_cffex_contract_local(value: &str) -> bool {
    let product_len = value.chars().take_while(char::is_ascii_alphabetic).count();
    product_len > 0
        && product_len < value.len()
        && value[product_len..].len() == 4
        && value[product_len..]
            .chars()
            .all(|character| character.is_ascii_digit())
}

fn parse_date(value: &str) -> Result<Date> {
    let format = time::format_description::parse("[year]-[month]-[day]")?;
    Date::parse(value, &format).with_context(|| format!("invalid CFFEX date: {value}"))
}

fn parse_compact_date(value: &str) -> Result<Date> {
    let normalized = if value.len() == 8 {
        format!("{}-{}-{}", &value[..4], &value[4..6], &value[6..])
    } else {
        value.to_owned()
    };
    parse_date(&normalized)
}

fn exchange_day_start(date: Date) -> Result<OffsetDateTime> {
    OffsetDateTime::parse(&format!("{date}T00:00:00+08:00"), &Rfc3339)
        .context("invalid CFFEX exchange-day timestamp")
}

fn compact_date(date: Date) -> String {
    date.to_string().replace('-', "")
}

fn nonempty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn verify_retained_evidence(snapshot_dir: &Path, expected_sha256: &str) -> Result<()> {
    let path = retained_evidence_path(snapshot_dir, expected_sha256)?;
    let actual = hex::encode(Sha256::digest(std::fs::read(path)?));
    if actual != expected_sha256 {
        bail!("retained CFFEX evidence SHA-256 mismatch: {expected_sha256}");
    }
    Ok(())
}

fn read_retained_evidence(snapshot_dir: &Path, expected_sha256: &str) -> Result<Vec<u8>> {
    std::fs::read(retained_evidence_path(snapshot_dir, expected_sha256)?)
        .context("cannot read retained CFFEX evidence")
}

fn retained_evidence_path(snapshot_dir: &Path, expected_sha256: &str) -> Result<PathBuf> {
    if expected_sha256.len() != 64
        || !expected_sha256
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        bail!("invalid CFFEX evidence SHA-256: {expected_sha256}");
    }
    let prefix = format!("{expected_sha256}.");
    let matches = std::fs::read_dir(snapshot_dir)?
        .filter_map(std::result::Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name == expected_sha256 || name.starts_with(&prefix))
        })
        .map(|entry| entry.path())
        .collect::<Vec<_>>();
    if matches.len() != 1 {
        bail!("retained CFFEX evidence must resolve uniquely: {expected_sha256}");
    }
    Ok(matches[0].clone())
}
