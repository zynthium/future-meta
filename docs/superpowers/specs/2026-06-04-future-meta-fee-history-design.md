# future-meta 手续费历史服务设计

日期：2026-06-04

## 背景

`future-meta` 当前是一个 Rust workspace，包含两个 crate：

- `future-meta`：面向业务程序的 client/library crate。
- `future-meta-daemon`：面向数据采集、历史维护和发布的 daemon crate。

目标是维护国内期货手续费和保证金规则的历史版本，让程序可以在本地高性能执行 as-of 查询。数据源第一版使用九期网的手续费页面和 CSV 下载端点：

- 总表页：`https://www.9qihuo.com/qihuoshouxufei`
- 单品种页：`https://www.9qihuo.com/qihuoshouxufeisingle?heyue=<code>`
- CSV 下载：`https://www.9qihuo.com/shouxufeixz?heyue=<code>`

九期网单品种 CSV 返回的是品种当前/近期合约数据；同一个合约可能在同一份 CSV 中出现多行，并用“手续费更新时间”表达不同手续费规则版本。它不是完整的按日期查询历史 API，因此本项目会尽量吸收源 CSV 暴露的版本行，同时不保证源站未返回的更早历史可被准确查询。

最新截面使用总表页 HTML 的 `table#heyuetbl`。该页“下载手续费 Excel 表格”按钮通过浏览器端 `tableToExcel('heyuetbl', ...)` 从 HTML 表格生成文件，不提供稳定的全合约 CSV 下载端点；因此 latest 更新直接解析 HTML 表格，只读取允许字段。

## 目标

- 自动发现全部期货品种并定期更新。
- 维护手续费和保证金规则的历史版本。
- 支持合约、品种、主力合约维度的 as-of 查询。
- 将发布给 client 的数据打包为压缩二进制文件。
- client 自动下载、校验、缓存和加载二进制文件。
- client 查询全部在本地执行，避免运行时依赖在线查询服务。
- 第一版部署使用 Cloudflare 免费方案完成公共分发。

## 非目标

- 不做实时行情服务。
- 不存储或查询行情派生字段。
- 不提供服务端在线查询 API 作为第一版主路径。
- 不在 Cloudflare Worker 内运行完整 Rust daemon。
- 不保证源站 CSV 未返回的更早历史可被准确查询。
- 第一版不实现 client 端 delta patch；manifest 未变则不下载，manifest 变化则下载完整压缩二进制文件。
- GitHub Actions 不执行初始全量历史抓取；初始 seed 在本地完成并发布到 Cloudflare。

## 派生字段存储约束

所有行情派生字段都不得持久化，也不得进入发布给 client 的二进制文件。

明确禁止持久化的字段：

- `现价`
- `涨/跌停板`
- `保证金/每手(元)`
- `每跳毛利/元`
- `手续费(开+平)/元`
- `每跳净利/元`
- 完整原始 CSV 内容
- 基于完整 CSV 或完整原始行计算的持久化 hash

daemon 可以将 CSV 作为临时输入读取，但解析完成后只保存允许字段。增量 hash 只能基于允许字段计算，不能让禁止字段影响版本判断。

## 允许持久化字段

合约标识字段：

- `symbol`：唯一合约标识，严格对齐天勤量化 TqSdk 命名规则
- 上市日期
- 到期日期
- 是否正在交易
- 是否主力合约

不持久化品种名称、合约名称、交易所名称、交易所编码、裸合约代码或裸品种代码。程序如需品种维度查询，从期货合约 `symbol` 派生 `underlying_symbol`，例如 `SHFE.cu2607` 派生为 `SHFE.cu`。`underlying_symbol` 是查询索引键，不替代 `symbol` 作为合约唯一标识。

## TqSdk symbol 规范

`symbol` 是本项目唯一持久化的合约身份字段。第一版采集和发布的数据只覆盖期货合约；主连和指数作为 client 查询别名支持；期权、套利和证券交易所代码只作为后续兼容边界，不进入第一版采集范围。

第一版必须支持的交易所前缀：

- `SHFE`
- `DCE`
- `CZCE`
- `CFFEX`
- `INE`
- `GFEX`

期货合约 symbol：

