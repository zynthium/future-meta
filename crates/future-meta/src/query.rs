//! Query API entry points.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::FutureMetaError;
use crate::model::{Contract, ContractFee, FeeArchiveV1, FeeKind, FeeSpec, TradingStatus};
use crate::symbol::{SymbolKind, derive_underlying_symbol, parse_symbol};
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime, UtcOffset};

/// High-performance local future-meta query client.
#[derive(Debug, Clone)]
pub struct FutureMeta {
    archive: FeeArchiveV1,
    handle_token: u64,
    history_start: OffsetDateTime,
    history_start_unix_nanos: i64,
    history_start_date: Date,
    contract_by_symbol: HashMap<String, ContractHandle>,
    contract_indexes: Vec<ContractIndex>,
    fee_indexes_by_contract: Vec<Vec<FeeVersionIndex>>,
    contracts_by_underlying: HashMap<String, Vec<ContractHandle>>,
}

/// Pre-resolved contract reference for high-frequency query paths.
///
/// A handle is valid only for the `FutureMeta` that produced it and clones of
/// that client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractHandle {
    handle_token: u64,
    index: usize,
    contract_id: u32,
}

static NEXT_HANDLE_TOKEN: AtomicU64 = AtomicU64::new(1);

fn next_handle_token() -> u64 {
    NEXT_HANDLE_TOKEN.fetch_add(1, Ordering::Relaxed)
}

impl ContractHandle {
    /// Return the archive-local contract id.
    #[must_use]
    pub const fn contract_id(self) -> u32 {
        self.contract_id
    }
}

/// Fee rule compiled for tight backtest loops.
///
/// Each fee leg is represented as `lots * (fixed_per_lot + price_rate * price)`.
/// Fixed CNY-per-lot fees set `price_rate` to zero; turnover-rate fees set
/// `fixed_per_lot` to zero and pre-multiply the contract lot size.
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct PreparedFee {
    lot_size: f64,
    open_fixed_per_lot: f64,
    open_price_rate: f64,
    close_yesterday_fixed_per_lot: f64,
    close_yesterday_price_rate: f64,
    close_today_fixed_per_lot: f64,
    close_today_price_rate: f64,
}

impl PreparedFee {
    /// Contract lot size used when compiling turnover-rate fees.
    #[must_use]
    #[inline]
    pub const fn lot_size(&self) -> f64 {
        self.lot_size
    }

    /// Fee for opening `lots` lots at `price`.
    #[must_use]
    #[inline]
    pub fn open_amount(&self, price: f64, lots: f64) -> f64 {
        lots * (self.open_fixed_per_lot + self.open_price_rate * price)
    }

    /// Fee for closing yesterday positions.
    #[must_use]
    #[inline]
    pub fn close_yesterday_amount(&self, price: f64, lots: f64) -> f64 {
        lots * (self.close_yesterday_fixed_per_lot + self.close_yesterday_price_rate * price)
    }

    /// Fee for closing today positions.
    #[must_use]
    #[inline]
    pub fn close_today_amount(&self, price: f64, lots: f64) -> f64 {
        lots * (self.close_today_fixed_per_lot + self.close_today_price_rate * price)
    }
}

/// Dense prepared fee table. Slot order matches the handles passed to
/// `TradingDayMeta::prepare_fee_book`.
#[derive(Debug, Clone)]
pub struct PreparedFeeBook {
    fees: Vec<PreparedFee>,
}

impl PreparedFeeBook {
    /// Prepared fees in caller-supplied slot order.
    #[must_use]
    pub fn fees(&self) -> &[PreparedFee] {
        &self.fees
    }

    /// Return a prepared fee by slot.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<&PreparedFee> {
        self.fees.get(slot)
    }

    /// Number of prepared fee slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.fees.len()
    }

    /// Whether the book contains no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fees.is_empty()
    }
}

