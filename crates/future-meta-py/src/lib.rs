//! Python bindings for `future-meta`.

#![allow(
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::needless_pass_by_value,
    clippy::used_underscore_items
)]

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use future_meta::error::FutureMetaError as RustError;
use future_meta::model::{
    Contract as RustContract, ContractFee as RustContractFee,
    ContractSpecVersion as RustContractSpecVersion, FeeKind as RustFeeKind, FeeSpec as RustFeeSpec,
    SCHEMA_VERSION, TradingStatus as RustTradingStatus,
};
use future_meta::query::{
    ContractHandle as RustContractHandle, FutureMeta as RustFutureMeta,
    PreparedFee as RustPreparedFee, PreparedFeeBook as RustPreparedFeeBook,
};
use future_meta::symbol::{ParsedSymbol as RustParsedSymbol, SymbolKind as RustSymbolKind};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use time::format_description::well_known::Rfc3339;
use time::{Date, Month, OffsetDateTime};

create_exception!(future_meta, FutureMetaError, PyException);

fn to_py_err(err: RustError) -> PyErr {
    FutureMetaError::new_err(err.to_string())
}

fn tokio_runtime() -> PyResult<&'static tokio::runtime::Runtime> {
    static RUNTIME: OnceLock<Result<tokio::runtime::Runtime, String>> = OnceLock::new();
    let result = RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|err| err.to_string())
    });
    match result {
        Ok(rt) => Ok(rt),
        Err(msg) => Err(FutureMetaError::new_err(format!(
            "failed to initialize tokio runtime: {msg}"
        ))),
    }
}

fn parse_trading_date(raw: &str) -> Result<Date, RustError> {
    let bytes = raw.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return Err(RustError::InvalidTimestamp(raw.to_owned()));
    }
    let year = raw[0..4]
        .parse::<i32>()
        .map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))?;
    let month_num = raw[5..7]
        .parse::<u8>()
        .map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))?;
    let day = raw[8..10]
        .parse::<u8>()
        .map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))?;
    let month =
        Month::try_from(month_num).map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))?;
    Date::from_calendar_date(year, month, day)
        .map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))
}

fn parse_rfc3339(raw: &str) -> Result<OffsetDateTime, RustError> {
    OffsetDateTime::parse(raw, &Rfc3339).map_err(|_| RustError::InvalidTimestamp(raw.to_owned()))
}

const fn fee_kind_str(kind: &RustFeeKind) -> &'static str {
    match kind {
        RustFeeKind::CnyPerLot => "CnyPerLot",
        RustFeeKind::TurnoverRatePerTenThousand => "TurnoverRatePerTenThousand",
        RustFeeKind::Zero => "Zero",
        RustFeeKind::Unknown => "Unknown",
    }
}

const fn trading_status_str(status: &RustTradingStatus) -> &'static str {
    match status {
        RustTradingStatus::Trading => "Trading",
        RustTradingStatus::NotTrading => "NotTrading",
        RustTradingStatus::Unknown => "Unknown",
    }
}

const fn symbol_kind_str(kind: &RustSymbolKind) -> &'static str {
    match kind {
        RustSymbolKind::Futures => "Futures",
        RustSymbolKind::MainContinuous => "MainContinuous",
        RustSymbolKind::Index => "Index",
        RustSymbolKind::Option => "Option",
        RustSymbolKind::Spread => "Spread",
        RustSymbolKind::Unsupported => "Unsupported",
    }
}

/// Structured representation of a parsed `TqSdk` symbol.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedSymbol {
    /// Original input string.
    #[pyo3(get)]
    pub raw: String,
    /// Parsed symbol category (`"Futures"`, `"MainContinuous"`, `"Index"`, `"Option"`, `"Spread"`, `"Unsupported"`).
    #[pyo3(get)]
    pub kind: String,
    /// Exchange or query namespace.
    #[pyo3(get)]
    pub exchange: String,
    /// Exchange-local symbol component.
    #[pyo3(get)]
    pub local: String,
    /// Underlying product symbol when derivable.
    #[pyo3(get)]
    pub underlying_symbol: Option<String>,
}

impl From<RustParsedSymbol> for ParsedSymbol {
    fn from(value: RustParsedSymbol) -> Self {
        Self {
            raw: value.raw,
            kind: symbol_kind_str(&value.kind).to_owned(),
            exchange: value.exchange,
            local: value.local,
            underlying_symbol: value.underlying_symbol,
        }
    }
}

#[pymethods]
impl ParsedSymbol {
    /// Convert to a Python dictionary.
    pub fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("raw", &self.raw)?;
        dict.set_item("kind", &self.kind)?;
        dict.set_item("exchange", &self.exchange)?;
        dict.set_item("local", &self.local)?;
        dict.set_item("underlying_symbol", &self.underlying_symbol)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "ParsedSymbol(raw={:?}, kind={:?}, exchange={:?}, local={:?}, underlying_symbol={:?})",
            self.raw, self.kind, self.exchange, self.local, self.underlying_symbol
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

/// Pre-resolved contract reference for high-frequency query paths.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ContractHandle {
    inner: RustContractHandle,
}

