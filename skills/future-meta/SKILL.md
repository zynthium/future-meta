---
name: future-meta
description: 指导 Rust 与 Python 客户端使用 future-meta 按需下载刷新中国期货元数据，查询历史手续费、多空保证金、日内开仓费及合约规格，判断数据覆盖和交易日边界，并选择普通查询或 tick 级预编译 API。
---

# future-meta 客户端手续费、保证金与合约规格指南

这个 skill 用于帮助客户端用户把 `future-meta` 接入自己的 Rust 或 Python 程序、
回测引擎、交易系统或研究工具。回答时优先解决“该用哪个 API、代码怎么写、
历史时点应使用哪一版手续费、保证金和合约规格、数据是否覆盖目标日期、热路径怎么放置数据结构”。

## 选择 API

根据用户场景选择最小可用路径：

| 场景 | 推荐 |
| --- | --- |
| 偶尔查询某个合约手续费 | `load_or_fetch` + `contract_fee_asof` |
| 查询历史多空保证金率 | `contract_fee_*` 返回的 `buy_margin_rate` / `sell_margin_rate` |
| 查询日内开仓费或计算金额 | `ContractFee.intraday_open_fee` 字段 / `PreparedFee::intraday_open_amount` |
| 判断历史是否覆盖目标日期 | `update-state.json` 的核验截止和例外 + 查询结果；历史缺口需结合本地证据 |
| 查询当前合约乘数和最小变动价位 | `contract` / `contract_for_handle` |
| 回测历史合约规格 | `contract_spec_asof` / `contract_spec_at` / `contract_spec_on` |
| 已有 `ContractHandle` 并查询历史规格 | `contract_spec_for_handle_at/on`（Python 为 `contract_spec_for_handle_asof/on`） |
| 已有 `OffsetDateTime`（Rust） | `contract_fee_at` |
| 已经按交易日切分 | `for_trading_day` + `TradingDayMeta::prepare_fee` |
| tick 级或高频回测，日期与时间戳符合自然日边界 | `FutureMeta::prepare_fee_cursors`；夜盘先看“时间和交易日” |
| 多合约混合 tick | 循环外将 symbol 映射为 `ContractHandle` 和整数 slot |
| 实盘或长期运行需要获取新数据 | 保留 `DownloadClient` 并按需调用 `refresh()`；仅返回 `true` 时重建 handle/cursor |
| 需要查看源规则字段 | `contract_fee_*` / `fee_rule` 这类 raw rule API（Python 支持 `.to_dict()`） |
| 本地离线 artifact | Rust: `FutureMeta::load_file` 或 `decode_archive_bytes` + `FutureMeta::from_archive`；Python: `FutureMeta.from_file` / `FutureMeta.from_compressed_bytes` |

如果用户提到 tick、高频、回测循环、逐笔、性能、跨日、多合约或 CPU cache，
先核对交易日与实际时间戳的边界；符合 cursor 自然日约束时推荐 `PreparedFeeCursors`。
按夜盘归属交易日切分的流优先用 `for_trading_day` 准备费用，见“时间和交易日”。

## 安装和加载

在线下载启用 `download` feature。可通过 crates.io 或固定 Git 标签引入依赖：