/// Monotonic exact-as-of fee cursor for one contract in one trading day.
///
/// Use this when tick timestamps are sorted and intraday fee changes must be
/// respected. The hot path is one integer comparison against
/// `next_change_unix_nanos`; call `advance_to_unix_nanos` only when the tick
/// reaches that boundary. Ticks must be monotonic; this cursor does not validate
/// every timestamp on the fastest path.
#[derive(Debug, Clone)]
pub struct PreparedFeeCursor<'a> {
    meta: &'a FutureMeta,
    handle: ContractHandle,
    fee_indexes: &'a [FeeVersionIndex],
    position: usize,
    current: PreparedFee,
    current_valid_from_unix_nanos: i64,
    next_change_unix_nanos: i64,
    trading_day_start_unix_nanos: i64,
    trading_day_end_unix_nanos: i64,
}

impl PreparedFeeCursor<'_> {
    /// Current prepared fee.
    #[must_use]
    #[inline]
    pub const fn current(&self) -> &PreparedFee {
        &self.current
    }

    /// Unix timestamp in nanoseconds where current fee may change.
    ///
    /// `i64::MAX` means no known change. For cursors built from
    /// `TradingDayMeta`, this is capped at that trading day's end.
    #[must_use]
    #[inline]
    pub const fn next_change_unix_nanos(&self) -> i64 {
        self.next_change_unix_nanos
    }

    /// Advance cursor to a monotonic Unix timestamp in nanoseconds.
    ///
    /// Call this only when `unix_nanos >= next_change_unix_nanos()`.
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp is before the current fee interval,
    /// leaves this trading day, or no fee version covers the timestamp.
    pub fn advance_to_unix_nanos(&mut self, unix_nanos: i64) -> Result<(), FutureMetaError> {
        if unix_nanos < self.trading_day_start_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is outside cursor trading day"
            )));
        }

        if unix_nanos < self.current_valid_from_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor interval"
            )));
        }

        if unix_nanos >= self.trading_day_end_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is outside cursor trading day"
            )));
        }

        if unix_nanos < self.next_change_unix_nanos {
            return Ok(());
        }

        self.advance_slow_to_unix_nanos(unix_nanos)
    }

    /// Advance cursor to a pre-parsed timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp cannot fit in `i64` nanoseconds, is
    /// before the current fee interval, leaves this trading day, or no fee
    /// version covers the timestamp.
    pub fn advance_to(&mut self, at: OffsetDateTime) -> Result<(), FutureMetaError> {
        let unix_nanos = query_timestamp_unix_nanos(at)?;
        self.advance_to_unix_nanos(unix_nanos)
    }

    /// Advance if needed and return the current fee.
    ///
    /// This convenience method keeps the hot-path branch inside the method. For
    /// absolute minimum overhead, compare `next_change_unix_nanos()` in the
    /// caller and use `current()` directly.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `advance_to_unix_nanos`.
    pub fn advance_and_get_unix_nanos(
        &mut self,
        unix_nanos: i64,
    ) -> Result<&PreparedFee, FutureMetaError> {
        if unix_nanos < self.trading_day_start_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is outside cursor trading day"
            )));
        }

        if unix_nanos < self.current_valid_from_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor interval"
            )));
        }

        if unix_nanos >= self.next_change_unix_nanos {
            self.advance_to_unix_nanos(unix_nanos)?;
        }
        Ok(&self.current)
    }

    /// Advance if needed and return the current fee.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `advance_to`.
    pub fn advance_and_get(&mut self, at: OffsetDateTime) -> Result<&PreparedFee, FutureMetaError> {
        let unix_nanos = query_timestamp_unix_nanos(at)?;
        self.advance_and_get_unix_nanos(unix_nanos)
    }

    fn advance_slow_to_unix_nanos(&mut self, unix_nanos: i64) -> Result<(), FutureMetaError> {
        let mut position = self.position;
        while let Some(next) = self.fee_indexes.get(position + 1) {
            if next.valid_from_unix_nanos > unix_nanos {
                break;
            }
            position += 1;
        }

        let index = &self.fee_indexes[position];
        if !fee_index_covers_unix_nanos(index, unix_nanos) {
            return Err(FutureMetaError::NoVersionAt(
                self.meta.contract_symbol(self.handle)?.to_owned(),
            ));
        }

        if position != self.position {
            let fee = &self.meta.archive.fee_versions[index.archive_index];
            self.current = self.meta.prepare_contract_fee(self.handle, fee)?;
            self.position = position;
            self.current_valid_from_unix_nanos = index.valid_from_unix_nanos;
        }
        self.next_change_unix_nanos =
            next_change_unix_nanos(self.fee_indexes, position, self.trading_day_end_unix_nanos);

        Ok(())
    }
}

