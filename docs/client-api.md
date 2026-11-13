# future-meta Client API

`future-meta` loads a published fee archive into a local `FutureMeta` query
client. Queries do not call the network after the archive is loaded.

## Periodic download refresh

Since SDK v0.8.0, the `download` feature provides `DownloadClient` for periodic update checks. Keep one instance across checks.
Inside an async function:

```rust
use future_meta::{DownloadClient, DownloadConfig};

let mut client = DownloadClient::load(DownloadConfig::default()).await?;
let mut handle = client.meta().resolve_contract("SHFE.cu2607")?;

// Run at the application's refresh boundary.
if client.refresh().await? {
    handle = client.meta().resolve_contract("SHFE.cu2607")?;
}
let contract = client.meta().contract_for_handle(handle)?;
```

`refresh()` returns `false` for HTTP 304 or an unchanged archive SHA. It preserves
the current query instance and handles without reading or decoding the disk cache.
When it returns `true`, resolve handles and rebuild prepared cursors/books for the
new snapshot. Cloned older snapshots and their handles remain usable together.
Download, checksum, decode and cache-write failures leave the current in-memory
snapshot and its manifest validator unchanged. Downloaded bytes are decoded before
replacing the disk cache. Each refresh still checks the manifest over the network;
the client reuses its HTTP connection pool and stores the ETag only in memory.
The existing `load_or_fetch` API remains suitable for one-time loading.

## Python Bindings (`future_meta`)

`crates/future-meta-py` wraps `future-meta` as a native Python extension module (`future_meta`) built with `PyO3 + maturin` (`abi3-py310`), without adding Python dependencies to the pure-Rust `future-meta` crate.

### Installation

Install directly from PyPI, from prebuilt wheels attached to SDK GitHub Releases, or build locally from a repository checkout:

```bash
pip install future-meta
# Or from a local checkout:
pip install maturin
maturin develop --release
```

### Online & Offline Loading

`load_or_fetch`, `DownloadClient.load`, `DownloadClient.refresh`, and `FutureMeta.from_file` run synchronously on an internal Tokio runtime while releasing the Python GIL (`py.detach`):

```python
import future_meta

# One-shot download/cache load
meta = future_meta.load_or_fetch()

# Long-running client with ETag / SHA conditional refresh
client = future_meta.DownloadClient.load()
if client.refresh():
    meta = client.meta()

# Offline archive loading from disk or bytes
offline_meta = future_meta.FutureMeta.from_file("latest.fmeta.zst")
```

### Queries & Cross-Day Tick Cursors

Trading-day APIs (`*_on`, `for_trading_day`, `prepare_fee_cursors`, `advance_to`, `advance_and_get`) accept `"YYYY-MM-DD"` strings:

```python
cu = meta.resolve_contract("SHFE.cu2607")

spec = meta.contract_spec_for_handle_on(cu, "2026-06-04")
fee = meta.contract_fee_for_handle_on(cu, "2026-06-04")

cursors = meta.prepare_fee_cursors([cu], "2026-06-04", 1780534800_000_000_000)
prepared = cursors.current(0)
cost = prepared.open_amount(80000.0, 2.0)
```

All model objects (`Contract`, `ContractSpecVersion`, `ContractFee`, `FeeSpec`, `ParsedSymbol`) expose read-only attributes and `.to_dict()`. Client errors raise `future_meta.FutureMetaError`. Full PEP 561 type stubs (`__init__.pyi` and `py.typed`) are bundled in the package.

## Concrete contracts only

Fee and specification records belong only to concrete futures contracts. Pass symbols such as
`SHFE.cu2607` to `contract`, `resolve_contract`, `contract_spec_*`, and `contract_fee_*`.

SDK 0.7.0 also accepts DCE monthly-average futures such as `DCE.l2611F`,
`DCE.pp2611F`, and `DCE.v2611F`. Keep the uppercase `F`: their fees, specifications,
and lifecycles are independent of the ordinary contract with the same month.
TqSdk underlying symbols remain `DCE.l`, `DCE.pp`, and `DCE.v`; underlying queries
include both kinds of concrete contract. Schema 4 archives containing these symbols
require SDK 0.7.0 or later; older SDKs reject the symbols when indexing the archive.
`KQ.m@...` and `KQ.i@...` are aliases, not contracts; these APIs reject them with
`UnsupportedSymbolKind` and archives containing them are corrupt. For a product-family view, call
`underlying_fees_*` with an underlying such as `SHFE.cu`; it returns concrete contracts and excludes
only `NotTrading` records, retaining `Unknown` when status was not independently refreshed.