```toml
[dependencies]
future-meta = { version = "0.9", features = ["download"] }
# 或使用固定 Git 标签：
# future-meta = { git = "https://github.com/zynthium/future-meta", tag = "v0.9.0", features = ["download"] }
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

- 默认 manifest 是 `https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json`。
- `load_or_fetch` 读取独立 `data` 分支的 manifest，再加载 SHA 校验通过的磁盘缓存或其指向的不可变 `data-*` Release 附件。每次调用都会创建查询实例、解码并构建索引，适合一次性加载。
- `DownloadClient` 从 SDK v0.8.0 提供，供长驻进程复用 HTTP 连接和当前查询快照；周期刷新见下一节。
- 包含大商所月均价 `F` 合约的 archive 需要 SDK v0.7.0 或更新版本。v0.6.0 及更早版本会拒绝这些 symbol，需要升级一次；之后兼容的每日数据更新不要求升级 SDK。
- 当前导出格式为 schema 4；共享字典与按合约变更序列只改变传输编码，不删除手续费、保证金或合约规格历史。
- 不再使用 Pages 或线上维护包，也不兼容旧托管地址。数据 Release 不占用 SDK Release 的 Latest，客户端应始终跟随 manifest。
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

### Python 安装与加载

Python 绑定位于独立 crate `crates/future-meta-py`，通过 `PyO3 + maturin`（`abi3-py310`，兼容 Python 3.10+）编译为原生扩展模块 `future_meta`，内部托管 Tokio Runtime 并在网络/磁盘 I/O 与解压时通过 `py.detach` 释放 GIL，对外提供同步阻塞接口。

可直接从 PyPI 安装，或从 GitHub Release 下载对应平台的 `.whl` 附件、在仓库根目录本地编译安装：

```bash
pip install future-meta
# 或在本地源码目录构建安装：
pip install maturin
maturin develop --release
```

Python 在线与离线加载：

```python
import future_meta

# 在线下载或读取校验通过的本地缓存
meta = future_meta.load_or_fetch()
spec = meta.contract_spec_asof("DCE.p2605", "2026-04-09T10:00:00+08:00")
fee = meta.contract_fee_asof("SHFE.cu2607", "2026-06-08T10:48:06Z")
print(spec.lot_size, spec.tick_size, spec.tick_value(), fee.open_fee.to_dict())

# 离线加载本地文件或字节
meta_from_file = future_meta.FutureMeta.from_file("latest.fmeta.zst")
```

## 长期运行和实盘更新

`FutureMeta` 是加载得到的只读快照，不会在后台自动更新。长期运行时在启动阶段
创建并保留一个 `DownloadClient`，平时通过 `client.meta()` 本地查询，由调用方按需调度 `refresh()`。

刷新语义：

- `refresh()` 复用 HTTP 连接池；服务端提供 ETag 时，下次发送 `If-None-Match`，ETag 仅保存在内存中。
- HTTP 304 或 archive SHA 未变时返回 `false`：不下载 archive、不读磁盘缓存、不解码或重建索引；当前实例、句柄和预计算表继续有效。
- archive SHA 变化时，加载候选包并完成 SHA 校验、解码和必要的缓存写入后才替换实例，返回 `true`。此时重新解析 `ContractHandle`，并重建 `TradingDayMeta`、`PreparedFeeBook` 或 `PreparedFeeCursors`。
- 下载、校验、解码或缓存写入失败会返回错误，保留当前内存快照、ETag 和已有有效缓存；是否继续交易由调用方风控策略决定。
- 每次刷新仍发出一次 manifest 检查请求。客户端没有后台轮询、刷新间隔或交易日历门禁，频率由调用方控制；`meta()` 查询不访问网络或磁盘。

推荐策略：

- 回测要可复现时，固定使用某个本地 artifact，不要在回测中途刷新。
- 长期运行的研究或实盘程序，把刷新放在独立调度任务里，绝不要放进 tick 热路径。
- 根据发布节奏和应用需求调度 `refresh()`，不必每个交易时段都重新加载。需要判断交易所最近核验时间时单独读取状态文件，见“数据来源与覆盖边界”。
- 当前手续费和保证金查询使用日度索引；刷新频率按发布节奏安排。这不证明交易所从未在盘中调整参数。
- 不要把旧句柄或预计算表传给替换后的实例。旧 `FutureMeta` 的 clone 仍可配合其原有句柄和表继续使用。

在 async 函数中使用：

```rust
use future_meta::{DownloadClient, DownloadConfig};

// 启动时创建，后续检查复用同一实例。
let mut client = DownloadClient::load(DownloadConfig::default()).await?;
let mut handle = client.meta().resolve_contract("SHFE.cu2701")?;