- 格式：`交易所代码.交易所内合约代码`
- 示例：`SHFE.cu2607`、`CFFEX.IF2406`、`DCE.m2501`
- 郑商所 `CZCE` 期货到期年份数字使用三位，例如 `CZCE.SR903`
- 其他期货交易所到期年月数字使用四位，例如 `SHFE.cu2607`

主连和指数查询别名：

- 主连格式：`KQ.m@交易所代码.品种代码`，例如 `KQ.m@SHFE.cu`
- 指数格式：`KQ.i@交易所代码.品种代码`，例如 `KQ.i@SHFE.bu`
- daemon 不将 `KQ.*` 别名作为独立合约版本入库
- client 收到 `KQ.m@...` 时，转换为对应 `underlying_symbol` 后查询当时标记为主力的期货合约
- client 收到 `KQ.i@...` 时，第一版返回 `UnsupportedSymbolKind`，因为本项目不维护指数行情或指数手续费规则

期权和套利：

- 期权 symbol 规则因交易所而异，例如 `DCE.m1807-C-2450`、`SHFE.au2004C308`
- 套利/组合 symbol 例如 `DCE.SP a1709&a1801`
- 第一版 parser 必须识别这些 symbol 不是普通期货合约，并返回 `UnsupportedSymbolKind`
- 第一版不得把期权、套利、组合合约混入期货手续费历史 archive

规则字段：

- 买开保证金率
- 卖开保证金率
- 开仓手续费
- 平昨手续费
- 平今手续费
- 每手数量
- 最小跳动

审计字段：

- `source_updated_at`：源站“手续费更新时间”
- `observed_at`：daemon 实际观察时间
- `source_url`
- HTTP 状态码
- 解析后的允许字段行数
- 基于允许字段计算的 `probe_hash` 和 `rule_hash`
- daemon 版本

## 历史语义

系统维护两类时间：

- 源站时间：`source_updated_at`，来自源站字段。
- 观察时间：`observed_at`，daemon 抓取并成功解析的时间。

as-of 查询使用本系统维护的版本区间：

- `valid_from`
- `valid_to`

第一版 `valid_from` 取值规则：

- 当 CSV 行提供 `source_updated_at` 时，将其按中国交易所本地时间 `+08:00` 规范化后作为 `valid_from`。
- 当 CSV 行缺少 `source_updated_at` 时，回退为 daemon 本次成功解析的 `observed_at`。
- 同一 `symbol`、同一 `valid_from` 出现不同 `rule_hash` 时视为源数据冲突，本批次回滚。
- 同一 `symbol` 的相邻版本如果 `rule_hash` 相同，则合并为一个版本；`source_updated_at` 不参与 `rule_hash`，只保留为审计字段。

原因：实际源 CSV 会为同一合约返回多条带不同“手续费更新时间”的规则行，例如 `CZCE.AP705` 在 `2026-05-20 22:26:06` 和 `2026-06-05 22:49:34` 两个时间点的平今手续费不同。将这些行作为版本锚点，可以在首次全量加载时获得源站已经暴露的历史。由于源站仍不是完整历史 API，`history_start` 只表示当前 archive 中最早可用版本，不表示市场真实历史的绝对起点。

当用户查询早于 `history_start` 的时间时，client 返回 `NotAvailableBeforeHistoryStart`。

## 总体架构

```text
本地初始 seed
  -> future-meta-daemon seed-history
      -> 拉取九期网总表
      -> 发现全部单品种入口
      -> 下载每个品种 CSV
      -> 解析允许字段和源站已暴露版本
      -> 写入本地 SQLite 历史库
  -> future-meta-daemon export
      -> 导出 FeeArchiveV1
      -> bincode 编码
      -> zstd 压缩
      -> 生成 manifest.json
  -> gzip SQLite 为 ops/future-meta.sqlite.gz
  -> 部署 public/ 到 Cloudflare Pages

GitHub Actions schedule
  -> 下载 https://future-meta.pages.dev/ops/future-meta.sqlite.gz
  -> 解压为 data/future-meta.sqlite
  -> future-meta-daemon update-latest --require-seed
      -> 拉取九期网总表
      -> 解析 table#heyuetbl 最新截面
      -> 用 seed 中已有合约元数据补齐 latest 行
      -> 写入新手续费版本
  -> future-meta-daemon export
  -> 重新生成 ops/future-meta.sqlite.gz
  -> 部署 public/ 到 Cloudflare Pages

future-meta client
  -> 拉取 manifest.json
  -> manifest 未变：使用本地缓存
  -> manifest 变化：下载 .fmeta.zst
  -> 校验 sha256 和 schema_version
  -> 构建内存索引
  -> 本地执行 as-of 查询
```

