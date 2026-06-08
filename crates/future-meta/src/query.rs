//! Query API entry points.

use std::collections::HashMap;

use crate::error::FutureMetaError;
use crate::model::{Contract, ContractFee, FeeArchiveV1, TradingStatus};
use crate::symbol::{SymbolKind, derive_underlying_symbol, parse_symbol};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, UtcOffset};

/// High-performance local future-meta query client.
#[derive(Debug, Clone)]
pub struct FutureMeta {
    archive: FeeArchiveV1,
    history_start: OffsetDateTime,
    contract_by_symbol: HashMap<String, u32>,
    contract_index_by_id: HashMap<u32, ContractIndex>,
    fees_by_contract: HashMap<u32, Vec<FeeVersionIndex>>,
    contracts_by_underlying: HashMap<String, Vec<u32>>,
}

#[derive(Debug, Clone)]
struct ContractIndex {
    listing_date: Option<Date>,
    expiry_date: Option<Date>,
}

#[derive(Debug, Clone)]
struct FeeVersionIndex {
    archive_index: usize,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
}

impl FutureMeta {
    /// Build an indexed query client from a decoded archive.
    ///
    /// # Errors
    ///
    /// Returns an error if a contract symbol in the archive cannot provide a
    /// supported futures underlying symbol.
    pub fn from_archive(archive: FeeArchiveV1) -> Result<Self, FutureMetaError> {
        let history_start = parse_archive_timestamp("history_start", &archive.history_start)?;
        let mut contract_by_symbol = HashMap::new();
        let mut contract_index_by_id = HashMap::new();
        let mut contracts_by_underlying: HashMap<String, Vec<u32>> = HashMap::new();

        for contract in &archive.contracts {
            contract_by_symbol.insert(contract.symbol.clone(), contract.id);
            contract_index_by_id.insert(
                contract.id,
                ContractIndex {
                    listing_date: parse_optional_archive_date(
                        "listing_date",
                        contract.listing_date.as_deref(),
                    )?,
                    expiry_date: parse_optional_archive_date(
                        "expiry_date",
                        contract.expiry_date.as_deref(),
                    )?,
                },
            );
            let underlying = derive_underlying_symbol(&contract.symbol)?;
            contracts_by_underlying
                .entry(underlying)
                .or_default()
                .push(contract.id);
        }

        let mut fees_by_contract: HashMap<u32, Vec<FeeVersionIndex>> = HashMap::new();
        for (index, fee) in archive.fee_versions.iter().enumerate() {
            fees_by_contract
                .entry(fee.contract_id)
                .or_default()
                .push(FeeVersionIndex {
                    archive_index: index,
                    valid_from: parse_archive_timestamp("valid_from", &fee.valid_from)?,
                    valid_to: parse_optional_archive_timestamp(
                        "valid_to",
                        fee.valid_to.as_deref(),
                    )?,
                });
        }
        for indexes in fees_by_contract.values_mut() {
            indexes.sort_by(|left, right| {
                left.valid_from
                    .cmp(&right.valid_from)
                    .then_with(|| left.archive_index.cmp(&right.archive_index))
            });
        }

        Ok(Self {
            archive,
            history_start,
            contract_by_symbol,
            contract_index_by_id,
            fees_by_contract,
            contracts_by_underlying,
        })
    }

    /// Load an encoded archive file and build an indexed query client.
    ///
    /// # Errors
    ///
    /// Returns an error if the file cannot be read, the archive cannot be
    /// decoded, or the decoded archive cannot be indexed.
    #[cfg(feature = "download")]
    pub async fn load_file(path: impl AsRef<std::path::Path>) -> Result<Self, FutureMetaError> {
        let bytes = tokio::fs::read(path).await?;
        let archive = crate::archive::decode_archive_bytes(&bytes)?;
        Self::from_archive(archive)
    }