// 由应用调度器在需要检查更新时调用。
if client.refresh().await? {
    handle = client.meta().resolve_contract("SHFE.cu2701")?;
    // 在此基于新快照重建业务使用的预计算表。
}
let contract = client.meta().contract_for_handle(handle)?;
```

多读者共享时可发布 `client.meta().clone()` 得到同一查询快照；只在 `refresh()` 返回
`true` 后切换读者所用快照。旧 clone 与旧句柄保持配套，不要在持有借用的 cursor 时刷新其所属客户端。

在 Python 中使用 `DownloadClient`（同步调用）：

```python
import future_meta

client = future_meta.DownloadClient.load()
meta = client.meta()
handle = meta.resolve_contract("SHFE.cu2701")

if client.refresh():
    meta = client.meta()
    handle = meta.resolve_contract("SHFE.cu2701")

contract = meta.contract_for_handle(handle)
```

## 普通查询

低频查询优先使用可读 API：

```rust
let fee = meta.contract_fee_asof("SHFE.cu2607", "2026-06-08T10:48:06Z")?;
let fee = meta.contract_fee_on("SHFE.cu2607", trading_date)?;
```

说明：

- 手续费和规格只接受具体期货合约 symbol，例如 `SHFE.cu2607`、`CZCE.SR903`。
- 支持 `DCE.l2611F`、`DCE.pp2611F`、`DCE.v2611F` 月均价合约，必须保留大写 `F`。它们有独立费率、规格和生命周期，不能删除后缀或借用同月份普通合约数据。
- 月均价合约的 TqSdk 品种标识仍为 `DCE.l`、`DCE.pp`、`DCE.v`；不要把内部规则键 `l-F/pp-F/v-F` 当成 SDK symbol。`underlying_fees_*` 包含该品种的普通及月均价具体合约，仅排除明确 `NotTrading` 的版本，保留状态未知的版本。
- `KQ.m@...`、`KQ.i@...` 是别名而非具体合约，不能查询手续费或规格，会返回
  `UnsupportedSymbolKind`。需要品种视图时使用 `underlying_fees_*`，结果始终是具体合约。
- 手续费和保证金版本按交易所本地日期进入日度索引，不还原原件未提供的盘中调整时刻。
- `contract_fee_asof` / `contract_fee_at` 按北京时间自然日选择手续费，不自动映射夜盘归属交易日；夜盘使用显式日期的 `*_on` 或 `for_trading_day`，见“时间和交易日”。
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

- schema 4 使用字典和合约变更序列，解码后仍返回 `FeeArchiveV3` 并保持查询 API；新格式上线前先升级客户端。
- schema 3 包含规格历史及独立的 `intraday_open_fee`；schema 2 包含规格历史，但没有独立日内开仓费。
- schema 1/2/3 解码后升级为当前 `FeeArchiveV3`；schema 1/2 旧文件的 `intraday_open_fee` 为 `None`，不会从普通开仓费推算。
- schema 1 只能把静态规格转换成单一历史区间，不能恢复原文件未记录的规格变化。
- 旧载荷类型保留为 `FeeArchiveV1` / `FeeArchiveV2`，其费行类型为 `LegacyContractFee`。
  手工构造当前 `ContractFee` 的调用方需要补充 `intraday_open_fee` 字段。
- 遇到 `UnsupportedSchemaVersion` 时升级客户端，不要绕过版本检查或自行反序列化。

## 保证金和日内开仓费

保证金和手续费共用 `ContractFee` 的版本区间。即使手续费不变，仅保证金变化也会产生新版本；
`rule_hash` 标识手续费规则，不包含保证金，不要用它判断整个参数状态是否相同。

```rust
let rule = meta.contract_fee_asof("DCE.bb2001", "2020-01-02T10:00:00+08:00")?;
println!(
    "long_margin={:?}, short_margin={:?}, intraday_open={:?}",
    rule.buy_margin_rate,
    rule.sell_margin_rate,
    rule.intraday_open_fee,
);
```

字段和计算语义：

- `buy_margin_rate` / `sell_margin_rate` 为多头/空头投机保证金率，按百分点表示，例如 `12.0` 为 12%，计算比例时除以 100。
- `intraday_open_fee` 独立于普通开仓 `open_fee` 和平今 `close_today_fee`，不能互相替代。
- 费用金额单位为元/手，成交金额比例单位为万分比；优先用 `PreparedFee` 计算金额，自动使用历史乘数。
- 保证金率或独立日内开仓费为 `None` 表示未知，不是零；不要自动补成零或普通开仓费。

## 数据来源与覆盖边界

日更以已批准客户端 archive 为业务历史基线，只需轻量检查点和已批准小规则配置，
按顺序补齐缺失交易日的六所官方参数；不下载线上 SQLite、历史原件或维护包。
历史证据、逐日观测和维护 SQLite 仅在发布方本地保留。券商公告只用于人工发现线索，不能代替交易所证据。

日更复用已批准规格、生命周期和适用范围，不请求全量规则文档或规格目录；
中金所日度参数目录及基准快照校验仍执行。独立规则审阅另行维护小配置，
不能把日度参数检查成功解释为当日重新核验全部规格。月均价手续费复用同一份 DCE 响应，不增加交易所请求。

查询前区分发布时间、观测截止日和版本有效期：

- `Manifest.generated_at` / `data_version` 是构建或缓存版本信息，不是源数据最新日期。
- 当前导出器的 `history_end` / `fee_effective_from` 是最后一个已批准参数版本的起点；最后一次变化可能早于最新观测。
- 核验状态入口为 `https://raw.githubusercontent.com/zynthium/future-meta/data/update-state.json`，不再提供旧线上 `coverage.json`。SDK 下载接口不自动加载此状态文件。
- `last_attempt` 记录最近尝试和结果；`running` 可能来自硬终止，需要检查对应 Actions run。`last_success_at` 记录最近日度参数检查成功时间，无业务变化也推进；迁移后未知时间可能为 `null`。
- `checked_through` 按交易所记录日度参数完整核验截止日。它不保证此前每个合约任意历史日期都有观测；存在静态合约元数据也不证明存在对应手续费或保证金。查询不到时保留未知，不回填旧值或零。
- `exceptions.without_observations` 记录无观测合约；`exceptions.assumed_expiry` / `exceptions.assumptions` 保留明确标记的到期日假设，推算日期不等于交易所确认日期。检查点不包含完整逐日观测或全部历史缺口，单合约日期溯源需结合客户端区间与发布方本地证据。
- `source_versions.specifications` 记录规则配置 SHA、`approved_through` 和 `live_check_performed=false`。日度参数核验截止日不等于规格规则批准截止日。
- `archive_sha256` 把状态绑定到业务基线。联合判断 manifest 与状态时核对该值与 manifest SHA 一致；需要严格一致快照时从 `data` 分支同一 commit 读取两文件，避免 raw CDN 延迟混读。
- `valid_to = None` 表示版本没有已知后继边界，可能在观测截止日之后仍返回最后已知参数，不能据此认定之后的日期已经核验。