## crate 边界

### future-meta

职责：

- 定义共享数据模型。
- 定义二进制格式版本。
- 解压、解码、校验 `.fmeta.zst`。
- 构建内存索引。
- 提供高性能查询 API。
- 在启用 `download` feature 时支持 manifest 下载、artifact 下载和本地缓存。

不负责：

- 抓取网页。
- 解析 HTML 或 CSV。
- 维护 SQLite 历史库。
- 发布 Cloudflare Pages。

### future-meta-daemon

职责：

- 抓取总表和 CSV。
- 解析并归一化源站数据。
- 严格丢弃所有禁止持久化字段。
- 维护本地历史库。
- 执行单品种入口级增量探测。
- 导出 client 二进制产物。
- 生成 Cloudflare Pages 可发布目录。

不负责：

- 提供运行时在线查询 API。
- 在 Cloudflare Worker 内常驻执行。
- 将派生字段写入任何持久化介质。

## daemon 本地历史库

daemon 使用 SQLite 作为本地历史库。SQLite 文件只存在于 GitHub Actions workspace、开发机或未来自托管环境中，不发布给 client。

建议表：

```text
contracts(
  id,
  symbol,
  listing_date,
  expiry_date,
  lot_size,
  tick_size,
  first_seen_at,
  last_seen_at,
  active
)

source_state(
  source_url,
  last_probe_hash,
  last_rule_set_hash,
  last_success_at,
  last_error_at,
  last_error_message
)

fetch_runs(
  id,
  started_at,
  finished_at,
  source_url,
  http_status,
  parsed_allowed_rows,
  daemon_version,
  result
)

fee_versions(
  id,
  contract_id,
  rule_hash,
  buy_margin_rate,
  sell_margin_rate,
  open_fee_kind,
  open_fee_value,
  open_fee_raw_text,
  close_yesterday_fee_kind,
  close_yesterday_fee_value,
  close_yesterday_fee_raw_text,
  close_today_fee_kind,
  close_today_fee_value,
  close_today_fee_raw_text,
  trading_status,
  is_main_contract,
  source_updated_at,
  valid_from,
  valid_to,
  first_seen_at,
  last_seen_at
)

artifact_builds(
  id,
  schema_version,
  data_version,
  generated_at,
  history_start,
  history_end,
  artifact_name,
  sha256,
  byte_size,
  rule_set_hash
)
```

`fetch_runs` 不存原始 CSV，也不存完整响应 hash。`last_probe_hash` 和 `rule_hash` 只基于允许字段计算。

`source_state.source_url` 用于 daemon 追踪上游采集入口。历史 seed 使用单品种 CSV 入口，例如 `https://www.9qihuo.com/shouxufeixz?heyue=cu`；latest 更新使用总表页 `https://www.9qihuo.com/qihuoshouxufei`，probe key 为 `table#heyuetbl`。这些都是采集调度键，不进入 client archive 的合约标识模型。

## 手续费字段归一化

手续费保存为结构化枚举：

```text
FeeSpec {
  kind,
  value,
  raw_text
}
```

`kind` 第一版支持：

- `CnyPerLot`：每手固定金额，例如 `2元`
- `TurnoverRatePerTenThousand`：成交额万分比，例如 `0.51/万分之`
- `Zero`
- `Unknown`

解析失败时：

- 不保存原始整行。
- 可以保存原始手续费单元格文本到 `raw_text`，仅限该单元格属于规则字段。
- `kind = Unknown`，`value = null`。
- daemon 记录 warning 并继续处理其他行。

## 增量更新策略

上游不提供真正的 delta API，因此第一版区分两种更新模式：

### 本地历史 seed

本地命令 `seed-history` 负责初始化和低频重建历史库：

