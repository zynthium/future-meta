# 交易所官方数据采集

两个脚本独立生成官方结算参数、合约元数据和规格历史，供后续从零构建数据库使用。采集链路不读取 `future-meta.sqlite`，也不使用第三方手续费数据补值。

## 来源与输出

- `collect_settle_params.py`：六所手续费和保证金；输出各所逐日 CSV、CFFEX 变更快照及可选逐日展开表。
- `build_lifecycles.py`：根据交易所官方日历与法定交割细则，生成 CFFEX、GFEX、DCE、CZCE 的全量官方合约生命周期清单（`*-lifecycle-*.tsv`）。
- `collect_contract_specs.py`：交易所合约基础信息、产品规格和真实变更依据；输出 `contracts_meta.csv`、`contract_spec_versions.csv`、来源映射和缺口报告。
- `raw/` 与 `data/official-evidence/`：保留交易所响应原文、官方 URL、请求日期和 SHA-256。可以复用经校验的官方原件，不用旧 SQLite 或未经证实的 CSV 补值。

普通期货使用 TqSdk 风格 symbol。期权和 DCE 月均价 F 合约不属于当前普通期货重建范围。

## 结算参数采集

在线采集示例：

```bash
python3 scripts/collect_settle_params.py   --start-date 2020-01-01 --end-date 2026-10-15   --output-dir /tmp/future-meta-rebuild/settle_params   --raw-dir /tmp/future-meta-rebuild/raw   --evidence-dir data/official-evidence   --overwrite --futures-only --forward-fill-cffex
```

`--offline` 只读取校验通过的官方原始缓存；缺少所需原件时失败，不回退现有 CSV。所有模式均从官方原件构建本次指定日期范围，不读取旧 CSV 保留其他日期。输出目录不能遗留本次没有请求生成的参数 CSV（包括未启用逐日展开时的旧 CFFEX 日表）；这种情况会中止并要求使用空目录或请求对应输出。目标文件已存在时必须提供 `--overwrite`；完整采集成功后才替换目标文件，本轮源数据不存在的旧行不会留下。

交易日从官方日历来源确定，不能把已有 SHFE CSV 的日期集合当作完整交易日历；HTTP 错误不能当作休市证明。CFFEX 只在变更时发布完整参数快照，目录抓取成功且原始快照完整后才能逐日展开。目录或分页请求失败必须中止，不能用旧快照掩盖采集失败。

### CSV 字段与单位

|字段|含义|
|---|---|
|`date`|参数对应的交易日|
|`exchange`, `symbol`|实际交易所和本地合约代码|
|`long_margin_ratio`, `short_margin_ratio`|投机多空保证金率，`10.0` 表示 10%|
|`hedge_long_margin_ratio`, `hedge_short_margin_ratio`|套保多空保证金率，单位同上|
|`open_fee`, `offset_fee`, `close_today_fee`|普通开仓、平昨、平今手续费|
|`intraday_open_fee`|日内开仓手续费；不能与平今字段混用|
|`fee_type`|`cny_per_lot` 为元/手，`rate_per_10k` 为成交额万分比|
|`settle_price`, `delivery_fee`|原始官方参数的辅助字段，不直接作为客户端 archive 发布字段|

缺失值和数值零必须区分。现有 Rust 导入器是否支持所有字段，需要在实际重建入口中另外落实，不能因 CSV 已采到值就声称 archive 已完整表达。

### 各所语义

- **SHFE/INE**：共同手续费源为 `https://www.shfe.com.cn/data/busiparamdata/future/SettlementYYYYMMDD.dat`。`TRADEFEEUNIT` 是元/手；`TRADEFEERATION` 为成交额绝对比例，乘 10000 为万分比；平今为普通费乘 `DISCOUNTRATE`，保留 0 和大于 1 的乘数。`HTRADE*` / `TTRADE*` 为套保费用，不能作为投机平今费用。按实际交易所分开 `sc/nr/lu/bc/ec`，不能照抄源内统一的 SHFE 标记。
- **INE**：保证金和结算价使用能源中心独立日表，核对响应日期。部分 EC 到期日交易保证金与共同结算表口径不同，不能用共同表的零值覆盖独立交易参数。
- **DCE/GFEX**：`openFee` 为普通开仓，`offsetFee` 为普通平仓，`shortOpenFee` 为日内开仓，`shortOffsetFee` 为平今。套保费不替代投机费。
- **CZCE**：旧表没有收取方式列时按官方旧表的绝对额语义解析；新表按“绝对值/比例值”识别，未知类型失败。
- **CFFEX**：按完整快照进行 forward-fill，不将无变更日期判为缺日。快照必须包含目标区间起点所需的最近一次基准；不能用区间内首次快照向过去回填。

## 合约与规格采集

```bash
python3 scripts/collect_contract_specs.py   --offline   --evidence-dir data/official-evidence   --settle-dir /tmp/future-meta-rebuild/settle_params   --out /tmp/future-meta-rebuild/contracts_meta.csv   --versions-out /tmp/future-meta-rebuild/contract_spec_versions.csv
```

脚本不接受 `--db`。默认只处理留存官方原件；显式 `--cbi-date YYYY-MM-DD` 才联网抓取指定日期的合约基础信息，不能与 `--offline` 同用。合约生命周期来自官方 ContractBaseInfo 或官方生命周期清单；产品规格来自带原件摘要的官方规格清单。

