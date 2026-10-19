//! Private schema-4 wire representation. Public query models stay materialized.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{
    error::FutureMetaError,
    model::{
        Contract, ContractFee, ContractSpecVersion, FeeArchiveV3, FeeSpec, SCHEMA_VERSION,
        TradingStatus,
    },
};

type Result<T> = std::result::Result<T, FutureMetaError>;
type FeeValue = (String, FeeSpec, FeeSpec, FeeSpec, Option<FeeSpec>);
type MarginValue = (Option<f64>, Option<f64>);
type ContextValue = (TradingStatus, bool, Option<String>);

fn corrupt(message: impl Into<String>) -> FutureMetaError {
    FutureMetaError::CorruptArchive(message.into())
}
fn index(value: usize) -> Result<u32> {
    u32::try_from(value).map_err(|_| corrupt("schema-4 collection exceeds u32 indexes"))
}
fn lookup<T>(values: &[T], id: u32) -> Result<&T> {
    values
        .get(id as usize)
        .ok_or_else(|| corrupt(format!("invalid schema-4 dictionary index {id}")))
}

struct Pool<T> {
    values: Vec<T>,
    indexes: BTreeMap<Vec<u8>, u32>,
}
impl<T: Serialize> Pool<T> {
    fn new() -> Self {
        Self {
            values: Vec::new(),
            indexes: BTreeMap::new(),
        }
    }
    fn intern(&mut self, value: T) -> Result<u32> {
        // Binary keys preserve exact f64 bits and distinguish unknown from zero.
        let key = bincode::serde::encode_to_vec(&value, bincode::config::standard())
            .map_err(|e| corrupt(e.to_string()))?;
        if let Some(id) = self.indexes.get(&key) {
            return Ok(*id);
        }
        let id = index(self.values.len())?;
        self.values.push(value);
        self.indexes.insert(key, id);
        Ok(id)
    }
}

#[derive(Serialize, Deserialize)]
struct ContractRef {
    id: u32,
    symbol: String,
    listing: Option<u32>,
    expiry: Option<u32>,
    spec: u32,
    active: bool,
}
#[derive(Serialize, Deserialize)]
struct SpecRef {
    contract: u32,
    spec: u32,
    from: u32,
    to: Option<u32>,
}
#[derive(Serialize, Deserialize)]
struct ContractHistory {
    contract: u32,
    end: Option<u32>,
    // Timestamp-ID delta, fee ID, margin ID, context ID.
    changes: Vec<(u32, u32, u32, u32)>,
    // Interior ends that differ from the next start, including explicit None.
    end_overrides: Vec<(u32, Option<u32>)>,
}

/// Field order is the schema-4 wire contract. This type is intentionally private.
#[derive(Serialize, Deserialize)]
pub(crate) struct CompactArchive {
    schema_version: u32,
    generated_at: String,
    history_start: String,
    history_end: String,
    text: Vec<String>,
    contracts: Vec<ContractRef>,
    specs: Vec<(f64, f64)>,
    spec_versions: Vec<SpecRef>,
    fees: Vec<FeeValue>,
    margins: Vec<MarginValue>,
    contexts: Vec<ContextValue>,
    histories: Vec<ContractHistory>,
    // Empty for normal exports. Otherwise maps original positions to canonical
    // positions so the public encoder also preserves arbitrary caller ordering.
    original_order: Vec<u32>,
}

