# future-meta Client API

`future-meta` loads a published fee archive into a local `FutureMeta` query
client. Queries do not call the network after the archive is loaded.

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