    /// Return the fee rule for a concrete contract at the requested time.
    ///
    /// # Errors
    ///
    /// Returns an error when `at` predates the archive history, the contract is
    /// unknown, or no fee version covers `at`.
    pub fn contract_fee_asof(
        &self,
        symbol: &str,
        at: &str,
    ) -> Result<&ContractFee, FutureMetaError> {
        let at = parse_query_timestamp(at)?;
        if at < self.history_start {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }

        let contract_id = self
            .contract_by_symbol
            .get(symbol)
            .copied()
            .ok_or_else(|| FutureMetaError::UnknownContract(symbol.to_owned()))?;

        self.fee_for_contract_id_asof(contract_id, at)
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    /// Return all underlying contract fee rules available at the requested time.
    ///
    /// # Errors
    ///
    /// Returns an error when `at` predates the archive history or the
    /// underlying symbol is unknown.
    pub fn underlying_fees_asof(
        &self,
        underlying_symbol: &str,
        at: &str,
    ) -> Result<Vec<&ContractFee>, FutureMetaError> {
        let at = parse_query_timestamp(at)?;
        if at < self.history_start {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }

        let contract_ids = self
            .contracts_by_underlying
            .get(underlying_symbol)
            .ok_or_else(|| {
                FutureMetaError::UnknownUnderlyingSymbol(underlying_symbol.to_owned())
            })?;

        Ok(contract_ids
            .iter()
            .filter_map(|contract_id| self.contract_fee_for_underlying_asof(*contract_id, at))
            .collect())
    }

    /// Return the main-contract fee rule for a `KQ.m@...` query alias.
    ///
    /// # Errors
    ///
    /// Returns an error when the symbol is not a supported main-continuous
    /// alias, the underlying is unknown, or no main fee version covers `at`.
    pub fn main_contract_fee_asof(
        &self,
        symbol: &str,
        at: &str,
    ) -> Result<&ContractFee, FutureMetaError> {
        let parsed = parse_symbol(symbol)?;
        if parsed.kind != SymbolKind::MainContinuous {
            return Err(FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()));
        }

        let underlying = parsed
            .underlying_symbol
            .ok_or_else(|| FutureMetaError::UnsupportedSymbolKind(symbol.to_owned()))?;
        let fees = self.underlying_fees_asof(&underlying, at)?;

        fees.into_iter()
            .find(|fee| fee.is_main_contract)
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    /// Return contract metadata in archive order.
    #[must_use]
    pub fn contracts(&self) -> &[Contract] {
        &self.archive.contracts
    }

    fn fee_for_contract_id_asof(
        &self,
        contract_id: u32,
        at: OffsetDateTime,
    ) -> Option<&ContractFee> {
        let indexes = self.fees_by_contract.get(&contract_id)?;
        let position = indexes.partition_point(|index| index.valid_from <= at);
        if position == 0 {
            return None;
        }

        let index = &indexes[position - 1];
        let fee = &self.archive.fee_versions[index.archive_index];
        if index.valid_to.is_none_or(|end| at < end) {
            Some(fee)
        } else {
            None
        }
    }

    fn contract_fee_for_underlying_asof(
        &self,
        contract_id: u32,
        at: OffsetDateTime,
    ) -> Option<&ContractFee> {
        let contract = self.contract_index_by_id.get(&contract_id)?;
        if !contract_is_listed_at(contract, exchange_date(at)) {
            return None;
        }

        let fee = self.fee_for_contract_id_asof(contract_id, at)?;
        if fee.trading_status == TradingStatus::Trading {
            Some(fee)
        } else {
            None
        }
    }
}

fn contract_is_listed_at(contract: &ContractIndex, at_date: Date) -> bool {
    if contract
        .listing_date
        .is_some_and(|listing_date| at_date < listing_date)
    {
        return false;
    }

    if contract
        .expiry_date
        .is_some_and(|expiry_date| at_date > expiry_date)
    {
        return false;
    }

    true
}

fn parse_query_timestamp(value: &str) -> Result<OffsetDateTime, FutureMetaError> {
    OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|err| FutureMetaError::InvalidTimestamp(format!("{value}: {err}")))
}

fn parse_archive_timestamp(field: &str, value: &str) -> Result<OffsetDateTime, FutureMetaError> {
    OffsetDateTime::parse(value, &Rfc3339).map_err(|err| {
        FutureMetaError::CorruptArchive(format!("invalid {field} timestamp {value}: {err}"))
    })
}

fn parse_optional_archive_timestamp(
    field: &str,
    value: Option<&str>,
) -> Result<Option<OffsetDateTime>, FutureMetaError> {
    value
        .map(|value| parse_archive_timestamp(field, value))
        .transpose()
}

fn parse_optional_archive_date(
    field: &str,
    value: Option<&str>,
) -> Result<Option<Date>, FutureMetaError> {
    value
        .map(|value| parse_archive_date(field, value))
        .transpose()
}

fn parse_archive_date(field: &str, value: &str) -> Result<Date, FutureMetaError> {
    let bytes = value.as_bytes();
    if bytes.len() != 8 || !bytes.iter().all(u8::is_ascii_digit) {
        return Err(FutureMetaError::CorruptArchive(format!(
            "invalid {field} date {value}"
        )));
    }

    let year = i32::from(parse_ascii_digits(&bytes[0..4])?);
    let month = parse_ascii_digits(&bytes[4..6])?;
    let day = parse_ascii_digits(&bytes[6..8])?;
    let month = u8::try_from(month).map_err(|err| {
        FutureMetaError::CorruptArchive(format!("invalid {field} date {value}: {err}"))
    })?;
    let day = u8::try_from(day).map_err(|err| {
        FutureMetaError::CorruptArchive(format!("invalid {field} date {value}: {err}"))
    })?;
    let month = Month::try_from(month).map_err(|err| {
        FutureMetaError::CorruptArchive(format!("invalid {field} date {value}: {err}"))
    })?;

    Date::from_calendar_date(year, month, day).map_err(|err| {
        FutureMetaError::CorruptArchive(format!("invalid {field} date {value}: {err}"))
    })
}

fn parse_ascii_digits(bytes: &[u8]) -> Result<u16, FutureMetaError> {
    bytes.iter().try_fold(0_u16, |value, byte| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(u16::from(byte - b'0')))
            .ok_or_else(|| FutureMetaError::CorruptArchive("date component overflow".to_owned()))
    })
}

fn exchange_date(at: OffsetDateTime) -> Date {
    at.to_offset(exchange_offset()).date()
}

fn exchange_offset() -> UtcOffset {
    UtcOffset::from_hms(8, 0, 0).expect("valid exchange UTC offset")
}