#[pymethods]
impl ContractHandle {
    /// Return the archive-local contract id.
    #[getter]
    pub const fn contract_id(&self) -> u32 {
        self.inner.contract_id()
    }

    fn __repr__(&self) -> String {
        format!("ContractHandle(contract_id={})", self.inner.contract_id())
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

/// Futures contract metadata.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, PartialEq)]
pub struct Contract {
    /// Stable numeric id inside the archive.
    #[pyo3(get)]
    pub id: u32,
    /// TqSdk-style contract symbol.
    #[pyo3(get)]
    pub symbol: String,
    /// Listing date when known.
    #[pyo3(get)]
    pub listing_date: Option<String>,
    /// Expiry date when known.
    #[pyo3(get)]
    pub expiry_date: Option<String>,
    /// Number of units per lot.
    #[pyo3(get)]
    pub lot_size: f64,
    /// Minimum price tick.
    #[pyo3(get)]
    pub tick_size: f64,
    /// Whether the contract is currently active.
    #[pyo3(get)]
    pub active: bool,
}

impl From<&RustContract> for Contract {
    fn from(value: &RustContract) -> Self {
        Self {
            id: value.id,
            symbol: value.symbol.clone(),
            listing_date: value.listing_date.clone(),
            expiry_date: value.expiry_date.clone(),
            lot_size: value.lot_size,
            tick_size: value.tick_size,
            active: value.active,
        }
    }
}

#[pymethods]
impl Contract {
    /// Monetary value of one minimum price movement for one lot.
    pub const fn tick_value(&self) -> f64 {
        self.lot_size * self.tick_size
    }

    /// Convert to a Python dictionary.
    pub fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("id", self.id)?;
        dict.set_item("symbol", &self.symbol)?;
        dict.set_item("listing_date", &self.listing_date)?;
        dict.set_item("expiry_date", &self.expiry_date)?;
        dict.set_item("lot_size", self.lot_size)?;
        dict.set_item("tick_size", self.tick_size)?;
        dict.set_item("tick_value", self.tick_value())?;
        dict.set_item("active", self.active)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "Contract(id={}, symbol={:?}, lot_size={}, tick_size={}, active={})",
            self.id, self.symbol, self.lot_size, self.tick_size, self.active
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

/// Lot and tick specification for one contract over a validity interval.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, PartialEq)]
pub struct ContractSpecVersion {
    /// Contract id from the contract table.
    #[pyo3(get)]
    pub contract_id: u32,
    /// Number of units per lot.
    #[pyo3(get)]
    pub lot_size: f64,
    /// Minimum price tick.
    #[pyo3(get)]
    pub tick_size: f64,
    /// Inclusive first timestamp this specification is valid.
    #[pyo3(get)]
    pub valid_from: String,
    /// Exclusive end timestamp; `None` means open-ended/current.
    #[pyo3(get)]
    pub valid_to: Option<String>,
}

impl From<&RustContractSpecVersion> for ContractSpecVersion {
    fn from(value: &RustContractSpecVersion) -> Self {
        Self {
            contract_id: value.contract_id,
            lot_size: value.lot_size,
            tick_size: value.tick_size,
            valid_from: value.valid_from.clone(),
            valid_to: value.valid_to.clone(),
        }
    }
}

#[pymethods]
impl ContractSpecVersion {
    /// Monetary value of one minimum price movement for one lot.
    pub const fn tick_value(&self) -> f64 {
        self.lot_size * self.tick_size
    }

    /// Convert to a Python dictionary.
    pub fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("contract_id", self.contract_id)?;
        dict.set_item("lot_size", self.lot_size)?;
        dict.set_item("tick_size", self.tick_size)?;
        dict.set_item("tick_value", self.tick_value())?;
        dict.set_item("valid_from", &self.valid_from)?;
        dict.set_item("valid_to", &self.valid_to)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "ContractSpecVersion(contract_id={}, lot_size={}, tick_size={}, valid_from={:?}, valid_to={:?})",
            self.contract_id, self.lot_size, self.tick_size, self.valid_from, self.valid_to
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

/// Parsed fee specification with original text.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, PartialEq)]
pub struct FeeSpec {
    /// Parsed fee unit (`"CnyPerLot"`, `"TurnoverRatePerTenThousand"`, `"Zero"`, `"Unknown"`).
    #[pyo3(get)]
    pub kind: String,
    /// Parsed numeric value when available.
    #[pyo3(get)]
    pub value: Option<f64>,
    /// Trimmed source text when meaningful.
    #[pyo3(get)]
    pub raw_text: Option<String>,
}

impl From<&RustFeeSpec> for FeeSpec {
    fn from(value: &RustFeeSpec) -> Self {
        Self {
            kind: fee_kind_str(&value.kind).to_owned(),
            value: value.value,
            raw_text: value.raw_text.clone(),
        }
    }
}

#[pymethods]
impl FeeSpec {
    /// Convert to a Python dictionary.
    pub fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("kind", &self.kind)?;
        dict.set_item("value", self.value)?;
        dict.set_item("raw_text", &self.raw_text)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "FeeSpec(kind={:?}, value={:?}, raw_text={:?})",
            self.kind, self.value, self.raw_text
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

/// Fee rule for a contract over a validity interval.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, PartialEq)]
pub struct ContractFee {
    /// Contract id from the contract table.
    #[pyo3(get)]
    pub contract_id: u32,
    /// Hash identifying the normalized fee rule.
    #[pyo3(get)]
    pub rule_hash: String,
    /// Long-side margin rate when known.
    #[pyo3(get)]
    pub buy_margin_rate: Option<f64>,
    /// Short-side margin rate when known.
    #[pyo3(get)]
    pub sell_margin_rate: Option<f64>,
    /// Open position fee.
    #[pyo3(get)]
    pub open_fee: FeeSpec,
    /// Close yesterday position fee.
    #[pyo3(get)]
    pub close_yesterday_fee: FeeSpec,
    /// Close today position fee.
    #[pyo3(get)]
    pub close_today_fee: FeeSpec,
    /// Intraday opening fee; `None` means the source did not publish it.
    #[pyo3(get)]
    pub intraday_open_fee: Option<FeeSpec>,
    /// Trading status reported by the source (`"Trading"`, `"NotTrading"`, `"Unknown"`).
    #[pyo3(get)]
    pub trading_status: String,
    /// Whether this record refers to the main contract.
    #[pyo3(get)]
    pub is_main_contract: bool,
    /// Source update timestamp when provided.
    #[pyo3(get)]
    pub source_updated_at: Option<String>,
    /// Inclusive first date this rule is valid.
    #[pyo3(get)]
    pub valid_from: String,
    /// Exclusive end date this rule is valid; `None` means open-ended/current.
    #[pyo3(get)]
    pub valid_to: Option<String>,
}

impl From<&RustContractFee> for ContractFee {
    fn from(value: &RustContractFee) -> Self {
        Self {
            contract_id: value.contract_id,
            rule_hash: value.rule_hash.clone(),
            buy_margin_rate: value.buy_margin_rate,
            sell_margin_rate: value.sell_margin_rate,
            open_fee: FeeSpec::from(&value.open_fee),
            close_yesterday_fee: FeeSpec::from(&value.close_yesterday_fee),
            close_today_fee: FeeSpec::from(&value.close_today_fee),
            intraday_open_fee: value.intraday_open_fee.as_ref().map(FeeSpec::from),
            trading_status: trading_status_str(&value.trading_status).to_owned(),
            is_main_contract: value.is_main_contract,
            source_updated_at: value.source_updated_at.clone(),
            valid_from: value.valid_from.clone(),
            valid_to: value.valid_to.clone(),
        }
    }
}

#[pymethods]
impl ContractFee {
    /// Convert to a Python dictionary.
    pub fn to_dict<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyDict>> {
        let dict = PyDict::new(py);
        dict.set_item("contract_id", self.contract_id)?;
        dict.set_item("rule_hash", &self.rule_hash)?;
        dict.set_item("buy_margin_rate", self.buy_margin_rate)?;
        dict.set_item("sell_margin_rate", self.sell_margin_rate)?;
        dict.set_item("open_fee", self.open_fee.to_dict(py)?)?;
        dict.set_item("close_yesterday_fee", self.close_yesterday_fee.to_dict(py)?)?;
        dict.set_item("close_today_fee", self.close_today_fee.to_dict(py)?)?;
        match &self.intraday_open_fee {
            Some(fee) => dict.set_item("intraday_open_fee", fee.to_dict(py)?)?,
            None => dict.set_item("intraday_open_fee", py.None())?,
        }
        dict.set_item("trading_status", &self.trading_status)?;
        dict.set_item("is_main_contract", self.is_main_contract)?;
        dict.set_item("source_updated_at", &self.source_updated_at)?;
        dict.set_item("valid_from", &self.valid_from)?;
        dict.set_item("valid_to", &self.valid_to)?;
        Ok(dict)
    }

    fn __repr__(&self) -> String {
        format!(
            "ContractFee(contract_id={}, valid_from={:?}, valid_to={:?}, trading_status={:?})",
            self.contract_id, self.valid_from, self.valid_to, self.trading_status
        )
    }

    fn __eq__(&self, other: &Self) -> bool {
        self == other
    }
}

/// Fee rule compiled for tight backtest loops.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PreparedFee {
    inner: RustPreparedFee,
}

#[pymethods]
impl PreparedFee {
    /// Contract lot size used when compiling turnover-rate fees.
    #[getter]
    pub const fn lot_size(&self) -> f64 {
        self.inner.lot_size()
    }

    /// Fee for opening `lots` lots at `price`.
    pub fn open_amount(&self, price: f64, lots: f64) -> f64 {
        self.inner.open_amount(price, lots)
    }

    /// Published intraday opening fee; `None` means unknown, not zero.
    pub fn intraday_open_amount(&self, price: f64, lots: f64) -> Option<f64> {
        self.inner.intraday_open_amount(price, lots)
    }

    /// Fee for closing yesterday positions.
    pub fn close_yesterday_amount(&self, price: f64, lots: f64) -> f64 {
        self.inner.close_yesterday_amount(price, lots)
    }

    /// Fee for closing today positions.
    pub fn close_today_amount(&self, price: f64, lots: f64) -> f64 {
        self.inner.close_today_amount(price, lots)
    }

    fn __repr__(&self) -> String {
        format!("PreparedFee(lot_size={})", self.inner.lot_size())
    }

    fn __eq__(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

/// Dense prepared fee table in caller-supplied handle slot order.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone)]
pub struct PreparedFeeBook {
    inner: RustPreparedFeeBook,
}

#[pymethods]
impl PreparedFeeBook {
    /// Prepared fees in caller-supplied slot order.
    pub fn fees(&self) -> Vec<PreparedFee> {
        self.inner
            .fees()
            .iter()
            .copied()
            .map(|inner| PreparedFee { inner })
            .collect()
    }

    /// Return a prepared fee by slot.
    pub fn get(&self, slot: usize) -> Option<PreparedFee> {
        self.inner
            .get(slot)
            .copied()
            .map(|inner| PreparedFee { inner })
    }

    /// Number of prepared fee slots.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the book contains no slots.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    fn __len__(&self) -> usize {
        self.inner.len()
    }

    fn __getitem__(&self, slot: usize) -> PyResult<PreparedFee> {
        self.get(slot)
            .ok_or_else(|| to_py_err(RustError::InvalidFeeSlot(slot)))
    }
}

/// Dense prepared fee table that can advance across trading days.
#[pyclass(module = "future_meta", from_py_object)]
#[derive(Debug, Clone)]
pub struct PreparedFeeCursors {
    meta: RustFutureMeta,
    handles: Vec<RustContractHandle>,
    trading_date: Date,
    next_change_unix_nanos: i64,
    last_unix_nanos: i64,
    fees: Vec<RustPreparedFee>,
}

impl PreparedFeeCursors {
    fn build(
        meta: RustFutureMeta,
        handles: Vec<RustContractHandle>,
        trading_date: Date,
        start_unix_nanos: i64,
    ) -> Result<Self, RustError> {
        let cursors =
            meta.prepare_fee_cursors(handles.iter().copied(), trading_date, start_unix_nanos)?;
        let next_change_unix_nanos = cursors.next_change_unix_nanos();
        let fees = (0..cursors.len())
            .filter_map(|slot| cursors.get(slot).copied())
            .collect();
        Ok(Self {
            meta,
            handles,
            trading_date,
            next_change_unix_nanos,
            last_unix_nanos: start_unix_nanos,
            fees,
        })
    }

    fn advance_to_date(&mut self, trading_date: Date, unix_nanos: i64) -> Result<(), RustError> {
        if trading_date != self.trading_date {
            self.meta.for_trading_day(trading_date)?;
        }
        if unix_nanos < self.last_unix_nanos {
            return Err(RustError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor table position"
            )));
        }
        if trading_date != self.trading_date {
            let cursors = self.meta.prepare_fee_cursors(
                self.handles.iter().copied(),
                trading_date,
                unix_nanos,
            )?;
            self.trading_date = trading_date;
            self.next_change_unix_nanos = cursors.next_change_unix_nanos();
            self.last_unix_nanos = unix_nanos;
            self.fees.clear();
            self.fees
                .extend((0..cursors.len()).filter_map(|slot| cursors.get(slot).copied()));
            return Ok(());
        }
        if unix_nanos >= self.next_change_unix_nanos {
            return Err(RustError::InvalidTimestamp(format!(
                "{unix_nanos} is outside trading day {}",
                self.trading_date
            )));
        }
        self.last_unix_nanos = unix_nanos;
        Ok(())
    }
}

#[pymethods]
impl PreparedFeeCursors {
    /// Current exchange-local trading date (`YYYY-MM-DD`) for this cursor table.
    pub fn current_trading_date(&self) -> String {
        self.trading_date.to_string()
    }

    /// Unix timestamp in nanoseconds where any slot may need to change.
    pub const fn next_change_unix_nanos(&self) -> i64 {
        self.next_change_unix_nanos
    }

    /// Number of cursor slots.
    pub fn len(&self) -> usize {
        self.fees.len()
    }

    /// Whether the cursor table contains no slots.
    pub fn is_empty(&self) -> bool {
        self.fees.is_empty()
    }

    /// Return the current prepared fee by slot, or `None` if out of bounds.
    pub fn get(&self, slot: usize) -> Option<PreparedFee> {
        self.fees
            .get(slot)
            .copied()
            .map(|inner| PreparedFee { inner })
    }

    /// Return the current prepared fee by slot.
    pub fn current(&self, slot: usize) -> PyResult<PreparedFee> {
        self.get(slot)
            .ok_or_else(|| to_py_err(RustError::InvalidFeeSlot(slot)))
    }

    /// Advance the cursor table to a monotonic timestamp and trading date (`YYYY-MM-DD`).
    pub fn advance_to(&mut self, trading_date: &str, unix_nanos: i64) -> PyResult<()> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.advance_to_date(date, unix_nanos).map_err(to_py_err)
    }

    /// Advance if needed and return the current prepared fee by slot.
    pub fn advance_and_get(
        &mut self,
        trading_date: &str,
        slot: usize,
        unix_nanos: i64,
    ) -> PyResult<PreparedFee> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        if date != self.trading_date || unix_nanos >= self.next_change_unix_nanos {
            self.advance_to_date(date, unix_nanos).map_err(to_py_err)?;
        } else if unix_nanos < self.last_unix_nanos {
            return Err(to_py_err(RustError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor table position"
            ))));
        } else {
            self.last_unix_nanos = unix_nanos;
        }
        self.current(slot)
    }

    /// Return a slot fee for a monotonic timestamp within the current trading day.
    pub fn advance_and_get_unix_nanos(
        &mut self,
        slot: usize,
        unix_nanos: i64,
    ) -> PyResult<PreparedFee> {
        if unix_nanos < self.last_unix_nanos {
            return Err(to_py_err(RustError::InvalidTimestamp(format!(
                "{unix_nanos} is before current fee cursor table position"
            ))));
        }
        if unix_nanos >= self.next_change_unix_nanos {
            return Err(to_py_err(RustError::InvalidTimestamp(format!(
                "{unix_nanos} is outside trading day {}",
                self.trading_date
            ))));
        }
        self.last_unix_nanos = unix_nanos;
        self.current(slot)
    }

    fn __len__(&self) -> usize {
        self.fees.len()
    }
}