1. 抓取总表页。
2. 从总表解析所有单品种入口 URL。
3. 对每个品种下载 `https://www.9qihuo.com/shouxufeixz?heyue=<code>`。
4. CSV 解析后只保留允许字段。
5. 按允许字段计算每行 `rule_hash`。
6. 同一合约的源站历史行进入 `fee_versions`。
7. 导出 client artifact。
8. 将 SQLite gzip 为 `public/ops/future-meta.sqlite.gz` 并发布到 Cloudflare Pages。

本地 seed 可以手动切换代理/IP，应对源站限频或 503。GitHub Actions 不做这个全量历史抓取。

### CI latest 更新

GitHub Actions 只负责从 Cloudflare seed 延续更新：

1. 从 `https://future-meta.pages.dev/ops/future-meta.sqlite.gz` 下载 SQLite seed。
2. 解压后要求 `contracts` 和 `fee_versions` 非空，否则中止。
3. 抓取总表页 `https://www.9qihuo.com/qihuoshouxufei`。
4. 解析 `table#heyuetbl` 最新截面。
5. 费用单元格只取首个非空文本节点，丢弃括号中的每手金额等派生展示值。
6. 不是普通期货合约的代码跳过，例如源站月均价类 `l2607F`。
7. latest HTML 不提供上市日、到期日、每手数量、最小跳动时，只从 seed 中已有 `contracts` 元数据补齐。
8. seed 不认识且页面也没有直接规格字段的新普通期货 `symbol` 跳过；下次本地 seed 后再纳入。
9. 对补齐后的允许字段集合计算 `rule_set_hash`。
10. `rule_set_hash` 未变化则只更新 `source_state`。
11. `rule_set_hash` 变化则写入新手续费版本，导出 artifact，并替换 Cloudflare 上的 SQLite seed。

失败策略：

- seed 缺失或为空时不更新、不发布。
- latest 表格缺失、解析为空、或所有普通期货行都无法用 seed 补齐时不更新、不发布。
- 解析失败写入 `source_state.last_error_*`，供 GitHub Actions 日志和后续告警查看。

## 发布二进制格式

第一版使用：

- 编码：`bincode 2`
- 压缩：`zstd`
- 文件后缀：`.fmeta.zst`

原因：

- 数据规模小。
- 实现简单。
- 跨平台稳定。
- 不需要第一版承担 zero-copy 或 mmap 的复杂度。

发布结构：

```text
public/
  manifest.json
  latest.fmeta.zst
  artifacts/
    future-meta-fees-v1-20260604T120000.fmeta.zst
```

`FeeArchiveV1`：

```text
FeeArchiveV1 {
  schema_version,
  generated_at,
  history_start,
  history_end,
  contracts,
  fee_versions
}
```

`manifest.json`：

```json
{
  "schema_version": 1,
  "data_version": "2026-06-04T12:00:00+08:00",
  "generated_at": "2026-06-04T12:00:00+08:00",
  "history_start": "2026-06-04T12:00:00+08:00",
  "history_end": "2026-06-04T12:00:00+08:00",
  "artifact": "latest.fmeta.zst",
  "sha256": "hex",
  "size": 123456,
  "mirrors": [
    "https://future-meta.pages.dev/latest.fmeta.zst"
  ]
}
```

如果配置了 GitHub Release 镜像，则 `mirrors` 增加 GitHub URL。

## client API 设计

核心 API：

```rust
let meta = FutureMeta::load_file("latest.fmeta.zst")?;
let fee = meta.contract_fee_asof("SHFE.cu2607", at)?;
let contracts = meta.underlying_fees_asof("SHFE.cu", at)?;
let main = meta.main_contract_fee_asof("KQ.m@SHFE.cu", at)?;
```

启用下载 feature：

```rust
let meta = FutureMeta::load_or_fetch(DownloadConfig::default()).await?;
```

主要类型：

```text
FutureMeta
DownloadConfig
FeeArchiveV1
FeeRule
FeeSpec
ContractFee
AsOfError
```

查询错误：

