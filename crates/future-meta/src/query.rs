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
    history_start_date: Date,
    contract_by_symbol: HashMap<String, ContractHandle>,
    contract_indexes: Vec<ContractIndex>,
    fee_indexes_by_contract: Vec<Vec<FeeVersionIndex>>,
    contracts_by_underlying: HashMap<String, Vec<ContractHandle>>,
}

/// Pre-resolved contract reference for high-frequency query paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractHandle {
    index: usize,
    contract_id: u32,
}

impl ContractHandle {
    /// Return the archive-local contract id.
    #[must_use]
    pub const fn contract_id(self) -> u32 {
        self.contract_id
    }
}

#[derive(Debug, Clone)]
struct ContractIndex {
    contract_id: u32,
    listing_date: Option<Date>,
    expiry_date: Option<Date>,
}

#[derive(Debug, Clone)]
struct FeeVersionIndex {
    archive_index: usize,
    valid_from: OffsetDateTime,
    valid_to: Option<OffsetDateTime>,
    valid_from_date: Date,
    valid_to_date: Option<Date>,
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
        let history_start_date = exchange_date(history_start);
        let mut contract_by_symbol = HashMap::with_capacity(archive.contracts.len());
        let mut contract_indexes = Vec::with_capacity(archive.contracts.len());
        let mut contract_handle_by_id = HashMap::with_capacity(archive.contracts.len());
        let mut contracts_by_underlying: HashMap<String, Vec<ContractHandle>> = HashMap::new();

        for (index, contract) in archive.contracts.iter().enumerate() {
            let handle = ContractHandle {
                index,
                contract_id: contract.id,
            };
            contract_by_symbol.insert(contract.symbol.clone(), handle);
            contract_handle_by_id.insert(contract.id, handle);
            contract_indexes.push(ContractIndex {
                contract_id: contract.id,
                listing_date: parse_optional_archive_date(
                    "listing_date",
                    contract.listing_date.as_deref(),
                )?,
                expiry_date: parse_optional_archive_date(
                    "expiry_date",
                    contract.expiry_date.as_deref(),
                )?,
            });
            let underlying = derive_underlying_symbol(&contract.symbol)?;
            contracts_by_underlying
                .entry(underlying)
                .or_default()
                .push(handle);
        }

        let mut fee_indexes_by_contract = vec![Vec::new(); archive.contracts.len()];
        for (index, fee) in archive.fee_versions.iter().enumerate() {
            let handle = contract_handle_by_id
                .get(&fee.contract_id)
                .copied()
                .ok_or_else(|| {
                    FutureMetaError::CorruptArchive(format!(
                        "fee version references unknown contract id {}",
                        fee.contract_id
                    ))
                })?;
            let valid_from = parse_archive_timestamp("valid_from", &fee.valid_from)?;
            let valid_to = parse_optional_archive_timestamp("valid_to", fee.valid_to.as_deref())?;
            fee_indexes_by_contract[handle.index].push(FeeVersionIndex {
                archive_index: index,
                valid_from,
                valid_to,
                valid_from_date: exchange_date(valid_from),
                valid_to_date: valid_to.map(exchange_date),
            });
        }
        for indexes in &mut fee_indexes_by_contract {
            indexes.sort_by(|left, right| {
                left.valid_from
                    .cmp(&right.valid_from)
                    .then_with(|| left.archive_index.cmp(&right.archive_index))
            });
        }