/// Dense set of exact-as-of cursors that can advance across trading days.
///
/// Slot order matches the handles passed to `FutureMeta::prepare_fee_cursors`
/// or `TradingDayMeta::prepare_fee_cursors`. Prefer constructing it through
/// `FutureMeta::prepare_fee_cursors` for multi-day tick streams: compare
/// `next_change_unix_nanos`, call `advance_to` only at that boundary, then read
/// the current prepared fee by slot.
#[derive(Debug, Clone)]
pub struct PreparedFeeCursors<'a> {
    meta: &'a FutureMeta,
    handles: Vec<ContractHandle>,
    trading_date: Date,
    trading_day_start_unix_nanos: i64,
    trading_day_end_unix_nanos: i64,
    last_unix_nanos: i64,
    next_change_unix_nanos: i64,
    cursors: Vec<PreparedFeeCursor<'a>>,
}

impl<'a> PreparedFeeCursors<'a> {
    /// Current exchange-local trading date for this cursor table.
    #[must_use]
    pub const fn current_trading_date(&self) -> Date {
        self.trading_date
    }

    /// Unix timestamp in nanoseconds where any slot may need to change.
    ///
    /// This is the minimum of all slot fee-version boundaries and the current
    /// trading-day end.
    #[must_use]
    #[inline]
    pub const fn next_change_unix_nanos(&self) -> i64 {
        self.next_change_unix_nanos
    }

    /// Number of cursor slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.cursors.len()
    }

    /// Whether the cursor table contains no slots.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cursors.is_empty()
    }

    /// Return a cursor by slot.
    #[must_use]
    pub fn get(&self, slot: usize) -> Option<&PreparedFeeCursor<'a>> {
        self.cursors.get(slot)
    }

    /// Return the current prepared fee by slot.
    ///
    /// # Errors
    ///
    /// Returns an error when `slot` is missing.
    pub fn current(&self, slot: usize) -> Result<&PreparedFee, FutureMetaError> {
        self.cursors
            .get(slot)
            .map(PreparedFeeCursor::current)
            .ok_or(FutureMetaError::InvalidFeeSlot(slot))
    }

    /// Advance the cursor table to a monotonic timestamp and trading date.
    ///
    /// Rebuilds the internal day snapshot when `trading_date` changes. Within
    /// the same day, only slots whose fee boundary has been reached take the
    /// slow path.
    ///
    /// # Errors
    ///
    /// Returns an error when the timestamp predates history, falls outside the
    /// supplied trading date, moves outside the current same-day bounds, or any
    /// slot has no fee version at the requested timestamp.
    pub fn advance_to(
        &mut self,
        trading_date: Date,
        unix_nanos: i64,
    ) -> Result<(), FutureMetaError> {
        self.meta.reject_unix_nanos_before_history(unix_nanos)?;
        if unix_nanos < self.last_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor table position"
            )));
        }

        if trading_date != self.trading_date {
            let next = self.meta.prepare_fee_cursors(
                self.handles.iter().copied(),
                trading_date,
                unix_nanos,
            )?;
            *self = next;
            return Ok(());
        }

        if unix_nanos < self.trading_day_start_unix_nanos
            || unix_nanos >= self.trading_day_end_unix_nanos
        {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is outside trading day {}",
                self.trading_date
            )));
        }

        let mut cursors = self.cursors.clone();
        for cursor in &mut cursors {
            if unix_nanos >= cursor.next_change_unix_nanos() {
                cursor.advance_to_unix_nanos(unix_nanos)?;
            }
        }
        self.cursors = cursors;
        self.next_change_unix_nanos =
            next_cursor_table_change_unix_nanos(&self.cursors, self.trading_day_end_unix_nanos);
        self.last_unix_nanos = unix_nanos;

        Ok(())
    }

    /// Advance if needed and return the current prepared fee by slot.
    ///
    /// This is convenient but keeps a branch and error path in the loop. For
    /// the tightest loop, compare `next_change_unix_nanos()` in caller code and
    /// use `current()`.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `advance_to`, plus `InvalidFeeSlot` for a
    /// missing slot.
    pub fn advance_and_get(
        &mut self,
        trading_date: Date,
        slot: usize,
        unix_nanos: i64,
    ) -> Result<&PreparedFee, FutureMetaError> {
        if trading_date != self.trading_date
            || unix_nanos >= self.next_change_unix_nanos
            || unix_nanos < self.last_unix_nanos
        {
            self.advance_to(trading_date, unix_nanos)?;
        }
        self.current(slot)
    }

    /// Advance a cursor by slot and return its current fee within the current
    /// trading day.
    ///
    /// # Errors
    ///
    /// Returns an error when the slot is missing, cursor advancement fails, or
    /// the timestamp reaches a cross-day boundary. Prefer `advance_and_get` for
    /// multi-day tick streams.
    pub fn advance_and_get_unix_nanos(
        &mut self,
        slot: usize,
        unix_nanos: i64,
    ) -> Result<&PreparedFee, FutureMetaError> {
        if unix_nanos < self.last_unix_nanos {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor table position"
            )));
        }

        let cursor = self
            .cursors
            .get_mut(slot)
            .ok_or(FutureMetaError::InvalidFeeSlot(slot))?;
        if unix_nanos >= cursor.next_change_unix_nanos() {
            cursor.advance_to_unix_nanos(unix_nanos)?;
            self.next_change_unix_nanos =
                next_cursor_table_change_unix_nanos(&self.cursors, self.trading_day_end_unix_nanos);
        }
        self.last_unix_nanos = unix_nanos;
        self.current(slot)
    }
}

