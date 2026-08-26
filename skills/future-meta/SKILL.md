---
name: future-meta
description: 面向 future-meta 客户端用户的接入与正确性指南。每当用户想在 Rust 量化研究、交易系统或回测框架中查询中国期货历史手续费、合约乘数、最小变动价位或每手每跳价值，下载和缓存 schema v2 artifact，处理长期运行期间的元数据刷新，选择 contract_fee_* / contract_spec_* / TradingDayMeta / PreparedFeeCursors API，处理 TqSdk symbol，或优化跨日 tick 级手续费热路径时，都应使用此 skill。
---

# future-meta 客户端手续费与合约规格指南

这个 skill 用于帮助客户端用户把 `future-meta` 接入自己的 Rust 程序、
回测引擎、交易系统或研究工具。回答时优先解决“该用哪个 API、代码怎么写、
历史时点应使用哪一版手续费和合约规格、热路径怎么放置数据结构”。

## 选择 API

根据用户场景选择最小可用路径：

| 场景 | 推荐 |
| --- | --- |
| 偶尔查询某个合约手续费 | `load_or_fetch` + `contract_fee_asof` |
| 查询当前合约乘数和最小变动价位 | `contract` / `contract_for_handle` |
| 回测历史合约规格 | `contract_spec_asof` / `contract_spec_at` / `contract_spec_on` |
| 已有 `ContractHandle` 并查询历史规格 | `contract_spec_for_handle_at/on` |
| 已有 `OffsetDateTime` | `contract_fee_at` |
| 已经按交易日切分 | `for_trading_day` + `TradingDayMeta::prepare_fee` |
| tick 级或高频回测，多数会跨日 | `FutureMeta::prepare_fee_cursors` |
| 多合约混合 tick | 循环外将 symbol 映射为 `ContractHandle` 和整数 slot |
| 实盘或长期运行需要获取新数据 | 开盘前调用 `load_or_fetch`，替换 `FutureMeta` 快照并重建 handle/cursor |
| 需要查看源规则字段 | `contract_fee_*` / `fee_rule` 这类 raw rule API |
| 本地离线 artifact | `FutureMeta::load_file`，或 `decode_archive_bytes` + `FutureMeta::from_archive` |

如果用户提到 tick、高频、回测循环、逐笔、性能、跨日、多合约或 CPU cache，
直接推荐 `PreparedFeeCursors`。

## 安装和加载

在线自动下载推荐启用 `download` feature：

```toml
[dependencies]
future-meta = { git = "https://github.com/zynthium/future-meta", features = ["download"] }
tokio = { version = "1", features = ["macros", "rt-multi-thread"] }
time = "0.3"
```

```rust
use future_meta::{DownloadConfig, load_or_fetch};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let meta = load_or_fetch(DownloadConfig::default()).await?;
    let spec = meta.contract_spec_asof("DCE.p2605", "2026-04-09T10:00:00+08:00")?;
    let fee = meta.contract_fee_asof("SHFE.cu2607", "2026-06-08T10:48:06Z")?;

    println!(
        "lot_size={}, tick_size={}, tick_value={}, open={:?}",
        spec.lot_size,
        spec.tick_size,
        spec.tick_value(),
        fee.open_fee,
    );
    Ok(())
}
```

要点：

- 默认 manifest 是 `https://future-meta.pages.dev/manifest.json`。
- `load_or_fetch` 会下载并缓存已发布 artifact；archive 加载后，查询都在本地内存完成。
- 当前发布格式是 schema v2，包含手续费和合约规格历史；解码器仍兼容 schema v1。
- 可用 `FUTURE_META_CACHE_DIR` 指定缓存目录。
- 客户端不应该把上游网站当作实时查询 API。

本地 artifact：

```rust
let meta = future_meta::FutureMeta::load_file("latest.fmeta.zst").await?;
```

未启用 `download` feature 时：

```rust
let bytes = std::fs::read("latest.fmeta.zst")?;
let archive = future_meta::archive::decode_archive_bytes(&bytes)?;
let meta = future_meta::FutureMeta::from_archive(archive)?;
```

## 长期运行和实盘更新

`FutureMeta` 是一次加载得到的只读快照，不会在后台自动更新。`load_or_fetch`
只有在被调用时才检查 manifest；如果远端 artifact 变更，会下载新 artifact，
然后返回新的 `FutureMeta`。