无业务变化时 archive 字节、SHA、生成时间、manifest 和数据 Release 不变，仅更新状态；
因此 `refresh() == false` 不表示当天没有执行交易所检查。失败只记录尝试，不推进成功检查点。
官方参数未发布、反爬页、日期错误、空表或元数据缺失时停止整批，不能以旧值冒充当日数据。

发布方通常在北京时间工作日 18:45 夜盘前采集，08:05 仅用于法定休市后首个早晨；
周末、普通早晨、休市及节前无夜盘等时段由离线官方日历跳过。
这约束交易所采集，不限制客户端读取 GitHub 或本地查询。需要当前状态时读取 manifest 和检查点，
不把某次发布的计数写成永久保证；成功下载不等于源数据已更新到今天。

## 高频回测

以下 cursor 示例要求所传日期与 tick 时间戳处于同一北京时间自然日；夜盘归属日期另见“时间和交易日”。

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

Python 高频回测同样支持 `prepare_fee_cursors` 与 `for_trading_day`（交易日参数传 `"YYYY-MM-DD"` 字符串）：

```python
cu = meta.resolve_contract("SHFE.cu2607")
fees = meta.prepare_fee_cursors([cu], "2026-06-04", first_tick_unix_nanos)

for tick in ticks:
    if tick.unix_nanos >= fees.next_change_unix_nanos():
        fees.advance_to(tick.trading_date, tick.unix_nanos)
    fee = fees.current(tick.fee_slot)
    cost += fee.open_amount(tick.price, tick.lots)
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
- 时间戳查询按北京时间自然日取值，不自动推断夜盘、周末或节假日后的归属交易日。
- 对夜盘先由调用方映射归属交易日，再用 `contract_fee_on`、`contract_spec_on` 或 `for_trading_day` 查询和准备费用。例：2025-10-28 21:00 开始的首批月均价夜盘属于 2025-10-29，不能用自然日 2025-10-28 查询其当日费率。
- `PreparedFeeCursors` 的日期和 `unix_nanos` 必须落在该日期北京时间零点至次日零点的半开区间。仅将夜盘日期改成下一交易日而保留原时间戳会被拒绝；不要伪造 Unix 时间来绕过校验。此类夜盘流按归属交易日使用 `for_trading_day(...).prepare_fee/prepare_fee_book`，在调用方的交易日边界切换预计算表。
- 不要在 tick 循环里解析 RFC3339 字符串。

## 费用计算

`PreparedFee` 暴露金额计算方法：

```rust
let open = fee.open_amount(price, lots);
let intraday_open: Option<f64> = fee.intraday_open_amount(price, lots);
let close_today = fee.close_today_amount(price, lots);
let close_yesterday = fee.close_yesterday_amount(price, lots);
```

不要让用户在热路径里判断手续费类型。`CnyPerLot`、
`TurnoverRatePerTenThousand` 和 `Zero` 会在准备阶段编译成数值系数。
无法编译的规则会返回 `UnsupportedFeeRule`。独立日内开仓费未知时，
`intraday_open_amount` 返回 `None`，由调用方明确处理缺失。

## 错误处理

- Rust 中 `FutureMetaError` 是 `#[non_exhaustive]`，匹配时保留通配分支；Python 中所有客户端错误统一抛出 `future_meta.FutureMetaError` 异常。
- `InvalidSymbol`：symbol 格式不符合 TqSdk 风格。
- `NoVersionAt`：目标日期没有可用手续费或合约规格版本。
- `InvalidFeeSlot`：tick 的 slot 和准备好的 handle 表不一致。
- `InvalidContractHandle`：句柄不属于当前查询实例；数据变化后需基于新实例重新解析。
- `InvalidTimestamp`：时间戳格式或范围无效，或 cursor 的日期与实际时间戳不匹配。
- `UnsupportedFeeRule`：规则不能编译进 `PreparedFee` 热路径，可用 raw rule API 核对。
- `UnsupportedSchemaVersion`：客户端版本落后于 artifact schema，应升级 `future-meta`。

## 回答风格

- 先给推荐 API，再给最小代码。
- 用户问性能时，明确区分循环外准备和循环内热路径。
- 用户问历史回测时，明确区分 `contract()` 的最新规格与 `contract_spec_*` 的 as-of 规格，并核对观测范围和未知区间。
- 用户问保证金或日内开仓费时，明确说明字段单位与 `None` 语义，不根据手续费哈希判断保证金是否变化。
- 用户手工计算比例手续费时，优先改为 `PreparedFee`，确保使用历史乘数。
- 用户只想查询一次时，不要引入高频架构。
- 用户问最优设计时，根据交易日边界选择 `PreparedFeeCursors` 或按归属交易日准备的 fee book；长驻刷新保留 `DownloadClient`，只在 `refresh()` 返回 `true` 后重建。