- `UnknownUnderlyingSymbol`
- `UnknownContract`
- `UnsupportedSymbolKind`
- `NotAvailableBeforeHistoryStart`
- `NoVersionAt`
- `CorruptArchive`
- `UnsupportedSchemaVersion`
- `ChecksumMismatch`
- `DownloadFailed`

内存索引：

```text
symbol -> contract_id
underlying_symbol -> Vec<contract_id>
contract_id -> sorted Vec<fee_version_id>
main_contract_by_underlying_symbol -> sorted versions or filtered query
```

单合约 as-of 查询对版本数组二分查找。品种查询先从传入的 `underlying_symbol` 查合约集合，再对每个合约做 as-of 查询并按交易状态、上市日期、到期日期过滤。主连查询接收 `KQ.m@...` symbol，解析出 `underlying_symbol` 后返回该时间点的主力合约手续费版本。

## Cloudflare 免费部署方案

第一版使用 Cloudflare Pages 免费层作为静态文件分发，不使用 Worker 作为主链路，不使用 R2 作为主存储。

部署流：

```text
本地 seed
  -> future-meta-daemon seed-history --db data/future-meta.sqlite --force-full
  -> future-meta-daemon export --db data/future-meta.sqlite --out public/
  -> gzip -c data/future-meta.sqlite > public/ops/future-meta.sqlite.gz
  -> 部署 public/ 到 Cloudflare Pages

GitHub Actions schedule
  -> 下载 Cloudflare 上的 ops/future-meta.sqlite.gz
  -> future-meta-daemon update-latest --db data/future-meta.sqlite --require-seed
  -> future-meta-daemon export --db data/future-meta.sqlite --out public/
  -> gzip 更新后的 SQLite seed
  -> 部署 public/ 到 Cloudflare Pages
```

免费层约束对应策略：

- Pages 单文件大小限制：artifact 必须保持在限制内；第一版去掉派生字段后预计远小于限制。
- Pages 每月构建次数有限：只有 `rule_set_hash` 变化时才部署，定时 refresh 不等于每次部署。
- Pages 文件数量有限：只保留 `latest.fmeta.zst`、最近 N 个 artifacts 和 `ops/future-meta.sqlite.gz`。
- Worker 免费层 CPU 和请求限制：第一版不把查询或 daemon 放到 Worker。
- R2 免费额度适合后续扩展：当 artifact 超过 Pages 静态限制或需要长期归档时再迁移。

保留 GitHub Release 作为可选镜像：

- release tag 可使用 `data-latest` 或按日期发布。
- client manifest 支持 `mirrors`，Cloudflare Pages 失败时尝试 GitHub。

## GitHub Actions 设计

建议 workflow：

```text
name: update-fee-data

on:
  schedule:
    - cron: "45 18 * * *"
  workflow_dispatch:

jobs:
  update:
    steps:
      - checkout
      - setup rust
      - cargo test --workspace
      - curl https://future-meta.pages.dev/ops/future-meta.sqlite.gz
      - gzip -dc /tmp/future-meta.sqlite.gz > data/future-meta.sqlite
      - cargo run -p future-meta-daemon -- inspect --db data/future-meta.sqlite
      - cargo run -p future-meta-daemon -- update-latest --db data/future-meta.sqlite --require-seed
      - cargo run -p future-meta-daemon -- export --db data/future-meta.sqlite --out public
      - gzip -c data/future-meta.sqlite > public/ops/future-meta.sqlite.gz
      - deploy public/ to Cloudflare Pages
```

SQLite 持久化选择：

- 第一版用 Cloudflare Pages 上的 `ops/future-meta.sqlite.gz` 保存 daemon SQLite seed。
- 每次成功发布后，新的 SQLite seed 随 `public/` 一起替换。
- 仓库不提交 SQLite 文件。
- 如果 Cloudflare seed 丢失或损坏，GitHub Actions 必须失败，不允许从空库覆盖生产；需要本地重新执行 `seed-history` 并发布 seed。

为了降低 seed 丢失风险，后续可增加 GitHub Release 私有运维 artifact 或 Cloudflare R2 备份，但第一版不依赖它。

## 命令设计

daemon 命令：