        Ok(Self {
            archive,
            history_start,
            history_start_date,
            contract_by_symbol,
            contract_indexes,
            fee_indexes_by_contract,
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
        self.contract_fee_at(symbol, at)
    }

    /// Resolve a concrete contract symbol once for repeated hot-path queries.
    ///
    /// # Errors
    ///
    /// Returns an error when `symbol` is not present in the archive.
    pub fn resolve_contract(&self, symbol: &str) -> Result<ContractHandle, FutureMetaError> {
        self.contract_by_symbol
            .get(symbol)
            .copied()
            .ok_or_else(|| FutureMetaError::UnknownContract(symbol.to_owned()))
    }

    /// Return the fee rule for a concrete contract at a pre-parsed timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when `at` predates the archive history, the contract is
    /// unknown, or no fee version covers `at`.
    pub fn contract_fee_at(
        &self,
        symbol: &str,
        at: OffsetDateTime,
    ) -> Result<&ContractFee, FutureMetaError> {
        self.reject_timestamp_before_history(at)?;
        let handle = self.resolve_contract(symbol)?;
        self.fee_for_contract_handle_asof(handle, at)?
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    /// Return the fee rule for a concrete contract on an exchange-local date.
    ///
    /// This is the fastest symbol-based API for callers that already work at
    /// trading-day granularity. Intraday source timestamps are normalized to the
    /// exchange-local calendar date in the in-memory index.
    ///
    /// # Errors
    ///
    /// Returns an error when `trading_date` predates the archive history, the
    /// contract is unknown, or no fee version covers `trading_date`.
    pub fn contract_fee_on(
        &self,
        symbol: &str,
        trading_date: Date,
    ) -> Result<&ContractFee, FutureMetaError> {
        self.reject_date_before_history(trading_date)?;
        let handle = self.resolve_contract(symbol)?;
        self.fee_for_contract_handle_on(handle, trading_date)?
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    /// Return the fee rule for a pre-resolved contract at a pre-parsed timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when `at` predates the archive history, the handle is
    /// invalid for this client, or no fee version covers `at`.
    pub fn contract_fee_for_handle_at(
        &self,
        handle: ContractHandle,
        at: OffsetDateTime,
    ) -> Result<&ContractFee, FutureMetaError> {
        self.reject_timestamp_before_history(at)?;
        if let Some(fee) = self.fee_for_contract_handle_asof(handle, at)? {
            return Ok(fee);
        }

        Err(FutureMetaError::NoVersionAt(
            self.contract_symbol(handle)?.to_owned(),
        ))
    }

    /// Return the fee rule for a pre-resolved contract on an exchange-local date.
    ///
    /// # Errors
    ///
    /// Returns an error when `trading_date` predates the archive history, the
    /// handle is invalid for this client, or no fee version covers
    /// `trading_date`.
    pub fn contract_fee_for_handle_on(
        &self,
        handle: ContractHandle,
        trading_date: Date,
    ) -> Result<&ContractFee, FutureMetaError> {
        self.reject_date_before_history(trading_date)?;
        if let Some(fee) = self.fee_for_contract_handle_on(handle, trading_date)? {
            return Ok(fee);
        }

        Err(FutureMetaError::NoVersionAt(
            self.contract_symbol(handle)?.to_owned(),
        ))
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
        self.reject_timestamp_before_history(at)?;

        let handles = self
            .contracts_by_underlying
            .get(underlying_symbol)
            .ok_or_else(|| {
                FutureMetaError::UnknownUnderlyingSymbol(underlying_symbol.to_owned())
            })?;

        Ok(handles
            .iter()
            .filter_map(|handle| self.contract_fee_for_underlying_asof(*handle, at))
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

    fn reject_timestamp_before_history(&self, at: OffsetDateTime) -> Result<(), FutureMetaError> {
        if at < self.history_start {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }
        Ok(())
    }

    fn reject_date_before_history(&self, trading_date: Date) -> Result<(), FutureMetaError> {
        if trading_date < self.history_start_date {
            return Err(FutureMetaError::NotAvailableBeforeHistoryStart);
        }
        Ok(())
    }

    fn contract_index(&self, handle: ContractHandle) -> Result<&ContractIndex, FutureMetaError> {
        self.contract_indexes
            .get(handle.index)
            .filter(|index| index.contract_id == handle.contract_id)
            .ok_or(FutureMetaError::InvalidContractHandle)
    }

    fn contract_symbol(&self, handle: ContractHandle) -> Result<&str, FutureMetaError> {
        self.contract_index(handle)?;
        self.archive
            .contracts
            .get(handle.index)
            .map(|contract| contract.symbol.as_str())
            .ok_or(FutureMetaError::InvalidContractHandle)
    }

    fn fee_indexes_for_handle(
        &self,
        handle: ContractHandle,
    ) -> Result<&[FeeVersionIndex], FutureMetaError> {
        self.contract_index(handle)?;
        self.fee_indexes_by_contract
            .get(handle.index)
            .map(Vec::as_slice)
            .ok_or(FutureMetaError::InvalidContractHandle)
    }

    fn fee_for_contract_handle_asof(
        &self,
        handle: ContractHandle,
        at: OffsetDateTime,
    ) -> Result<Option<&ContractFee>, FutureMetaError> {
        let indexes = self.fee_indexes_for_handle(handle)?;
        let position = indexes.partition_point(|index| index.valid_from <= at);
        if position == 0 {
            return Ok(None);
        }

        let index = &indexes[position - 1];
        let fee = &self.archive.fee_versions[index.archive_index];
        if index.valid_to.is_none_or(|end| at < end) {
            Ok(Some(fee))
        } else {
            Ok(None)
        }
    }

    fn fee_for_contract_handle_on(
        &self,
        handle: ContractHandle,
        trading_date: Date,
    ) -> Result<Option<&ContractFee>, FutureMetaError> {
        let indexes = self.fee_indexes_for_handle(handle)?;
        let fee = indexes.iter().rev().find_map(|index| {
            if index.valid_from_date <= trading_date
                && index.valid_to_date.is_none_or(|end| trading_date < end)
            {
                Some(&self.archive.fee_versions[index.archive_index])
            } else {
                None
            }
        });

        Ok(fee)
    }

    fn contract_fee_for_underlying_asof(
        &self,
        handle: ContractHandle,
        at: OffsetDateTime,
    ) -> Option<&ContractFee> {
        let contract = self.contract_index(handle).ok()?;
        if !contract_is_listed_at(contract, exchange_date(at)) {
            return None;
        }

        let fee = self.fee_for_contract_handle_asof(handle, at).ok()??;
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