推荐策略：

- 回测要可复现时，固定使用某个本地 artifact，不要在回测中途刷新。
- 长期运行的研究或实盘程序，把刷新放在独立调度任务里，绝不要放进 tick 热路径。
- 实盘交易建议在每个交易时段开盘前几分钟调用 `load_or_fetch`。
- 手续费不在日内盘中变化；进入交易时段后通常不需要轮询刷新。
- 刷新成功后，用新的 `FutureMeta` 重新解析 `ContractHandle`，并重新创建
  `TradingDayMeta`、`PreparedFeeBook` 或 `PreparedFeeCursors`。
- 不要跨 `FutureMeta` 快照复用旧的 `ContractHandle`、`TradingDayMeta` 或 cursor。
- 刷新失败时，保留上一份已加载快照并告警；是否继续交易由调用方风控策略决定。

示例形状：

```rust
use std::sync::Arc;

use future_meta::{DownloadConfig, FutureMeta, load_or_fetch};

async fn refresh_meta() -> Result<Arc<FutureMeta>, Box<dyn std::error::Error>> {
    let meta = load_or_fetch(DownloadConfig::default()).await?;
    Ok(Arc::new(meta))
}

// 由调度器在开盘前几分钟调用：
let next_meta = refresh_meta().await?;

// 生产系统中用 ArcSwap、RwLock 或消息切换到 next_meta。
// 切换后基于 next_meta 重建合约 handle 和当天 fee cursor。
```

## 普通查询

低频查询优先使用可读 API：

```rust
let fee = meta.contract_fee_asof("SHFE.cu2607", "2026-06-08T10:48:06Z")?;
let fee = meta.contract_fee_on("SHFE.cu2607", trading_date)?;
```

说明：

- 手续费和规格只接受具体期货合约 symbol，例如 `SHFE.cu2607`、`CZCE.SR903`。
- `KQ.m@...`、`KQ.i@...` 是别名而非具体合约，不能查询手续费或规格，会返回
  `UnsupportedSymbolKind`。需要品种视图时使用 `underlying_fees_*`，结果始终是具体合约。
- 手续费按交易所本地日期生效，不在日内盘中变化。
- `contract_fee_asof` 会把时间戳映射到交易所本地日期，再选择该日手续费。
- raw rule API 返回 `ContractFee`，适合展示、核对、审计和低频逻辑。

## 合约乘数和最小变动价位

`contract` 返回 archive 中该合约的最新已知规格。历史回测不要把这个最新值套用到
所有日期，应使用 `contract_spec_*` 查询目标时点的 `ContractSpecVersion`：

```rust
let current = meta.contract("DCE.p2605")?;
println!("current tick={}", current.tick_size);

let before = meta.contract_spec_asof(
    "DCE.p2605",
    "2026-04-09T10:00:00+08:00",
)?;
let after = meta.contract_spec_asof(
    "DCE.p2605",
    "2026-04-10T10:00:00+08:00",
)?;

assert_eq!(before.lot_size, 10.0);
assert_eq!(before.tick_size, 2.0);
assert_eq!(after.tick_size, 1.0);
assert_eq!(after.tick_value(), 10.0);
```

字段语义：

- `lot_size`：一手包含的标的数量，即合约乘数。
- `tick_size`：报价单位中的最小价格变动。
- `tick_value()`：`lot_size * tick_size`，即一手每跳价值；它是派生值，不单独持久化。
- `valid_from` 是闭区间起点，`valid_to` 是开区间终点；`None` 表示该版本之后无后继规格。
- `TradingDayMeta::contract_spec` 和 `contract_spec_for_handle` 可查询交易日对应规格。

`PreparedFee`、`TradingDayMeta::prepare_fee` 和 `PreparedFeeCursors` 会自动使用目标交易日
有效的历史 `lot_size` 编译成交金额比例手续费。不要再从 `contract()` 取最新乘数手工计算
历史比例手续费，否则规格调整前后的金额会被算错。

schema 兼容边界：

- schema v2 保存真实的 `contract_spec_versions`，应作为历史规格查询的标准来源。
- schema v1 可以被当前客户端读取，但只能把当时的静态规格转换成单一历史区间，无法恢复
  v1 文件本身没有记录的规格变化。