/// Precomputed fee snapshot for one exchange-local trading date.
///
/// Precomputed single-day fee snapshot.
///
/// This is an advanced API for callers that already partition ticks by trading
/// day. Multi-day backtests should prefer `FutureMeta::prepare_fee_cursors`.
#[derive(Debug)]
pub struct TradingDayMeta<'a> {
    meta: &'a FutureMeta,
    trading_date: Date,
    trading_day_start_unix_nanos: i64,
    trading_day_end_unix_nanos: i64,
    fee_archive_indexes_by_contract: Vec<Option<usize>>,
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
    valid_from_unix_nanos: i64,
    valid_to_unix_nanos: Option<i64>,
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
        let handle_token = next_handle_token();
        let history_start = parse_archive_timestamp("history_start", &archive.history_start)?;
        let history_start_unix_nanos =
            archive_timestamp_unix_nanos("history_start", history_start)?;
        let history_start_date = exchange_date(history_start);
        let mut contract_by_symbol = HashMap::with_capacity(archive.contracts.len());
        let mut contract_indexes = Vec::with_capacity(archive.contracts.len());
        let mut contract_handle_by_id = HashMap::with_capacity(archive.contracts.len());
        let mut contracts_by_underlying: HashMap<String, Vec<ContractHandle>> = HashMap::new();

        for (index, contract) in archive.contracts.iter().enumerate() {
            let handle = ContractHandle {
                handle_token,
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
            let valid_from_unix_nanos = archive_timestamp_unix_nanos("valid_from", valid_from)?;
            let valid_to_unix_nanos = valid_to
                .map(|valid_to| archive_timestamp_unix_nanos("valid_to", valid_to))
                .transpose()?;
            fee_indexes_by_contract[handle.index].push(FeeVersionIndex {
                archive_index: index,
                valid_from,
                valid_to,
                valid_from_unix_nanos,
                valid_to_unix_nanos,
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
            handle_token,
            history_start,
            history_start_unix_nanos,
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

    /// Build a precomputed fee snapshot for one exchange-local trading date.
    ///
    /// Use `TradingDayMeta::prepare_fee` for fee rules that are fixed within
    /// the trading day, or `TradingDayMeta::prepare_fee_cursor` when same-day
    /// fee boundaries must be respected.
    ///
    /// # Errors
    ///
    /// Returns an error when `trading_date` predates the archive history.
    pub fn for_trading_day(
        &self,
        trading_date: Date,
    ) -> Result<TradingDayMeta<'_>, FutureMetaError> {
        self.reject_date_before_history(trading_date)?;
        let trading_day_start_unix_nanos = exchange_midnight_unix_nanos(trading_date)?;
        let next_trading_date = trading_date.next_day().ok_or_else(|| {
            FutureMetaError::InvalidTimestamp(format!("{trading_date} has no next day"))
        })?;
        let trading_day_end_unix_nanos = exchange_midnight_unix_nanos(next_trading_date)?;
        let mut fee_archive_indexes_by_contract = Vec::with_capacity(self.contract_indexes.len());

        for (index, contract) in self.contract_indexes.iter().enumerate() {
            let handle = ContractHandle {
                handle_token: self.handle_token,
                index,
                contract_id: contract.contract_id,
            };
            fee_archive_indexes_by_contract
                .push(self.fee_archive_index_for_contract_handle_on(handle, trading_date)?);
        }

        Ok(TradingDayMeta {
            meta: self,
            trading_date,
            trading_day_start_unix_nanos,
            trading_day_end_unix_nanos,
            fee_archive_indexes_by_contract,
        })
    }

    /// Prepare dense exact-as-of fee cursors for a multi-day tick stream.
    ///
    /// This is the preferred high-frequency API when backtests can cross
    /// trading days. Resolve handles once, construct this table once at the
    /// first tick, then call `PreparedFeeCursors::advance_to` only when
    /// `next_change_unix_nanos` is reached.
    ///
    /// # Errors
    ///
    /// Returns an error when `trading_date` predates history, `start_unix_nanos`
    /// is outside that trading day, any handle is invalid, any slot has no fee
    /// version at the start timestamp, or a fee rule cannot be compiled to
    /// numeric coefficients.
    pub fn prepare_fee_cursors(
        &self,
        handles: impl IntoIterator<Item = ContractHandle>,
        trading_date: Date,
        start_unix_nanos: i64,
    ) -> Result<PreparedFeeCursors<'_>, FutureMetaError> {
        let day = self.for_trading_day(trading_date)?;
        day.prepare_fee_cursors(handles, start_unix_nanos)
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

    fn reject_unix_nanos_before_history(&self, unix_nanos: i64) -> Result<(), FutureMetaError> {
        if unix_nanos < self.history_start_unix_nanos {
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
            .filter(|index| {
                handle.handle_token == self.handle_token && index.contract_id == handle.contract_id
            })
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
        let archive_index = self.fee_archive_index_for_contract_handle_on(handle, trading_date)?;
        Ok(archive_index.map(|index| &self.archive.fee_versions[index]))
    }

    fn fee_archive_index_for_contract_handle_on(
        &self,
        handle: ContractHandle,
        trading_date: Date,
    ) -> Result<Option<usize>, FutureMetaError> {
        let indexes = self.fee_indexes_for_handle(handle)?;
        let position = indexes.partition_point(|index| index.valid_from_date <= trading_date);
        if position == 0 {
            return Ok(None);
        }

        let index = &indexes[position - 1];
        if index.valid_to_date.is_none_or(|end| trading_date < end) {
            Ok(Some(index.archive_index))
        } else {
            Ok(None)
        }
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

    fn prepare_contract_fee(
        &self,
        handle: ContractHandle,
        fee: &ContractFee,
    ) -> Result<PreparedFee, FutureMetaError> {
        self.contract_index(handle)?;
        let contract = self
            .archive
            .contracts
            .get(handle.index)
            .ok_or(FutureMetaError::InvalidContractHandle)?;
        prepare_fee(contract.symbol.as_str(), contract.lot_size, fee)
    }
}

impl<'a> TradingDayMeta<'a> {
    /// Return the exchange-local trading date for this snapshot.
    #[must_use]
    pub const fn trading_date(&self) -> Date {
        self.trading_date
    }

    /// Resolve a concrete contract symbol for repeated hot-path queries.
    ///
    /// # Errors
    ///
    /// Returns an error when `symbol` is not present in the archive.
    pub fn resolve_contract(&self, symbol: &str) -> Result<ContractHandle, FutureMetaError> {
        self.meta.resolve_contract(symbol)
    }

    /// Return the fee rule for a pre-resolved contract in this trading day.
    ///
    /// # Errors
    ///
    /// Returns an error when the handle is invalid for this client or no fee
    /// version covers this trading day.
    pub fn fee_rule(&self, handle: ContractHandle) -> Result<&'a ContractFee, FutureMetaError> {
        self.meta.contract_index(handle)?;
        let symbol = self.meta.contract_symbol(handle)?;
        let archive_index = self
            .fee_archive_indexes_by_contract
            .get(handle.index)
            .copied()
            .flatten();

        archive_index
            .map(|index| &self.meta.archive.fee_versions[index])
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))
    }

    /// Prepare the current daily fee rule for a hot loop.
    ///
    /// # Errors
    ///
    /// Returns an error when the handle is invalid, no fee version covers this
    /// trading day, or the fee rule cannot be compiled to numeric coefficients.
    pub fn prepare_fee(&self, handle: ContractHandle) -> Result<PreparedFee, FutureMetaError> {
        let fee = self.fee_rule(handle)?;
        self.meta.prepare_contract_fee(handle, fee)
    }

    /// Return the fee rule for a concrete contract symbol in this trading day.
    ///
    /// # Errors
    ///
    /// Returns an error when the contract is unknown or no fee version covers
    /// this trading day.
    pub fn fee_rule_by_symbol(&self, symbol: &str) -> Result<&'a ContractFee, FutureMetaError> {
        let handle = self.resolve_contract(symbol)?;
        self.fee_rule(handle)
    }

    /// Prepare the current daily fee rule for a concrete contract symbol.
    ///
    /// # Errors
    ///
    /// Returns an error when the contract is unknown, no fee version covers
    /// this trading day, or the fee rule cannot be compiled to numeric
    /// coefficients.
    pub fn prepare_fee_by_symbol(&self, symbol: &str) -> Result<PreparedFee, FutureMetaError> {
        let handle = self.resolve_contract(symbol)?;
        self.prepare_fee(handle)
    }

    /// Prepare a dense fee book in caller-supplied handle order.
    ///
    /// # Errors
    ///
    /// Returns an error when any handle cannot be prepared.
    pub fn prepare_fee_book(
        &self,
        handles: impl IntoIterator<Item = ContractHandle>,
    ) -> Result<PreparedFeeBook, FutureMetaError> {
        let fees = handles
            .into_iter()
            .map(|handle| self.prepare_fee(handle))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PreparedFeeBook { fees })
    }

    /// Prepare an exact-as-of fee cursor at a Unix timestamp in nanoseconds.
    ///
    /// Ticks passed to the cursor must be monotonic and stay inside this trading
    /// day. For maximum hot-path performance, store tick timestamps as `i64`
    /// Unix nanoseconds before entering the backtest loop.
    ///
    /// # Errors
    ///
    /// Returns an error when `start_unix_nanos` predates history, is outside
    /// this trading day, the handle is invalid, no fee version covers the start
    /// timestamp, or the fee rule cannot be compiled to numeric coefficients.
    pub fn prepare_fee_cursor(
        &self,
        handle: ContractHandle,
        start_unix_nanos: i64,
    ) -> Result<PreparedFeeCursor<'a>, FutureMetaError> {
        self.meta
            .reject_unix_nanos_before_history(start_unix_nanos)?;
        self.reject_unix_nanos_outside_trading_day(start_unix_nanos)?;
        let fee_indexes = self.meta.fee_indexes_for_handle(handle)?;
        let symbol = self.meta.contract_symbol(handle)?;
        let position = fee_position_for_unix_nanos(fee_indexes, start_unix_nanos)
            .ok_or_else(|| FutureMetaError::NoVersionAt(symbol.to_owned()))?;
        let index = &fee_indexes[position];
        let fee = &self.meta.archive.fee_versions[index.archive_index];
        let current = self.meta.prepare_contract_fee(handle, fee)?;
        let next_change_unix_nanos =
            next_change_unix_nanos(fee_indexes, position, self.trading_day_end_unix_nanos);

        Ok(PreparedFeeCursor {
            meta: self.meta,
            handle,
            fee_indexes,
            position,
            current,
            current_valid_from_unix_nanos: index.valid_from_unix_nanos,
            next_change_unix_nanos,
            trading_day_start_unix_nanos: self.trading_day_start_unix_nanos,
            trading_day_end_unix_nanos: self.trading_day_end_unix_nanos,
        })
    }

    /// Prepare an exact-as-of fee cursor at a pre-parsed timestamp.
    ///
    /// # Errors
    ///
    /// Returns an error when `start` is outside this trading day, cannot fit in
    /// `i64` nanoseconds, or `prepare_fee_cursor` fails.
    pub fn prepare_fee_cursor_at(
        &self,
        handle: ContractHandle,
        start: OffsetDateTime,
    ) -> Result<PreparedFeeCursor<'a>, FutureMetaError> {
        if exchange_date(start) != self.trading_date {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{start} is outside trading day {}",
                self.trading_date
            )));
        }
        let start_unix_nanos = query_timestamp_unix_nanos(start)?;
        self.prepare_fee_cursor(handle, start_unix_nanos)
    }

    /// Prepare dense exact-as-of fee cursors in caller-supplied handle order.
    ///
    /// # Errors
    ///
    /// Returns an error when any cursor cannot be prepared.
    pub fn prepare_fee_cursors(
        &self,
        handles: impl IntoIterator<Item = ContractHandle>,
        start_unix_nanos: i64,
    ) -> Result<PreparedFeeCursors<'a>, FutureMetaError> {
        self.meta
            .reject_unix_nanos_before_history(start_unix_nanos)?;
        self.reject_unix_nanos_outside_trading_day(start_unix_nanos)?;
        let handles = handles.into_iter().collect::<Vec<_>>();
        let cursors = handles
            .iter()
            .copied()
            .map(|handle| self.prepare_fee_cursor(handle, start_unix_nanos))
            .collect::<Result<Vec<_>, _>>()?;
        let next_change_unix_nanos =
            next_cursor_table_change_unix_nanos(&cursors, self.trading_day_end_unix_nanos);
        Ok(PreparedFeeCursors {
            meta: self.meta,
            handles,
            trading_date: self.trading_date,
            trading_day_start_unix_nanos: self.trading_day_start_unix_nanos,
            trading_day_end_unix_nanos: self.trading_day_end_unix_nanos,
            last_unix_nanos: start_unix_nanos,
            next_change_unix_nanos,
            cursors,
        })
    }

    fn reject_unix_nanos_outside_trading_day(
        &self,
        unix_nanos: i64,
    ) -> Result<(), FutureMetaError> {
        if unix_nanos < self.trading_day_start_unix_nanos
            || unix_nanos >= self.trading_day_end_unix_nanos
        {
            return Err(FutureMetaError::InvalidTimestamp(format!(
                "{unix_nanos} is outside trading day {}",
                self.trading_date
            )));
        }
        Ok(())
    }
}

