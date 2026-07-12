---
name: future-meta
description: future-meta Rust 工作区专用流程。用于本仓库内修改客户端 archive/query/download API、手续费解析、高频回测手续费热路径、PreparedFee/PreparedFeeCursor、daemon 抓取/SQLite/export、Cloudflare artifact 文档、测试或性能敏感元数据逻辑时。
---

# future-meta

使用此 skill 时，把项目边界放在首位：`future-meta` 客户端只做本地
archive 解码、索引和低分配查询；`future-meta-daemon` 负责抓取源站、
维护 SQLite 历史、导出静态 artifact。

## 开始前

- 先确认 `AGENTS.md` 已加载；没有加载就读它。
- 所有 shell 命令使用 `rtk` 前缀。
- 编辑前看 worktree；不要回滚无关改动。
- 不要提交 `data/`、`public/`、`target/`、`.env`、密钥或本地生成物。

## 客户端边界

修改 `crates/future-meta` 时：

- archive 只保留查询必要基础字段。
- 不要持久化可从 `symbol` 或上下文派生的字段。
- 不要写入源站展示用派生字段，例如价格、涨跌停、每手保证金、
  每跳盈亏、手续费折算金额、开平合计手续费。
- archive 加载后，查询路径必须是本地读，不允许网络请求。
- 抓取、HTML/CSV 解析、SQLite、Cloudflare export 逻辑留在 daemon。
- 改 `SCHEMA_VERSION`、公开模型字段或编码方式时，必须补兼容测试和迁移说明。

## 高频手续费最优路径

目标：tick 循环里不要查字符串、不要 `HashMap`、不要解析时间、不要匹配
`FeeKind`、不要每 tick 二分查找。循环外完成 symbol 解析、交易日快照、
手续费规则编译和 cursor 初始化。

实际回测通常跨多日。默认按跨日流设计；单日 `TradingDayMeta` 只是内部缓存
单元，不应要求调用方在策略循环里手工写大量日切换逻辑。

### 跨日回测默认路径

推荐抽象：外层持有“当前交易日缓存 + 当前手续费 cursor/book + 下一次事件时间”。

```rust
let mut fees = meta.prepare_fee_cursors(handles, first_tick.trading_date, first_tick.unix_nanos)?;

for tick in ticks {
    if tick.unix_nanos >= fees.next_change_unix_nanos() {
        fees.advance_to(tick.trading_date, tick.unix_nanos)?;
    }

    let fee = fees.current(tick.fee_slot)?;
    pnl -= fee.open_amount(tick.price, tick.lots);
}
```

不要要求调用方每次手写：

```rust
let day = meta.for_trading_day(trading_date)?;
let handle = day.resolve_contract("SHFE.cu2607")?;
```

跨日包装 API 设计要求：

- 以“tick 按时间单调递增”为前提。
- 内部按交易日重建 `TradingDayMeta` 和 slot cursor。
- `next_change_unix_nanos` 应取三者最小值：当前手续费变化时间、当前交易日结束、
  已知合约/slot 需要重建的下一边界。
- tick 热路径仍是一次 `i64` 比较 + slot 读当前 `PreparedFee`。
- 交易日变化时慢路径重建日缓存；复杂度应接近 `O(ticks + fee_changes + days)`。
- 不要在 tick 循环内调用 `for_trading_day`、`resolve_contract`、symbol 查询或字符串时间解析。
- 当前项目的 `trading_date` 指交易所本地 calendar date；如上层回测使用夜盘归属交易日，
  必须在进入 fee API 前明确映射，避免混用概念。

### 日内手续费固定

如果调用方已在外层按日切分，最快路径是每天循环外编译一次 `PreparedFee`，
日内循环只做数值计算。

```rust
let fee = day.prepare_fee(handle)?;

for tick in ticks {
    pnl -= fee.open_amount(tick.price, tick.lots);
}
```

设计要求：

- `CnyPerLot`、`TurnoverRatePerTenThousand`、`Zero` 必须在准备阶段编译成数值系数。
- `PreparedFee` 热路径只暴露金额计算方法：`open_amount`、
  `close_today_amount`、`close_yesterday_amount`。
- 遇到 `Unknown` 或无法编译的手续费，准备阶段返回 `UnsupportedFeeRule`，
  不要在循环内兜底。