/// Precomputed fee snapshot for one exchange-local trading date.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone)]
pub struct TradingDayMeta {
    meta: RustFutureMeta,
    trading_date: Date,
}

#[pymethods]
impl TradingDayMeta {
    /// Return the exchange-local trading date (`YYYY-MM-DD`) for this snapshot.
    pub fn trading_date(&self) -> String {
        self.trading_date.to_string()
    }

    /// Resolve a concrete contract symbol for repeated hot-path queries.
    pub fn resolve_contract(&self, symbol: &str) -> PyResult<ContractHandle> {
        self.meta
            .resolve_contract(symbol)
            .map(|inner| ContractHandle { inner })
            .map_err(to_py_err)
    }

    /// Return metadata for a concrete contract symbol.
    pub fn contract(&self, symbol: &str) -> PyResult<Contract> {
        self.meta
            .contract(symbol)
            .map(Contract::from)
            .map_err(to_py_err)
    }

    /// Return metadata for a pre-resolved contract.
    pub fn contract_for_handle(&self, handle: &ContractHandle) -> PyResult<Contract> {
        self.meta
            .contract_for_handle(handle.inner)
            .map(Contract::from)
            .map_err(to_py_err)
    }

    /// Return the contract specification for this trading day.
    pub fn contract_spec(&self, symbol: &str) -> PyResult<ContractSpecVersion> {
        self.meta
            .contract_spec_on(symbol, self.trading_date)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return a pre-resolved contract's specification for this trading day.
    pub fn contract_spec_for_handle(
        &self,
        handle: &ContractHandle,
    ) -> PyResult<ContractSpecVersion> {
        self.meta
            .contract_spec_for_handle_on(handle.inner, self.trading_date)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a pre-resolved contract in this trading day.
    pub fn fee_rule(&self, handle: &ContractHandle) -> PyResult<ContractFee> {
        self.meta
            .contract_fee_for_handle_on(handle.inner, self.trading_date)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a concrete contract symbol in this trading day.
    pub fn fee_rule_by_symbol(&self, symbol: &str) -> PyResult<ContractFee> {
        let day = self
            .meta
            .for_trading_day(self.trading_date)
            .map_err(to_py_err)?;
        day.fee_rule_by_symbol(symbol)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Prepare the current daily fee rule for a hot loop.
    pub fn prepare_fee(&self, handle: &ContractHandle) -> PyResult<PreparedFee> {
        let day = self
            .meta
            .for_trading_day(self.trading_date)
            .map_err(to_py_err)?;
        day.prepare_fee(handle.inner)
            .map(|inner| PreparedFee { inner })
            .map_err(to_py_err)
    }

    /// Prepare the current daily fee rule for a concrete contract symbol.
    pub fn prepare_fee_by_symbol(&self, symbol: &str) -> PyResult<PreparedFee> {
        let day = self
            .meta
            .for_trading_day(self.trading_date)
            .map_err(to_py_err)?;
        day.prepare_fee_by_symbol(symbol)
            .map(|inner| PreparedFee { inner })
            .map_err(to_py_err)
    }

    /// Prepare a dense fee book in caller-supplied handle order.
    pub fn prepare_fee_book(&self, handles: Vec<ContractHandle>) -> PyResult<PreparedFeeBook> {
        let day = self
            .meta
            .for_trading_day(self.trading_date)
            .map_err(to_py_err)?;
        day.prepare_fee_book(handles.into_iter().map(|h| h.inner))
            .map(|inner| PreparedFeeBook { inner })
            .map_err(to_py_err)
    }

    /// Prepare dense day-fixed fee cursors in caller-supplied handle order.
    pub fn prepare_fee_cursors(
        &self,
        handles: Vec<ContractHandle>,
        start_unix_nanos: i64,
    ) -> PyResult<PreparedFeeCursors> {
        PreparedFeeCursors::build(
            self.meta.clone(),
            handles.into_iter().map(|h| h.inner).collect(),
            self.trading_date,
            start_unix_nanos,
        )
        .map_err(to_py_err)
    }
}

/// High-performance local future-meta query client.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone)]
pub struct FutureMeta {
    inner: RustFutureMeta,
}

#[pymethods]
impl FutureMeta {
    /// Decode compressed archive bytes and build an indexed query client.
    #[staticmethod]
    pub fn from_compressed_bytes(py: Python<'_>, data: &[u8]) -> PyResult<Self> {
        let bytes = data.to_vec();
        let inner = py
            .detach(move || {
                let archive = future_meta::archive::decode_archive_bytes(&bytes)?;
                RustFutureMeta::from_archive(archive)
            })
            .map_err(to_py_err)?;
        Ok(Self { inner })
    }

    /// Load an encoded archive file from disk and build an indexed query client.
    #[staticmethod]
    pub fn from_file(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        let inner = py
            .detach(move || {
                let bytes = std::fs::read(path).map_err(RustError::from)?;
                let archive = future_meta::archive::decode_archive_bytes(&bytes)?;
                RustFutureMeta::from_archive(archive)
            })
            .map_err(to_py_err)?;
        Ok(Self { inner })
    }

    /// Alias for `from_file` matching the Rust `FutureMeta::load_file` method.
    #[staticmethod]
    pub fn load_file(py: Python<'_>, path: PathBuf) -> PyResult<Self> {
        Self::from_file(py, path)
    }

    /// Resolve a concrete contract symbol once for repeated hot-path queries.
    pub fn resolve_contract(&self, symbol: &str) -> PyResult<ContractHandle> {
        self.inner
            .resolve_contract(symbol)
            .map(|inner| ContractHandle { inner })
            .map_err(to_py_err)
    }

    /// Return metadata for a concrete contract symbol.
    pub fn contract(&self, symbol: &str) -> PyResult<Contract> {
        self.inner
            .contract(symbol)
            .map(Contract::from)
            .map_err(to_py_err)
    }

    /// Return metadata for a pre-resolved contract.
    pub fn contract_for_handle(&self, handle: &ContractHandle) -> PyResult<Contract> {
        self.inner
            .contract_for_handle(handle.inner)
            .map(Contract::from)
            .map_err(to_py_err)
    }

    /// Return all contract metadata rows in archive order.
    pub fn contracts(&self) -> Vec<Contract> {
        self.inner.contracts().iter().map(Contract::from).collect()
    }

    /// Return the contract lot/tick specification at an RFC 3339 timestamp.
    pub fn contract_spec_asof(&self, symbol: &str, at: &str) -> PyResult<ContractSpecVersion> {
        self.inner
            .contract_spec_asof(symbol, at)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return the contract lot/tick specification on an exchange-local date (`YYYY-MM-DD`).
    pub fn contract_spec_on(
        &self,
        symbol: &str,
        trading_date: &str,
    ) -> PyResult<ContractSpecVersion> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner
            .contract_spec_on(symbol, date)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return a pre-resolved contract's lot/tick specification at an RFC 3339 timestamp.
    pub fn contract_spec_for_handle_asof(
        &self,
        handle: &ContractHandle,
        at: &str,
    ) -> PyResult<ContractSpecVersion> {
        let ts = parse_rfc3339(at).map_err(to_py_err)?;
        self.inner
            .contract_spec_for_handle_at(handle.inner, ts)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return a pre-resolved contract's lot/tick specification on a date (`YYYY-MM-DD`).
    pub fn contract_spec_for_handle_on(
        &self,
        handle: &ContractHandle,
        trading_date: &str,
    ) -> PyResult<ContractSpecVersion> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner
            .contract_spec_for_handle_on(handle.inner, date)
            .map(ContractSpecVersion::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a concrete contract at an RFC 3339 timestamp.
    pub fn contract_fee_asof(&self, symbol: &str, at: &str) -> PyResult<ContractFee> {
        self.inner
            .contract_fee_asof(symbol, at)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a concrete contract on an exchange-local date (`YYYY-MM-DD`).
    pub fn contract_fee_on(&self, symbol: &str, trading_date: &str) -> PyResult<ContractFee> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner
            .contract_fee_on(symbol, date)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a pre-resolved contract at an RFC 3339 timestamp.
    pub fn contract_fee_for_handle_asof(
        &self,
        handle: &ContractHandle,
        at: &str,
    ) -> PyResult<ContractFee> {
        let ts = parse_rfc3339(at).map_err(to_py_err)?;
        self.inner
            .contract_fee_for_handle_at(handle.inner, ts)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Return the fee rule for a pre-resolved contract on an exchange-local date (`YYYY-MM-DD`).
    pub fn contract_fee_for_handle_on(
        &self,
        handle: &ContractHandle,
        trading_date: &str,
    ) -> PyResult<ContractFee> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner
            .contract_fee_for_handle_on(handle.inner, date)
            .map(ContractFee::from)
            .map_err(to_py_err)
    }

    /// Return all underlying contract fee rules available at an RFC 3339 timestamp.
    pub fn underlying_fees_asof(
        &self,
        underlying_symbol: &str,
        at: &str,
    ) -> PyResult<Vec<ContractFee>> {
        self.inner
            .underlying_fees_asof(underlying_symbol, at)
            .map(|fees| fees.into_iter().map(ContractFee::from).collect())
            .map_err(to_py_err)
    }

    /// Return all underlying contract fee rules available on a trading date (`YYYY-MM-DD`).
    pub fn underlying_fees_on(
        &self,
        underlying_symbol: &str,
        trading_date: &str,
    ) -> PyResult<Vec<ContractFee>> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner
            .underlying_fees_on(underlying_symbol, date)
            .map(|iter| iter.map(ContractFee::from).collect())
            .map_err(to_py_err)
    }

    /// Build a precomputed fee snapshot for one exchange-local trading date (`YYYY-MM-DD`).
    pub fn for_trading_day(&self, trading_date: &str) -> PyResult<TradingDayMeta> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        self.inner.for_trading_day(date).map_err(to_py_err)?;
        Ok(TradingDayMeta {
            meta: self.inner.clone(),
            trading_date: date,
        })
    }

    /// Prepare dense day-fixed fee cursors for a multi-day tick stream.
    pub fn prepare_fee_cursors(
        &self,
        handles: Vec<ContractHandle>,
        trading_date: &str,
        start_unix_nanos: i64,
    ) -> PyResult<PreparedFeeCursors> {
        let date = parse_trading_date(trading_date).map_err(to_py_err)?;
        PreparedFeeCursors::build(
            self.inner.clone(),
            handles.into_iter().map(|h| h.inner).collect(),
            date,
            start_unix_nanos,
        )
        .map_err(to_py_err)
    }
}

/// Download and cache configuration for loading published future-meta archives.
#[pyclass(module = "future_meta", frozen, from_py_object)]
#[derive(Debug, Clone)]
pub struct DownloadConfig {
    inner: future_meta::DownloadConfig,
}

#[pymethods]
impl DownloadConfig {
    #[new]
    #[pyo3(signature = (manifest_url=None, cache_dir=None))]
    pub fn new(manifest_url: Option<String>, cache_dir: Option<PathBuf>) -> Self {
        let mut inner = future_meta::DownloadConfig::default();
        if let Some(url) = manifest_url {
            inner.manifest_url = url;
        }
        if let Some(dir) = cache_dir {
            inner.cache_dir = dir;
        }
        Self { inner }
    }

    /// URL of the manifest JSON file.
    #[getter]
    pub fn manifest_url(&self) -> &str {
        &self.inner.manifest_url
    }

    /// Directory used to cache downloaded artifacts.
    #[getter]
    pub fn cache_dir(&self) -> String {
        self.inner.cache_dir.to_string_lossy().into_owned()
    }

    fn __repr__(&self) -> String {
        format!(
            "DownloadConfig(manifest_url={:?}, cache_dir={:?})",
            self.inner.manifest_url,
            self.inner.cache_dir.display()
        )
    }
}

/// A loaded archive with a reusable HTTP client and conditional refresh support.
#[pyclass(module = "future_meta")]
pub struct DownloadClient {
    inner: Mutex<future_meta::DownloadClient>,
}

#[pymethods]
impl DownloadClient {
    /// Fetch the manifest and load its verified archive, using the disk cache.
    #[staticmethod]
    #[pyo3(signature = (config=None))]
    pub fn load(py: Python<'_>, config: Option<DownloadConfig>) -> PyResult<Self> {
        let cfg = config.map_or_else(future_meta::DownloadConfig::default, |c| c.inner);
        let rt = tokio_runtime()?;
        let client = py
            .detach(move || rt.block_on(future_meta::DownloadClient::load(cfg)))
            .map_err(to_py_err)?;
        Ok(Self {
            inner: Mutex::new(client),
        })
    }

    /// Access the current immutable query snapshot without network or disk I/O.
    pub fn meta(&self) -> PyResult<FutureMeta> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| FutureMetaError::new_err("download client lock poisoned"))?;
        Ok(FutureMeta {
            inner: guard.meta().clone(),
        })
    }

    /// Check for changes, returning whether the query snapshot was replaced.
    pub fn refresh(&self, py: Python<'_>) -> PyResult<bool> {
        let rt = tokio_runtime()?;
        py.detach(|| {
            let mut guard = self
                .inner
                .lock()
                .map_err(|_| RustError::DownloadFailed("download client lock poisoned".into()))?;
            rt.block_on(guard.refresh())
        })
        .map_err(to_py_err)
    }
}

/// Encode a JSON-serialized `FeeArchiveV3` payload into compressed schema 4 archive bytes.
#[pyfunction]
pub fn encode_archive_json(json_str: &str) -> PyResult<Vec<u8>> {
    let payload: future_meta::FeeArchiveV3 =
        serde_json::from_str(json_str).map_err(|err| to_py_err(RustError::from(err)))?;
    future_meta::archive::encode_archive_bytes(&payload).map_err(to_py_err)
}

/// Load a cached archive or fetch the current manifest and artifact.
#[pyfunction]
#[pyo3(signature = (config=None))]
pub fn load_or_fetch(py: Python<'_>, config: Option<DownloadConfig>) -> PyResult<FutureMeta> {
    let cfg = config.map_or_else(future_meta::DownloadConfig::default, |c| c.inner);
    let rt = tokio_runtime()?;
    let inner = py
        .detach(move || rt.block_on(future_meta::load_or_fetch(cfg)))
        .map_err(to_py_err)?;
    Ok(FutureMeta { inner })
}

/// Parse a `TqSdk` symbol into its structured representation.
#[pyfunction]
pub fn parse_symbol(raw: &str) -> PyResult<ParsedSymbol> {
    future_meta::symbol::parse_symbol(raw)
        .map(ParsedSymbol::from)
        .map_err(to_py_err)
}

/// Normalize an exchange-local futures symbol into canonical `EXCHANGE.local` form.
#[pyfunction]
pub fn normalize_futures_symbol(exchange: &str, local: &str) -> PyResult<String> {
    future_meta::symbol::normalize_futures_symbol(exchange, local).map_err(to_py_err)
}

/// Return the parsed symbol's underlying product symbol.
#[pyfunction]
pub fn derive_underlying_symbol(symbol: &str) -> PyResult<String> {
    future_meta::symbol::derive_underlying_symbol(symbol).map_err(to_py_err)
}

/// Compute the SHA-256 checksum as lowercase hexadecimal text.
#[pyfunction]
pub fn sha256_hex(data: &[u8]) -> String {
    future_meta::archive::sha256_hex(data)
}

#[pymodule(name = "_native")]
fn native(py: Python<'_>, m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("SCHEMA_VERSION", SCHEMA_VERSION)?;
    m.add("FutureMetaError", py.get_type::<FutureMetaError>())?;
    m.add_class::<ParsedSymbol>()?;
    m.add_class::<ContractHandle>()?;
    m.add_class::<Contract>()?;
    m.add_class::<ContractSpecVersion>()?;
    m.add_class::<FeeSpec>()?;
    m.add_class::<ContractFee>()?;
    m.add_class::<PreparedFee>()?;
    m.add_class::<PreparedFeeBook>()?;
    m.add_class::<PreparedFeeCursors>()?;
    m.add_class::<TradingDayMeta>()?;
    m.add_class::<FutureMeta>()?;
    m.add_class::<DownloadConfig>()?;
    m.add_class::<DownloadClient>()?;
    m.add_function(wrap_pyfunction!(load_or_fetch, m)?)?;
    m.add_function(wrap_pyfunction!(parse_symbol, m)?)?;
    m.add_function(wrap_pyfunction!(normalize_futures_symbol, m)?)?;
    m.add_function(wrap_pyfunction!(derive_underlying_symbol, m)?)?;
    m.add_function(wrap_pyfunction!(sha256_hex, m)?)?;
    m.add_function(wrap_pyfunction!(encode_archive_json, m)?)?;
    Ok(())
}