fn prepare_fee(
    symbol: &str,
    lot_size: f64,
    fee: &ContractFee,
) -> Result<PreparedFee, FutureMetaError> {
    if !lot_size.is_finite() || lot_size <= 0.0 {
        return Err(FutureMetaError::UnsupportedFeeRule(format!(
            "{symbol} lot_size: {lot_size}"
        )));
    }

    let (open_fixed_per_lot, open_price_rate) =
        prepare_fee_spec(symbol, "open_fee", lot_size, &fee.open_fee)?;
    let (close_yesterday_fixed_per_lot, close_yesterday_price_rate) = prepare_fee_spec(
        symbol,
        "close_yesterday_fee",
        lot_size,
        &fee.close_yesterday_fee,
    )?;
    let (close_today_fixed_per_lot, close_today_price_rate) =
        prepare_fee_spec(symbol, "close_today_fee", lot_size, &fee.close_today_fee)?;

    Ok(PreparedFee {
        lot_size,
        open_fixed_per_lot,
        open_price_rate,
        close_yesterday_fixed_per_lot,
        close_yesterday_price_rate,
        close_today_fixed_per_lot,
        close_today_price_rate,
    })
}

fn prepare_fee_spec(
    symbol: &str,
    field: &str,
    lot_size: f64,
    spec: &FeeSpec,
) -> Result<(f64, f64), FutureMetaError> {
    match spec.kind {
        FeeKind::CnyPerLot => {
            let value = finite_fee_value(symbol, field, spec)?;
            Ok((value, 0.0))
        }
        FeeKind::TurnoverRatePerTenThousand => {
            let value = finite_fee_value(symbol, field, spec)?;
            Ok((0.0, lot_size * value / 10_000.0))
        }
        FeeKind::Zero => Ok((0.0, 0.0)),
        FeeKind::Unknown => Err(unsupported_fee_rule(symbol, field, spec)),
    }
}