## Tick-Level Backtests

Use `FutureMeta` for setup and validation. Do not query raw fee rules inside a
tick loop. Resolve handles once, then use the cross-day prepared cursor table as
the default hot-path API.

```rust
use future_meta::FutureMeta;

let meta = FutureMeta::from_archive(archive)?;
let cu = meta.resolve_contract("SHFE.cu2607")?;
```

### Default Path: Cross-Day Tick Streams

Most tick backtests cross trading days. Build one `PreparedFeeCursors` table at
the first tick and let it rebuild its internal day cache when the trading date
changes:

```rust
let mut fees = meta.prepare_fee_cursors(
    [cu],
    first_tick.trading_date,
    first_tick.unix_nanos,
)?;

for tick in ticks {
    if tick.unix_nanos >= fees.next_change_unix_nanos() {
        fees.advance_to(tick.trading_date, tick.unix_nanos)?;
    }

    let fee = fees.current(tick.fee_slot)?;
    cost += fee.open_amount(tick.price, tick.lots);
}
```

The normal tick path is one `i64` comparison plus a slot lookup. The slow path
runs only at a trading-day boundary. Futures fees are day-fixed; timestamp
queries are normalized to the exchange-local date before selecting the fee
version. `PreparedFee` stores only numeric coefficients, so the loop does not
branch on `FeeKind`.

### Single-Day Advanced APIs

If the caller has already partitioned ticks by trading day, use
`TradingDayMeta` directly:

```rust
let day = meta.for_trading_day(trading_date)?;
let cu = day.resolve_contract("SHFE.cu2607")?;

let fee = day.prepare_fee(cu)?;
```

Fees do not change within the trading day, so keep the `PreparedFee` or
`PreparedFeeBook` outside the tick loop and reuse it for every tick in that
day.

For lower-volume cross-day loops, use the convenience method:

```rust
let fee = fees.advance_and_get(tick.trading_date, tick.fee_slot, tick.unix_nanos)?;
```

### Mixed Contracts

For mixed-contract tick streams, map each symbol to a compact slot before the
loop:

```rust
let handles = [meta.resolve_contract("SHFE.cu2607")?, meta.resolve_contract("DCE.m2609")?];
let mut fees = meta.prepare_fee_cursors(handles, first_tick.trading_date, first_tick.unix_nanos)?;

for tick in ticks {
    let fee = fees.advance_and_get(tick.trading_date, tick.fee_slot, tick.unix_nanos)?;
    cost += fee.open_amount(tick.price, tick.lots);
}
```

Slot order matches the handle order passed to `prepare_fee_cursors`.

### Raw Rule Access

Use raw rule APIs outside hot loops when you need source-level fields:

```rust
let day = meta.for_trading_day(trading_date)?;
let rule = day.fee_rule(cu)?;
let by_symbol = day.fee_rule_by_symbol("SHFE.cu2607")?;
let exact = meta.contract_fee_asof("SHFE.cu2607", "2026-06-04T12:00:00+08:00")?;
```

## Breaking API Notes

Existing raw `FutureMeta::contract_fee_*` and
`FutureMeta::contract_fee_for_handle_*` APIs remain available for source-level
rule access. Hot tick loops should move to `FutureMeta::prepare_fee_cursors`,
which handles day-fixed fees, mixed contracts, and cross-day rebuilds behind
one API. `PreparedFeeCursor` and `TradingDayMeta::prepare_fee_cursor` were
removed because they encoded an invalid same-day-change assumption.

`TradingDayMeta` raw accessors are named `fee_rule` and `fee_rule_by_symbol` to
separate raw source rules from compiled hot-path fees.

`FutureMetaError` is now `#[non_exhaustive]`. It also includes
`UnsupportedFeeRule` for fee specs that cannot be compiled into numeric
coefficients, and `InvalidFeeSlot` for missing slots in prepared cursor tables.
Matches on `FutureMetaError` need a wildcard arm.