### 日内手续费可能变化

如果调用方已在外层按日切分，最优 exact-asof 路径是使用
`PreparedFeeCursor`，缓存当前手续费和下一次变化时间。

```rust
let mut cursor = day.prepare_fee_cursor(handle, first_tick.unix_nanos)?;

for tick in ticks {
    if tick.unix_nanos >= cursor.next_change_unix_nanos() {
        cursor.advance_to_unix_nanos(tick.unix_nanos)?;
    }

    let fee = cursor.current();
    pnl -= fee.open_amount(tick.price, tick.lots);
}
```

设计要求：

- tick 必须按时间单调递增；乱序 tick 不适合 cursor，应改用二分或先排序/分段。
- `next_change_unix_nanos` 使用 `i64` Unix nanoseconds，避免 `OffsetDateTime` 热路径成本。
- cursor 只在 fee boundary 或交易日边界慢路径推进，复杂度为
  `O(ticks + fee_changes)`。
- cursor 必须拒绝交易日外时间，包含交易日下界和上界。
- `valid_from` 闭区间，`valid_to` 开区间。
- 如果回测跨日，不要直接把单日 `PreparedFeeCursor` 暴露给策略循环长期持有；
  应使用外层跨日 cursor/book 管理日切换。

### 多合约混合 tick

入场前把 symbol 映射成紧凑 slot；tick 循环只使用整数 slot。

```rust
let mut fees = meta.prepare_fee_cursors(handles, first_tick.trading_date, first_tick.unix_nanos)?;

for tick in ticks {
    let fee = fees.advance_and_get(tick.trading_date, tick.fee_slot, tick.unix_nanos)?;
    pnl -= fee.close_today_amount(tick.price, tick.lots);
}
```

设计要求：

- `prepare_fee_cursors` 的 slot 顺序必须等于调用方传入 handles 顺序。
- slot 越界返回 `InvalidFeeSlot`。
- `ContractHandle` 只对生成它的 `FutureMeta` 及其 clone 有效；跨 archive/client
  使用必须返回 `InvalidContractHandle`。

## 普通查询 API

非热路径可以继续使用：

- `contract_fee_asof(symbol, at)`
- `contract_fee_at(symbol, at)`
- `contract_fee_on(symbol, trading_date)`
- `contract_fee_for_handle_at(handle, at)`
- `contract_fee_for_handle_on(handle, trading_date)`
- `TradingDayMeta::fee_rule(handle)`

不要把这些 API 推荐为跨日 tick 循环最优解。高频文档优先推荐
`FutureMeta::prepare_fee_cursors` 和 `PreparedFeeCursors`。

## 测试策略

客户端查询/API 改动：

1. 优先补 `crates/future-meta/tests/client_archive.rs` 行为测试。
2. 覆盖 `valid_from` inclusive、`valid_to` exclusive、等价 UTC 时间。
3. 覆盖 no-version、history start、未知合约、stale handle。
4. 涉及 cursor 时，覆盖日内边界、交易日上下界、slot 顺序。
5. 涉及 `PreparedFee` 时，覆盖固定费用、成交额费率、零手续费、未知手续费拒绝。

daemon/parser/DB/export 改动：

- 更新 daemon 相关测试，尤其 `daemon_pipeline`。
- 源站异常、latest 表缺失、空数据必须硬失败；不要发布空或过期 artifact。

## 文档策略

- 用户可见 API 或推荐用法变化：更新 `README.md`。
- 客户端高频用法、破坏性 API 说明：更新 `docs/client-api.md`。
- 部署和 GitHub Actions 行为变化：更新 `docs/deployment.md`。
- 文档写任务用法和取舍，不写无用实现流水账。

## 验证命令

迭代时先跑窄测试。收尾至少运行：

```bash
rtk cargo fmt --all -- --check
rtk cargo check --workspace --all-targets
rtk cargo test --workspace
rtk cargo test -p future-meta --features download
```

按影响面加跑：

```bash
rtk cargo test -p future-meta --test client_archive
rtk cargo test -p future-meta-daemon --test daemon_pipeline
rtk cargo clippy --workspace --all-targets --all-features
```

`clippy` 可能有既有 daemon warning；新增/触碰的客户端代码不要引入新 warning。