- 所有缓存原件校验 SHA-256 和官方域名；同一合约按源报告日期选择最新记录，网络请求顺序不能覆盖较新的缓存。
- 规格清单可用 `delivery_month_from` / `delivery_month_to` 指定包含端点的 `YYYY-MM` 交割月份范围，独立于 `valid_from` / `valid_to` 的生效时间。日期与月份范围必须同时匹配；重叠冲突也同时检查两种范围。范围会写入来源账本。该范围用于四位 YYMM 合约；郑商所三位代码应使用 `full_symbol` 清单明确适用合约。
- 例如大商所发〔2019〕437号的新纤维板规格仅适用 FB2001 及以后合约：新规格填写 `delivery_month_from=2020-01`；FB1912 及更早合约的旧规格填写 `delivery_month_to=2019-12` 并覆盖至其到期，不能在新规格启用日一起切换。
- `listing_date` / `expiry_date` 只写官方明确日期。`first_observed_date` / `last_observed_date` 为参数观测范围，不能冒充挂牌或到期日期。
- 经用户明确授权，可执行 `python3 scripts/build_lifecycles.py --assume-2027-holidays`，按用户提供的2027假期区间推算尚未公布的到期日：1月1—3日、2月4—12日、4月3—5日、5月1—5日、6月9日、9月15日、10月1—7日休市，周末不交易。该开关默认关闭，不修改已明确的官方日期；适用规则仍引用交易所原件，假期假设另记为 `calendar_assumption`。
- 这些日期标记为 `user_assumed_holiday_calendar`，规格范围为 `assumed_lifecycle`；来源账本保留完整假设。报告中的 `assumed_expiry_symbols` 给出数量，`official_only=false` 明确表示含假设日期，不能把 `complete=true` 解释成全部日期均由交易所确认。官方日历公布后，应更新官方日历采集范围、停用该开关并重新生成，以实际官方安排替换推算结果。
- 未知规格不默认填 `10/1`，不使用硬编码规格表补齐历史，不把当前快照无限向历史回填。
- 规格区间使用原件确认的适用范围；“结算时起”等边界必须保留阶段语义，不编造零点生效时间。边界当天的日度观测会单列歧义，不把全天都套用新规则。
- 缺少官方规格、生命周期或确定生效依据时，保留合约并列出缺口，不能删除合约来制造完整率。默认返回非零状态；`--allow-incomplete` 仅用于保存明确标记为不完整的候选数据，不能作为完整重建的成功证明。

### 官方清单

默认发现官方原件目录内的 `*-product-spec-*.tsv`、`cffex-product-metadata.tsv`、`*-contract-base-info-*.tsv` 和 `*-lifecycle-*.tsv`。其他交易所可通过 `--spec-manifest` / `--lifecycle-manifest` 指定带官方原件的清单；`--spec-manifest` 可以重复指定，显式提供时替换默认清单；如需保留其他交易所，需同时传入它们；生命周期清单的显式参数同样替换默认发现结果。具体字段以脚本 `--help` 和缺口报告为准。

清单中的 `lot_size` 是与报价单位一致的金额乘数。例如官方交易单位为 5 吨、报价为元/500 千克时，乘数为 10；不能只填物理吨数。

规格文本与生效公告可以分别引用：`boundary_canonical_url` / `boundary_sha256` / `boundary_raw_path` 记录生效依据；从官方日历推导下一交易日时，用 `application_calendar_*` 保存日历证据。 `listing_*`、`publication_*`、`additional_*` 同样验证并保存上市公告、发布通知和补充规格原件。`coverage_basis` 说明历史版本如何覆盖目标观察窗口，不把窗口起点冒称规则最早生效日。

`valid_from_trade_date` / `valid_to_trade_date` 是有官方依据的日度交易日适用边界；实际生效时点仍保存在 `valid_from` / `valid_to` 和 phase 中。例如周五 21:00 的夜盘属于下一交易日，不能把两种日期轴混为一谈。输出同时保留两者，逐日覆盖按明确交易日核验。
若交易日映射后的原规则边界恰好等于合约区间上限，仍保留原规则的实际时间和 `settlement` / `instant` 阶段；只有真正按合约范围截短时才使用普通日期边界。

来源映射记录官方 URL、原件摘要和适用日期。`official` 标签不能替代原始证据。既有清单本身也需要核对原公告中的生效日期，不能把文档抓取日期当作规则生效日期。

## 从零重建的顺序

1. 在独立输出目录采集或从官方原件重新解析结算参数。
2. 使用这些参数的观测范围和官方合约/规格原件生成元数据与规格版本。
3. 检查来源映射和缺口报告；补齐真实官方来源，不调用旧 SQLite 补值。
4. 在新数据库中导入全部已确认字段，按手续费、日内开仓费和保证金共同判断历史版本变化。
5. 核对交易所原始参数与新库结果后，再决定替换现库。

`data/`、官方缓存与生成报告不提交到 Git。本脚本修复不自动替换数据库，也不发布 artifact。

完整新库导入入口为 daemon 的 `rebuild-collected`，会导入多空保证金与日内开仓费并按变化生成版本，见 [官方数据重建说明](../docs/official-rebuild.md)。旧的单所手续费审阅导入命令不等同于这一完整重建入口。