```text
future-meta-daemon discover --out sources.json
future-meta-daemon seed-history --db data/future-meta.sqlite --force-full
future-meta-daemon update-latest --db data/future-meta.sqlite --require-seed
future-meta-daemon refresh --db data/future-meta.sqlite
future-meta-daemon refresh --db data/future-meta.sqlite --force-full
future-meta-daemon export --db data/future-meta.sqlite --out public
future-meta-daemon inspect --db data/future-meta.sqlite
```

client/library 不暴露命令行作为第一版要求，但可以在 examples 中提供加载和查询示例。

## 测试计划

单元测试：

- 手续费字符串解析。
- 保证金率解析。
- 天勤风格 `symbol` 解析和 `underlying_symbol` 派生。
- `KQ.m@...` 主连 symbol 解析。
- `KQ.i@...`、期权、套利 symbol 返回 `UnsupportedSymbolKind`。
- 允许字段 hash 稳定性。
- 禁止字段变化不影响 `rule_hash`。
- as-of 二分查询边界。
- schema version 不兼容报错。

fixture 测试：

- `cu` CSV 样本。
- `ag` CSV 样本。
- `IF` CSV 样本。
- 不存在品种只返回表头的 CSV 样本。
- 派生字段变化但规则字段不变的 CSV 样本。
- 规则字段变化的 CSV 样本。
- 总页 `table#heyuetbl` HTML 样本。
- latest 手续费单元格包含 `<nobr class="js_single_fee">` 派生金额的样本。

集成测试：

- fixture CSV -> daemon SQLite -> export -> client load -> as-of query。
- latest HTML -> seed 元数据补齐 -> daemon SQLite -> export -> client load。
- seed 缺失时 `update-latest --require-seed` 失败。
- manifest sha256 校验失败。
- manifest 未变时 client 使用缓存。
- manifest 变化时 client 下载新 artifact。

部署验证：

- GitHub Actions 手动触发成功。
- Cloudflare Pages 上 `manifest.json` 可访问。
- `latest.fmeta.zst` sha256 与 manifest 一致。
- client 从 Pages 下载并完成查询。

## 风险和缓解

源站 HTML/CSV 字段变化：

- parser 对表头做严格校验。
- 字段缺失时本轮不发布新 artifact。
- 保留 fixture 回归测试。

源站临时失败：

- 单品种入口重试。
- 本轮失败不覆盖旧 artifact。
- client 保留本地缓存。

Cloudflare seed 丢失：

- 旧 artifact 继续服务 client。
- GitHub Actions 必须失败，不从空库发布。
- 本地重新执行 `seed-history` 后发布新的 `ops/future-meta.sqlite.gz`。
- 后续可增加 R2 或 GitHub Release 运维备份。

Cloudflare Pages 限制：

- artifact 去掉派生字段。
- 只保留有限历史 artifacts。
- 超限后迁移 artifact 存储到 R2，Pages 继续服务 manifest 或入口。

历史语义误用：

- client 查询早于 `history_start` 明确报错。
- 文档说明 `source_updated_at` 优先、`observed_at` 兜底的版本边界。
- 不使用当前值冒充过去历史。

## 实施里程碑

1. 在 `future-meta` 定义核心数据结构、错误类型、archive 编解码。
2. 添加 as-of 查询索引和 fixture 单元测试。
3. 在 `future-meta-daemon` 实现 CSV 解析和允许字段归一化。
4. 实现禁止字段不影响 `rule_hash` 的测试。
5. 引入 SQLite 历史库和版本维护逻辑。
6. 实现总表单品种入口发现和入口级 probe 增量。
7. 实现 artifact export 和 manifest 生成。
8. 实现 client 下载、sha256 校验和缓存。
9. 添加 GitHub Actions workflow。
10. 添加 Cloudflare Pages 发布配置。
11. 做一次端到端 dry run。

## 验收标准

- daemon 不持久化任何禁止字段。
- 派生字段变化不会触发新 `fee_versions`。
- 规则字段变化会产生新的 as-of 版本。
- client 可从本地 `.fmeta.zst` 加载并查询。
- client 可通过 manifest 自动下载和缓存。
- Cloudflare Pages 免费方案可分发 `manifest.json` 和 `latest.fmeta.zst`。
- 源站失败时不会覆盖最后一个可用 artifact。
- 早于 `history_start` 的查询返回明确错误。