- 老客户端读取 schema v2 会返回 `UnsupportedSchemaVersion`；遇到此错误应升级依赖，
  不要绕过版本检查或自行反序列化。

## 高频回测

tick 循环内避免做字符串查询、时间解析、`HashMap` 查询或手续费类型分支。
循环外完成 symbol 解析、slot 映射、日级手续费准备和 cursor 初始化。

```rust
let cu = meta.resolve_contract("SHFE.cu2607")?;
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

性能要点：

- `PreparedFee` 已把手续费规则编译成数值系数。
- 正常 tick 路径是一条 `i64` 时间比较和一次整数 slot 读取。
- 只有到交易日边界时，才慢路径重建当天 fee book。
- tick 时间必须单调递增；乱序数据应先排序或分段。

更重视简洁时可用：

```rust
let fee = fees.advance_and_get(tick.trading_date, tick.fee_slot, tick.unix_nanos)?;
```

## 多合约混合 tick

进入循环前建立紧凑 slot 映射，tick 结构里保存 `fee_slot: usize`：

```rust
let handles = [
    meta.resolve_contract("SHFE.cu2607")?,
    meta.resolve_contract("DCE.m2609")?,
];
let mut fees = meta.prepare_fee_cursors(
    handles,
    first_tick.trading_date,
    first_tick.unix_nanos,
)?;

for tick in ticks {
    if tick.unix_nanos >= fees.next_change_unix_nanos() {
        fees.advance_to(tick.trading_date, tick.unix_nanos)?;
    }

    let fee = fees.current(tick.fee_slot)?;
    pnl -= fee.close_today_amount(tick.price, tick.lots);
}
```

注意：

- slot 顺序等于传给 `prepare_fee_cursors` 的 handle 顺序。
- slot 越界会返回 `InvalidFeeSlot`。
- `ContractHandle` 只对创建它的 `FutureMeta` 及其 clone 有效。

## 已按日切分

如果用户的回测框架已经按交易日切分，单日 API 更直接：

```rust
let day = meta.for_trading_day(trading_date)?;
let handle = day.resolve_contract("SHFE.cu2607")?;
let fee = day.prepare_fee(handle)?;

for tick in day_ticks {
    cost += fee.open_amount(tick.price, tick.lots);
}
```

多个合约时用 `prepare_fee_book(handles)`，循环内按整数 slot 取 `PreparedFee`。

## 时间和交易日

- `unix_nanos` 是 Unix 纳秒，用于热路径比较。
- `trading_date` 是交易所本地 calendar date。
- 如果回测系统使用夜盘归属交易日，应在进入 future-meta API 前完成映射。
- 不要在 tick 循环里解析 RFC3339 字符串。

## 费用计算

`PreparedFee` 暴露金额计算方法：

```rust
let open = fee.open_amount(price, lots);
let close_today = fee.close_today_amount(price, lots);
let close_yesterday = fee.close_yesterday_amount(price, lots);
```

不要让用户在热路径里判断手续费类型。`CnyPerLot`、
`TurnoverRatePerTenThousand` 和 `Zero` 会在准备阶段编译成数值系数。
无法编译的规则会返回 `UnsupportedFeeRule`。

## 错误处理

- `FutureMetaError` 是 `#[non_exhaustive]`，匹配时保留通配分支。
- `InvalidSymbol`：symbol 格式不符合 TqSdk 风格。
- `NoVersionAt`：目标日期没有可用手续费或合约规格版本。
- `InvalidFeeSlot`：tick 的 slot 和准备好的 handle 表不一致。
- `UnsupportedFeeRule`：规则不能编译进 `PreparedFee` 热路径，可用 raw rule API 核对。
- `UnsupportedSchemaVersion`：客户端版本落后于 artifact schema，应升级 `future-meta`。

## 回答风格

- 先给推荐 API，再给最小代码。
- 用户问性能时，明确区分循环外准备和循环内热路径。
- 用户问历史回测时，明确区分 `contract()` 的最新规格与 `contract_spec_*` 的 as-of 规格。
- 用户手工计算比例手续费时，优先改为 `PreparedFee`，确保使用历史乘数。
- 用户只想查询一次时，不要引入高频架构。
- 用户问最优设计时，优先推荐跨日 `PreparedFeeCursors`。