impl CompactArchive {
    pub(crate) fn pack(archive: &FeeArchiveV3) -> Result<Self> {
        let mut strings = BTreeSet::new();
        for c in &archive.contracts {
            strings.extend(c.listing_date.iter().chain(&c.expiry_date).cloned());
        }
        for s in &archive.contract_spec_versions {
            strings.insert(s.valid_from.clone());
            strings.extend(s.valid_to.iter().cloned());
        }
        for f in &archive.fee_versions {
            strings.insert(f.valid_from.clone());
            strings.extend(f.valid_to.iter().cloned());
        }
        let text = strings.into_iter().collect::<Vec<_>>();
        let text_ids = text
            .iter()
            .enumerate()
            .map(|(i, s)| Ok((s.as_str(), index(i)?)))
            .collect::<Result<BTreeMap<_, _>>>()?;
        let mut specs = Pool::new();
        let contracts = archive
            .contracts
            .iter()
            .map(|c| {
                Ok(ContractRef {
                    id: c.id,
                    symbol: c.symbol.clone(),
                    listing: c.listing_date.as_ref().map(|s| text_ids[s.as_str()]),
                    expiry: c.expiry_date.as_ref().map(|s| text_ids[s.as_str()]),
                    spec: specs.intern((c.lot_size, c.tick_size))?,
                    active: c.active,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let spec_versions = archive
            .contract_spec_versions
            .iter()
            .map(|s| {
                Ok(SpecRef {
                    contract: s.contract_id,
                    spec: specs.intern((s.lot_size, s.tick_size))?,
                    from: text_ids[s.valid_from.as_str()],
                    to: s.valid_to.as_ref().map(|s| text_ids[s.as_str()]),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        let h = pack_histories(archive, &text_ids)?;
        Ok(Self {
            schema_version: SCHEMA_VERSION,
            generated_at: archive.generated_at.clone(),
            history_start: archive.history_start.clone(),
            history_end: archive.history_end.clone(),
            text,
            contracts,
            specs: specs.values,
            spec_versions,
            fees: h.fees,
            margins: h.margins,
            contexts: h.contexts,
            histories: h.histories,
            original_order: h.original_order,
        })
    }

    pub(crate) fn unpack(self) -> Result<FeeArchiveV3> {
        if self.schema_version != SCHEMA_VERSION || self.text.windows(2).any(|w| w[0] >= w[1]) {
            return Err(corrupt("invalid schema-4 header or timestamp dictionary"));
        }
        let mut ids = BTreeSet::new();
        let mut symbols = BTreeSet::new();
        let mut contracts = Vec::with_capacity(self.contracts.len());
        for c in &self.contracts {
            if !ids.insert(c.id) || !symbols.insert(c.symbol.as_str()) {
                return Err(corrupt("duplicate schema-4 contract"));
            }
            let &(lot_size, tick_size) = lookup(&self.specs, c.spec)?;
            contracts.push(Contract {
                id: c.id,
                symbol: c.symbol.clone(),
                listing_date: self.optional_text(c.listing)?,
                expiry_date: self.optional_text(c.expiry)?,
                lot_size,
                tick_size,
                active: c.active,
            });
        }
        let mut contract_spec_versions = Vec::with_capacity(self.spec_versions.len());
        for s in &self.spec_versions {
            if !ids.contains(&s.contract) {
                return Err(corrupt("specification references unknown contract"));
            }
            let &(lot_size, tick_size) = lookup(&self.specs, s.spec)?;
            contract_spec_versions.push(ContractSpecVersion {
                contract_id: s.contract,
                lot_size,
                tick_size,
                valid_from: lookup(&self.text, s.from)?.clone(),
                valid_to: self.optional_text(s.to)?,
            });
        }
        let mut fee_versions = Vec::new();
        let mut seen = BTreeSet::new();
        for history in &self.histories {
            if !ids.contains(&history.contract) || !seen.insert(history.contract) {
                return Err(corrupt("unknown or duplicate schema-4 history contract"));
            }
            self.expand_history(history, &mut fee_versions)?;
        }
        let fee_versions = restore_order(fee_versions, &self.original_order)?;
        Ok(FeeArchiveV3 {
            schema_version: SCHEMA_VERSION,
            generated_at: self.generated_at,
            history_start: self.history_start,
            history_end: self.history_end,
            contracts,
            contract_spec_versions,
            fee_versions,
        })
    }

    fn optional_text(&self, id: Option<u32>) -> Result<Option<String>> {
        id.map(|id| lookup(&self.text, id).cloned()).transpose()
    }

    fn expand_history(&self, history: &ContractHistory, rows: &mut Vec<ContractFee>) -> Result<()> {
        if history.changes.is_empty() {
            return Err(corrupt("empty schema-4 contract history"));
        }
        let mut prior = None;
        for &(i, _) in &history.end_overrides {
            if i as usize >= history.changes.len() - 1 || prior.is_some_and(|p| p >= i) {
                return Err(corrupt("invalid schema-4 end override index"));
            }
            prior = Some(i);
        }
        let mut overrides = history.end_overrides.iter().peekable();
        let mut from = 0_u32;
        for (i, &(delta, f, m, c)) in history.changes.iter().enumerate() {
            from = from
                .checked_add(delta)
                .ok_or_else(|| corrupt("schema-4 timestamp delta overflow"))?;
            let to = if overrides.peek().is_some_and(|&&(at, _)| at as usize == i) {
                overrides
                    .next()
                    .ok_or_else(|| corrupt("missing end override"))?
                    .1
            } else if let Some(next) = history.changes.get(i + 1) {
                Some(
                    from.checked_add(next.0)
                        .ok_or_else(|| corrupt("schema-4 timestamp delta overflow"))?,
                )
            } else {
                history.end
            };
            let (rule_hash, open_fee, close_yesterday_fee, close_today_fee, intraday_open_fee) =
                lookup(&self.fees, f)?.clone();
            let (buy_margin_rate, sell_margin_rate) = *lookup(&self.margins, m)?;
            let (trading_status, is_main_contract, source_updated_at) =
                lookup(&self.contexts, c)?.clone();
            rows.push(ContractFee {
                contract_id: history.contract,
                rule_hash,
                buy_margin_rate,
                sell_margin_rate,
                open_fee,
                close_yesterday_fee,
                close_today_fee,
                intraday_open_fee,
                trading_status,
                is_main_contract,
                source_updated_at,
                valid_from: lookup(&self.text, from)?.clone(),
                valid_to: self.optional_text(to)?,
            });
        }
        Ok(())
    }
}

struct PackedHistories {
    fees: Vec<FeeValue>,
    margins: Vec<MarginValue>,
    contexts: Vec<ContextValue>,
    histories: Vec<ContractHistory>,
    original_order: Vec<u32>,
}

fn pack_histories(
    archive: &FeeArchiveV3,
    text_ids: &BTreeMap<&str, u32>,
) -> Result<PackedHistories> {
    let mut fees = Pool::new();
    let mut margins = Pool::new();
    let mut contexts = Pool::new();
    let mut grouped = BTreeMap::<u32, Vec<(usize, &ContractFee)>>::new();
    for (i, f) in archive.fee_versions.iter().enumerate() {
        grouped.entry(f.contract_id).or_default().push((i, f));
    }
    let mut original_order = vec![0; archive.fee_versions.len()];
    let mut position = 0;
    let mut histories = Vec::with_capacity(grouped.len());
    for (contract, mut rows) in grouped {
        rows.sort_by(|a, b| a.1.valid_from.cmp(&b.1.valid_from).then(a.0.cmp(&b.0)));
        let mut changes = Vec::with_capacity(rows.len());
        let mut end_overrides = Vec::new();
        let mut previous = 0;
        for (i, (original, f)) in rows.iter().enumerate() {
            original_order[*original] = index(position)?;
            position += 1;
            let from = text_ids[f.valid_from.as_str()];
            let delta = from
                .checked_sub(previous)
                .ok_or_else(|| corrupt("unordered schema-4 timestamps"))?;
            previous = from;
            changes.push((
                delta,
                fees.intern((
                    f.rule_hash.clone(),
                    f.open_fee.clone(),
                    f.close_yesterday_fee.clone(),
                    f.close_today_fee.clone(),
                    f.intraday_open_fee.clone(),
                ))?,
                margins.intern((f.buy_margin_rate, f.sell_margin_rate))?,
                contexts.intern((
                    f.trading_status.clone(),
                    f.is_main_contract,
                    f.source_updated_at.clone(),
                ))?,
            ));
            if let Some((_, next)) = rows.get(i + 1)
                && f.valid_to.as_ref() != Some(&next.valid_from)
            {
                end_overrides.push((index(i)?, f.valid_to.as_ref().map(|s| text_ids[s.as_str()])));
            }
        }
        let end = rows
            .last()
            .and_then(|(_, f)| f.valid_to.as_ref())
            .map(|s| text_ids[s.as_str()]);
        histories.push(ContractHistory {
            contract,
            end,
            changes,
            end_overrides,
        });
    }
    if original_order
        .iter()
        .enumerate()
        .all(|(i, &v)| usize::try_from(v) == Ok(i))
    {
        original_order.clear();
    }
    Ok(PackedHistories {
        fees: fees.values,
        margins: margins.values,
        contexts: contexts.values,
        histories,
        original_order,
    })
}

fn restore_order(rows: Vec<ContractFee>, order: &[u32]) -> Result<Vec<ContractFee>> {
    if order.is_empty() {
        return Ok(rows);
    }
    if order.len() != rows.len() {
        return Err(corrupt("invalid schema-4 original order length"));
    }
    let mut slots = rows.into_iter().map(Some).collect::<Vec<_>>();
    order
        .iter()
        .map(|&id| {
            slots
                .get_mut(id as usize)
                .and_then(Option::take)
                .ok_or_else(|| corrupt("invalid schema-4 order permutation"))
        })
        .collect()
}
