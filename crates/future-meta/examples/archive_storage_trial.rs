//! Offline wire-format experiments. Never writes published artifacts or changes the reader.

use future_meta::{
    Contract, ContractFee, ContractSpecVersion, FeeArchiveV3, FeeSpec, FutureMeta,
    archive::{decode_archive_bytes, sha256_hex},
    model::TradingStatus,
    symbol::derive_underlying_symbol,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    hint::black_box,
    path::{Path, PathBuf},
    time::Instant,
};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

const TRIAL_SCHEMA_VERSION: u32 = u32::MAX - 1;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
type FeeValue = (String, FeeSpec, FeeSpec, FeeSpec, Option<FeeSpec>);
type MarginValue = (Option<f64>, Option<f64>);
type ContextValue = (TradingStatus, bool, Option<String>);
type Version = (u32, u32, u32, u32, u32, Option<u32>);

#[derive(Default)]
struct Pool<T> {
    values: Vec<T>,
    indexes: BTreeMap<Vec<u8>, u32>,
}
impl<T: Serialize + Clone> Pool<T> {
    fn intern(&mut self, value: T) -> Result<u32> {
        let key = bincode::serde::encode_to_vec(&value, bincode::config::standard())?;
        if let Some(id) = self.indexes.get(&key) {
            return Ok(*id);
        }
        let id = u32::try_from(self.values.len())?;
        self.values.push(value);
        self.indexes.insert(key, id);
        Ok(id)
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct WireContract {
    id: u32,
    symbol: String,
    listing: Option<u32>,
    expiry: Option<u32>,
    spec: u32,
    active: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Span {
    contract: u32,
    value: u32,
    from: u32,
    to: Option<u32>,
}
#[derive(Clone, Serialize, Deserialize)]
struct Group {
    value: u32,
    from: u32,
    to: Option<u32>,
    members: Vec<u32>,
}
#[derive(Clone, Serialize, Deserialize)]
struct ContractChanges {
    contract: u32,
    end: Option<u32>,
    // Sorted timestamp dictionary IDs permit small nonnegative deltas.
    changes: Vec<(u32, u32, u32, u32)>,
    // Preserve genuine gaps rather than assuming every next start is an end.
    end_overrides: Vec<(u32, Option<u32>)>,
}
#[derive(Clone, Serialize, Deserialize)]
enum History {
    Joint(Vec<Version>),
    Batched(Vec<ContractChanges>),
    Split {
        fees: Vec<Span>,
        margins: Vec<Span>,
        contexts: Vec<Span>,
    },
    Grouped {
        fees: Vec<Group>,
        margins: Vec<Group>,
        contexts: Vec<Group>,
    },
}
#[derive(Clone, Serialize, Deserialize)]
struct WireArchive {
    // Private trial marker; this is not a production schema-4 layout.
    schema_version: u32,
    generated_at: String,
    history_start: String,
    history_end: String,
    text: Vec<String>,
    contracts: Vec<WireContract>,
    specs: Vec<(f64, f64)>,
    spec_versions: Vec<Span>,
    fees: Vec<FeeValue>,
    margins: Vec<MarginValue>,
    contexts: Vec<ContextValue>,
    history: History,
}

fn check(ok: bool, message: &str) -> Result<()> {
    if ok { Ok(()) } else { Err(message.into()) }
}
fn get<T>(values: &[T], id: u32) -> Result<&T> {
    values
        .get(usize::try_from(id)?)
        .ok_or_else(|| "invalid dictionary reference".into())
}
fn optional_text(pool: &mut Pool<String>, text: Option<&str>) -> Result<Option<u32>> {
    text.map(|s| pool.intern(s.to_owned())).transpose()
}

impl WireArchive {
    fn batched(&self) -> Result<Self> {
        let History::Joint(versions) = &self.history else {
            return Err("batching requires joint input".into());
        };
        let mut result = self.clone();
        result.text.sort();
        let remap = self
            .text
            .iter()
            .map(|s| {
                result
                    .text
                    .binary_search(s)
                    .map_err(|_| "missing timestamp")
                    .and_then(|i| u32::try_from(i).map_err(|_| "timestamp dictionary overflow"))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let mapped = |id: u32| remap[id as usize];
        for c in &mut result.contracts {
            c.listing = c.listing.map(mapped);
            c.expiry = c.expiry.map(mapped);
        }
        for s in &mut result.spec_versions {
            s.from = mapped(s.from);
            s.to = s.to.map(mapped);
        }
        let mut by_contract = BTreeMap::<u32, Vec<Version>>::new();
        for v in versions {
            by_contract.entry(v.0).or_default().push(*v);
        }
        let mut batches = Vec::new();
        for (contract, rows) in by_contract {
            let mut previous = 0;
            let mut changes = Vec::new();
            let mut end_overrides = Vec::new();
            for (i, row) in rows.iter().enumerate() {
                let from = mapped(row.4);
                let delta = from
                    .checked_sub(previous)
                    .ok_or("unordered contract timestamps")?;
                changes.push((delta, row.1, row.2, row.3));
                previous = from;
                if let Some(next) = rows.get(i + 1)
                    && row.5 != Some(next.4)
                {
                    end_overrides.push((u32::try_from(i)?, row.5.map(mapped)));
                }
            }
            batches.push(ContractChanges {
                contract,
                end: rows.last().and_then(|r| r.5).map(mapped),
                changes,
                end_overrides,
            });
        }
        result.history = History::Batched(batches);
        Ok(result)
    }
    fn pack(archive: &FeeArchiveV3) -> Result<Self> {
        let mut text = Pool::<String>::default();
        let mut specs = Pool::<(f64, f64)>::default();
        let mut fees = Pool::<FeeValue> {
            values: Vec::new(),
            indexes: BTreeMap::new(),
        };
        let mut margins = Pool::<MarginValue>::default();
        let mut contexts = Pool::<ContextValue> {
            values: Vec::new(),
            indexes: BTreeMap::new(),
        };
        let mut contracts = Vec::new();
        for c in &archive.contracts {
            contracts.push(WireContract {
                id: c.id,
                symbol: c.symbol.clone(),
                listing: optional_text(&mut text, c.listing_date.as_deref())?,
                expiry: optional_text(&mut text, c.expiry_date.as_deref())?,
                spec: specs.intern((c.lot_size, c.tick_size))?,
                active: c.active,
            });
        }
        let mut spec_versions = Vec::new();
        for s in &archive.contract_spec_versions {
            spec_versions.push(Span {
                contract: s.contract_id,
                value: specs.intern((s.lot_size, s.tick_size))?,
                from: text.intern(s.valid_from.clone())?,
                to: optional_text(&mut text, s.valid_to.as_deref())?,
            });
        }
        let mut history = Vec::new();
        for v in &archive.fee_versions {
            history.push((
                v.contract_id,
                fees.intern((
                    v.rule_hash.clone(),
                    v.open_fee.clone(),
                    v.close_yesterday_fee.clone(),
                    v.close_today_fee.clone(),
                    v.intraday_open_fee.clone(),
                ))?,
                margins.intern((v.buy_margin_rate, v.sell_margin_rate))?,
                contexts.intern((
                    v.trading_status.clone(),
                    v.is_main_contract,
                    v.source_updated_at.clone(),
                ))?,
                text.intern(v.valid_from.clone())?,
                optional_text(&mut text, v.valid_to.as_deref())?,
            ));
        }
        Ok(Self {
            schema_version: TRIAL_SCHEMA_VERSION,
            generated_at: archive.generated_at.clone(),
            history_start: archive.history_start.clone(),
            history_end: archive.history_end.clone(),
            text: text.values,
            contracts,
            specs: specs.values,
            spec_versions,
            fees: fees.values,
            margins: margins.values,
            contexts: contexts.values,
            history: History::Joint(history),
        })
    }

    fn split(&self, grouped: bool) -> Result<Self> {
        let History::Joint(versions) = &self.history else {
            return Err("split requires joint input".into());
        };
        // This corpus has canonical timestamps on one offset. Reject a different
        // axis rather than silently ordering mixed timestamp representations.
        let mut offset = None;
        for v in versions {
            for id in [Some(v.4), v.5].into_iter().flatten() {
                let literal = get(&self.text, id)?;
                let instant = OffsetDateTime::parse(literal, &Rfc3339)?;
                check(
                    instant.format(&Rfc3339)? == *literal,
                    "noncanonical split timestamp",
                )?;
                check(
                    offset.is_none_or(|o| o == instant.offset()),
                    "mixed split timestamp offsets",
                )?;
                offset = Some(instant.offset());
            }
        }
        let mut fee_spans = Vec::new();
        let mut margin_spans = Vec::new();
        let mut context_spans = Vec::new();
        for &(contract, fee, margin, context, from, to) in versions {
            append_span(
                &mut fee_spans,
                Span {
                    contract,
                    value: fee,
                    from,
                    to,
                },
            );
            append_span(
                &mut margin_spans,
                Span {
                    contract,
                    value: margin,
                    from,
                    to,
                },
            );
            append_span(
                &mut context_spans,
                Span {
                    contract,
                    value: context,
                    from,
                    to,
                },
            );
        }
        let mut result = self.clone();
        result.history = if grouped {
            let products = self
                .contracts
                .iter()
                .map(|c| Ok((c.id, derive_underlying_symbol(&c.symbol)?)))
                .collect::<Result<BTreeMap<_, _>>>()?;
            History::Grouped {
                fees: group_spans(fee_spans, &products)?,
                margins: group_spans(margin_spans, &products)?,
                contexts: group_spans(context_spans, &products)?,
            }
        } else {
            History::Split {
                fees: fee_spans,
                margins: margin_spans,
                contexts: context_spans,
            }
        };
        Ok(result)
    }

    fn versions(&self) -> Result<Vec<Version>> {
        Ok(match &self.history {
            History::Joint(v) => v.clone(),
            History::Batched(batches) => {
                let mut rows = Vec::new();
                for batch in batches {
                    let overrides = batch
                        .end_overrides
                        .iter()
                        .copied()
                        .collect::<BTreeMap<_, _>>();
                    let mut from = 0_u32;
                    for (i, &(delta, fee, margin, context)) in batch.changes.iter().enumerate() {
                        from = from.checked_add(delta).ok_or("timestamp delta overflow")?;
                        let end = match overrides.get(&u32::try_from(i)?) {
                            Some(to) => *to,
                            None => batch.changes.get(i + 1).map_or(Ok(batch.end), |next| {
                                from.checked_add(next.0)
                                    .map(Some)
                                    .ok_or("timestamp delta overflow")
                            })?,
                        };
                        rows.push((batch.contract, fee, margin, context, from, end));
                    }
                }
                rows
            }
            History::Split {
                fees,
                margins,
                contexts,
            } => join_spans(fees, margins, contexts, &self.text),
            History::Grouped {
                fees,
                margins,
                contexts,
            } => join_spans(
                &ungroup(fees),
                &ungroup(margins),
                &ungroup(contexts),
                &self.text,
            ),
        })
    }

    fn unpack(self) -> Result<FeeArchiveV3> {
        check(
            self.schema_version == TRIAL_SCHEMA_VERSION || self.schema_version == 4,
            "not an experimental wire archive",
        )?;
        let contracts = self
            .contracts
            .iter()
            .map(|c| {
                let &(lot_size, tick_size) = get(&self.specs, c.spec)?;
                Ok(Contract {
                    id: c.id,
                    symbol: c.symbol.clone(),
                    listing_date: c.listing.map(|i| get(&self.text, i).cloned()).transpose()?,
                    expiry_date: c.expiry.map(|i| get(&self.text, i).cloned()).transpose()?,
                    lot_size,
                    tick_size,
                    active: c.active,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let contract_spec_versions = self
            .spec_versions
            .iter()
            .map(|s| {
                let &(lot_size, tick_size) = get(&self.specs, s.value)?;
                Ok(ContractSpecVersion {
                    contract_id: s.contract,
                    lot_size,
                    tick_size,
                    valid_from: get(&self.text, s.from)?.clone(),
                    valid_to: s.to.map(|i| get(&self.text, i).cloned()).transpose()?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let versions = self.versions()?;
        let mut fee_versions = Vec::with_capacity(versions.len());
        for (contract_id, f, m, c, from, to) in versions {
            let (rule_hash, open_fee, close_yesterday_fee, close_today_fee, intraday_open_fee) =
                get(&self.fees, f)?.clone();
            let (buy_margin_rate, sell_margin_rate) = *get(&self.margins, m)?;
            let (trading_status, is_main_contract, source_updated_at) =
                get(&self.contexts, c)?.clone();
            fee_versions.push(ContractFee {
                contract_id,
                rule_hash,
                open_fee,
                close_yesterday_fee,
                close_today_fee,
                intraday_open_fee,
                buy_margin_rate,
                sell_margin_rate,
                trading_status,
                is_main_contract,
                source_updated_at,
                valid_from: get(&self.text, from)?.clone(),
                valid_to: to.map(|i| get(&self.text, i).cloned()).transpose()?,
            });
        }
        Ok(FeeArchiveV3 {
            schema_version: 3,
            generated_at: self.generated_at,
            history_start: self.history_start,
            history_end: self.history_end,
            contracts,
            contract_spec_versions,
            fee_versions,
        })
    }
}

fn append_span(spans: &mut Vec<Span>, next: Span) {
    if let Some(prior) = spans.last_mut()
        && prior.contract == next.contract
        && prior.value == next.value
        && prior.to == Some(next.from)
    {
        prior.to = next.to;
        return;
    }
    spans.push(next);
}
fn group_spans(spans: Vec<Span>, products: &BTreeMap<u32, String>) -> Result<Vec<Group>> {
    let mut groups = BTreeMap::<(String, u32, u32, Option<u32>), Vec<u32>>::new();
    for s in spans {
        let product = products.get(&s.contract).ok_or("unknown member contract")?;
        groups
            .entry((product.clone(), s.value, s.from, s.to))
            .or_default()
            .push(s.contract);
    }
    // Product text is used only while grouping. Persist explicit contract IDs,
    // never derived product labels or defaults for unobserved members/dates.
    Ok(groups
        .into_iter()
        .map(|((_, value, from, to), members)| Group {
            value,
            from,
            to,
            members,
        })
        .collect())
}
fn ungroup(groups: &[Group]) -> Vec<Span> {
    groups
        .iter()
        .flat_map(|g| {
            g.members.iter().map(|&contract| Span {
                contract,
                value: g.value,
                from: g.from,
                to: g.to,
            })
        })
        .collect()
}
fn join_spans(fees: &[Span], margins: &[Span], contexts: &[Span], text: &[String]) -> Vec<Version> {
    let mut contracts = BTreeMap::<u32, [Vec<&Span>; 3]>::new();
    for (kind, spans) in [fees, margins, contexts].into_iter().enumerate() {
        for s in spans {
            contracts.entry(s.contract).or_default()[kind].push(s);
        }
    }
    let mut versions = Vec::new();
    for (contract, mut sets) in contracts {
        for spans in &mut sets {
            spans.sort_by(|a, b| text[a.from as usize].cmp(&text[b.from as usize]));
        }
        let mut boundaries = BTreeSet::new();
        for s in sets.iter().flatten() {
            boundaries.insert(s.from);
            if let Some(end) = s.to {
                boundaries.insert(end);
            }
        }
        let mut boundaries = boundaries.into_iter().collect::<Vec<_>>();
        boundaries.sort_by(|&a, &b| text[a as usize].cmp(&text[b as usize]));
        let mut positions = [0; 3];
        for (index, &from) in boundaries.iter().enumerate() {
            let mut active = [None; 3];
            for kind in 0..3 {
                while positions[kind] < sets[kind].len()
                    && sets[kind][positions[kind]]
                        .to
                        .is_some_and(|end| text[end as usize] <= text[from as usize])
                {
                    positions[kind] += 1;
                }
                if let Some(s) = sets[kind].get(positions[kind])
                    && text[s.from as usize] <= text[from as usize]
                {
                    active[kind] = Some(s.value);
                }
            }
            if let [Some(f), Some(m), Some(c)] = active {
                versions.push((contract, f, m, c, from, boundaries.get(index + 1).copied()));
            }
        }
    }
    versions
}

fn encode<T: Serialize>(value: &T) -> Result<(Vec<u8>, usize)> {
    encode_level(value, 19)
}
fn encode_level<T: Serialize>(value: &T, level: i32) -> Result<(Vec<u8>, usize)> {
    let raw = bincode::serde::encode_to_vec(value, bincode::config::standard())?;
    Ok((zstd::stream::encode_all(raw.as_slice(), level)?, raw.len()))
}
fn decode(bytes: &[u8], experimental: bool) -> Result<FeeArchiveV3> {
    if !experimental {
        return Ok(decode_archive_bytes(bytes)?);
    }
    let raw = zstd::stream::decode_all(bytes)?;
    let (wire, used): (WireArchive, _) =
        bincode::serde::decode_from_slice(&raw, bincode::config::standard())?;
    check(used == raw.len(), "trailing trial bytes")?;
    wire.unpack()
}
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    values[values.len() / 2]
}

fn compare_variant(
    name: &str,
    encoded: (Vec<u8>, usize),
    experimental: bool,
    baseline: &FeeArchiveV3,
    baseline_raw: &[u8],
    queries: &[(String, OffsetDateTime, ContractFee)],
    output: &Path,
) -> Result<serde_json::Value> {
    let (bytes, raw_len) = encoded;
    let restored = decode(&bytes, experimental)?;
    check(&restored == baseline, "reconstructed archive differs")?;
    let restored_raw = bincode::serde::encode_to_vec(&restored, bincode::config::standard())?;
    check(
        restored_raw == baseline_raw,
        "reconstructed schema-3 serialization differs",
    )?;
    std::fs::write(output.join(format!("{name}.trial.zst")), &bytes)?;
    let mut loads = Vec::new();
    for _ in 0..8 {
        let started = Instant::now();
        let client = FutureMeta::from_archive(decode(&bytes, experimental)?)?;
        black_box(&client);
        loads.push(started.elapsed().as_secs_f64() * 1000.0);
    }
    let client = FutureMeta::from_archive(restored)?;
    for (symbol, at, expected) in queries {
        check(
            client.contract_fee_at(symbol, *at)? == expected,
            "as-of result differs",
        )?;
    }
    let mut query_times = Vec::new();
    for _ in 0..8 {
        let started = Instant::now();
        for _ in 0..100 {
            for (symbol, at, _) in queries {
                black_box(client.contract_fee_at(symbol, *at)?);
            }
        }
        query_times.push(started.elapsed().as_secs_f64() * 1000.0 / 100.0);
    }
    let item = serde_json::json!({"variant":name,"compressed_bytes":bytes.len(),"bincode_bytes":raw_len,"decode_and_index_ms":median(&mut loads[1..]),
            "asof_1000_ms":median(&mut query_times[1..]),"exact_archive_equal":true,
            "exact_schema3_serialized_sha256":sha256_hex(&restored_raw),"requires_new_decoder":experimental});
    eprintln!("{name}: {item}");
    Ok(item)
}

fn verify_existing_variants(output: &Path, baseline: &FeeArchiveV3, raw: &[u8]) -> Result<()> {
    let symbols = baseline
        .contracts
        .iter()
        .map(|c| (c.id, c.symbol.as_str()))
        .collect::<BTreeMap<_, _>>();
    for name in [
        "schema3",
        "schema3_zstd22",
        "dictionary",
        "contract_changes",
        "split",
        "shared_members",
    ] {
        let bytes = std::fs::read(output.join(format!("{name}.trial.zst")))?;
        let archive = decode(&bytes, !name.starts_with("schema3"))?;
        check(&archive == baseline, "saved trial archive differs")?;
        check(
            bincode::serde::encode_to_vec(&archive, bincode::config::standard())? == raw,
            "saved trial serialization differs",
        )?;
        let client = FutureMeta::from_archive(archive)?;
        for version in baseline.fee_versions.iter().step_by(56).take(1000) {
            check(
                client.contract_fee_asof(symbols[&version.contract_id], &version.valid_from)?
                    == version,
                "saved trial query differs",
            )?;
        }
    }
    println!("Verified six wire formats, complete serialization and 1000 as-of samples each");
    Ok(())
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let input = PathBuf::from(
        args.next()
            .ok_or("usage: archive_storage_trial <archive> <new-output-directory>")?,
    );
    let output = PathBuf::from(args.next().ok_or("missing output directory")?);
    let verify_existing = args.next().as_deref() == Some("--verify-existing");
    if !verify_existing {
        check(!output.exists(), "output directory already exists")?;
        std::fs::create_dir_all(&output)?;
    }
    let input_bytes = std::fs::read(&input)?;
    let mut baseline = decode_archive_bytes(&input_bytes)?;
    baseline.schema_version = future_meta::model::INTRADAY_SCHEMA_VERSION;
    let baseline_raw = bincode::serde::encode_to_vec(&baseline, bincode::config::standard())?;
    check(
        zstd::stream::decode_all(input_bytes.as_slice())? == baseline_raw,
        "baseline serialization is not canonical",
    )?;
    if verify_existing {
        return verify_existing_variants(&output, &baseline, &baseline_raw);
    }
    let wire = WireArchive::pack(&baseline)?;
    let mut profile = Vec::new();
    for (name, (compressed, raw)) in [
        ("contracts", encode(&baseline.contracts)?),
        ("spec_versions", encode(&baseline.contract_spec_versions)?),
        ("fee_versions", encode(&baseline.fee_versions)?),
    ] {
        profile.push(serde_json::json!({"section":name,"bincode_bytes":raw,"independently_zstd_bytes":compressed.len()}));
    }
    let by_id = baseline
        .contracts
        .iter()
        .map(|c| (c.id, c.symbol.clone()))
        .collect::<BTreeMap<_, _>>();
    let step = (baseline.fee_versions.len() / 1000).max(1);
    let queries = baseline
        .fee_versions
        .iter()
        .step_by(step)
        .take(1000)
        .map(|v| {
            Ok((
                by_id[&v.contract_id].clone(),
                OffsetDateTime::parse(&v.valid_from, &Rfc3339)?,
                v.clone(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut results = Vec::new();
    for (name, encoded, experimental) in [
        ("schema3", encode(&baseline)?, false),
        ("schema3_zstd22", encode_level(&baseline, 22)?, false),
        ("dictionary", encode(&wire)?, true),
        ("contract_changes", encode(&wire.batched()?)?, true),
        ("split", encode(&wire.split(false)?)?, true),
        ("shared_members", encode(&wire.split(true)?)?, true),
    ] {
        results.push(compare_variant(
            name,
            encoded,
            experimental,
            &baseline,
            &baseline_raw,
            &queries,
            &output,
        )?);
    }
    let split = wire.split(false)?;
    let grouped = wire.split(true)?;
    let counts = match (&split.history, &grouped.history) {
        (
            History::Split {
                fees,
                margins,
                contexts,
            },
            History::Grouped {
                fees: gf,
                margins: gm,
                contexts: gc,
            },
        ) => {
            serde_json::json!({"fee_intervals":fees.len(),"margin_intervals":margins.len(),"context_intervals":contexts.len(),"fee_groups":gf.len(),"margin_groups":gm.len(),"context_groups":gc.len()})
        }
        _ => return Err("unexpected history representation".into()),
    };
    let report = serde_json::json!({"input":input,"input_sha256":sha256_hex(&input_bytes),"contracts":baseline.contracts.len(),"spec_versions":baseline.contract_spec_versions.len(),
        "joint_versions":baseline.fee_versions.len(),"fee_payloads":wire.fees.len(),"margin_payloads":wire.margins.len(),"spec_payloads":wire.specs.len(),"text_values":wire.text.len(),
        "counts":counts,"section_profile":profile,"results":results,"production_reader_changed":false,"experimental_schema":TRIAL_SCHEMA_VERSION});
    std::fs::write(
        output.join("comparison.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(())
}