fn finite_fee_value(symbol: &str, field: &str, spec: &FeeSpec) -> Result<f64, FutureMetaError> {
    spec.value
        .filter(|value| value.is_finite())
        .ok_or_else(|| unsupported_fee_rule(symbol, field, spec))
}

fn unsupported_fee_rule(symbol: &str, field: &str, spec: &FeeSpec) -> FutureMetaError {
    FutureMetaError::UnsupportedFeeRule(format!(
        "{symbol} {field}: {:?}",
        spec.raw_text.as_deref().unwrap_or("missing value")
    ))
}

fn fee_position_for_unix_nanos(indexes: &[FeeVersionIndex], unix_nanos: i64) -> Option<usize> {
    let position = indexes.partition_point(|index| index.valid_from_unix_nanos <= unix_nanos);
    if position == 0 {
        return None;
    }

    let index_position = position - 1;
    let index = &indexes[index_position];
    fee_index_covers_unix_nanos(index, unix_nanos).then_some(index_position)
}

fn fee_index_covers_unix_nanos(index: &FeeVersionIndex, unix_nanos: i64) -> bool {
    index.valid_to_unix_nanos.is_none_or(|end| unix_nanos < end)
}

fn next_change_unix_nanos(
    indexes: &[FeeVersionIndex],
    position: usize,
    trading_day_end_unix_nanos: i64,
) -> i64 {
    let index = &indexes[position];
    let next_fee_start = indexes
        .get(position + 1)
        .map_or(i64::MAX, |next| next.valid_from_unix_nanos);
    index
        .valid_to_unix_nanos
        .unwrap_or(i64::MAX)
        .min(next_fee_start)
        .min(trading_day_end_unix_nanos)
}

fn next_cursor_table_change_unix_nanos(
    cursors: &[PreparedFeeCursor<'_>],
    trading_day_end_unix_nanos: i64,
) -> i64 {
    cursors
        .iter()
        .map(PreparedFeeCursor::next_change_unix_nanos)
        .min()
        .unwrap_or(trading_day_end_unix_nanos)
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

fn archive_timestamp_unix_nanos(field: &str, at: OffsetDateTime) -> Result<i64, FutureMetaError> {
    i64::try_from(at.unix_timestamp_nanos()).map_err(|err| {
        FutureMetaError::CorruptArchive(format!(
            "{field} timestamp {at} is outside i64 unix nanosecond range: {err}"
        ))
    })
}

fn query_timestamp_unix_nanos(at: OffsetDateTime) -> Result<i64, FutureMetaError> {
    i64::try_from(at.unix_timestamp_nanos()).map_err(|err| {
        FutureMetaError::InvalidTimestamp(format!(
            "{at} is outside i64 unix nanosecond range: {err}"
        ))
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

fn exchange_midnight_unix_nanos(date: Date) -> Result<i64, FutureMetaError> {
    query_timestamp_unix_nanos(date.midnight().assume_offset(exchange_offset()))
}

fn exchange_offset() -> UtcOffset {
    UtcOffset::from_hms(8, 0, 0).expect("valid exchange UTC offset")
}
