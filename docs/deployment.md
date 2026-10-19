# future-meta Deployment

使用 GitHub Actions 做定时更新，Cloudflare Pages 免费层做静态分发和 daemon seed 托管。

## 当前 Cloudflare 部署

- Project name: `future-meta`
- Production URL: `https://future-meta.pages.dev`
- Manifest URL: `https://future-meta.pages.dev/manifest.json`
- Daemon seed URL: `https://future-meta.pages.dev/ops/future-meta.sqlite.gz`

## 发布内容

`public/` 会被部署到 Cloudflare Pages：

- `manifest.json`
- `latest.fmeta.zst`
- `artifacts/*.fmeta.zst`
- `ops/future-meta.sqlite.gz`

`ops/future-meta.sqlite.gz` 是 daemon 的 SQLite 历史 seed，只供 GitHub Actions 后续更新使用；client 不下载它。

## 历史官方证据暂存

历史手续费补充只接受交易所原始公告、收费表和结算/业务参数文件。材料必须人工获取、保留原始字节并计算 SHA-256；候选记录只能进入独立证据库，不能导出或部署。

```bash
cargo run -p future-meta-daemon -- stage-official \
  --db data/official-evidence.sqlite \
  --input path/to/adjustments.json
```

已复核且配对完整的材料只能先物化到审阅副本：

```bash
cargo run -p future-meta-daemon -- import-official-history \
  --db path/to/review-copy.sqlite \
  --evidence-db data/official-evidence.sqlite \
  --exchange CFFEX \
  --snapshot-dir data/official-evidence \
  --from 2020-01-01 \
  --through 2026-08-24
```

命令逐份验证保留文件 SHA-256。部分调整必须能从同一具体合约此前的完整官方版本
补齐；不得从品种众数、第三方基线或后继版本反向推断。

生命周期与规格证据单独导入：

```bash
cargo run -p future-meta-daemon -- import-official-metadata \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/official-metadata.tsv \
  --snapshot-dir data/official-evidence
```

TSV 必须包含 `symbol`、`listing_date`、`expiry_date`、`valid_from`、`valid_to`、
`lot_size`、`tick_size`、`lifecycle_url`、`lifecycle_sha256`、
`specification_url`、`specification_sha256`。生命周期与规格引用分开核验。

CFFEX 使用官方产品规则和交易日历 XML 导入生命周期与规格：

```bash
cargo run -p future-meta-daemon -- import-cffex-metadata \
  --db path/to/review-copy.sqlite \
  --product-manifest data/official-evidence/cffex-product-metadata.tsv \
  --calendar-manifest data/official-evidence/cffex-calendar-2019-2026.tsv \
  --snapshot-dir data/official-evidence
```

官方证据库与 `data/future-meta.sqlite` 完全隔离，且不属于 Pages 发布物。满足完整、连续、人工复核条件前，禁止把候选材料写入生产历史。具体标准见 [官方历史证据流程](official-evidence.md)。

完整交易所参数表可作为较低等级 `official_parameter` 证据，无需配对公告，但必须保留
原始字节、核对官方 URL 与 SHA-256，并明确三腿手续费及单位。CZCE 审阅副本导入：

```bash
cargo run -p future-meta-daemon -- import-czce-parameters \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/czce-daily-params-20200101-20260818.tsv \
  --snapshot-dir data/official-evidence \
  --from 2020-01-01
```

`fee_version_evidence.evidence_level` 区分 `paired_official` 与
`official_parameter`。前者优先级更高，参数导入不得覆盖同一时点的前者。手续费参数
文件不自动提升合约规格证据等级。

GFEX 每日结算参数使用独立离线导入器：

```bash
cargo run -p future-meta-daemon -- import-gfex-parameters \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/gfex-daily-settlement-20221222-20260818.tsv \
  --snapshot-dir data/official-evidence \
  --from 2022-12-22
```

`openFee`、`offsetFee`、`shortOffsetFee` 依次对应开仓、平昨、平今；`绝对值` 按元/手
导入，`比例值` 按成交额万分比导入。只有 manifest 中状态为 `ok` 且通过 URL、SHA-256、
字段和单位校验的快照会参与历史物化；`official_empty` 与 `pending` 仍是覆盖缺口。

INE 日参数只含一般手续费，必须与独立复核的官方平今规则配对：

```bash
cargo run -p future-meta-daemon -- import-ine-parameters \
  --db path/to/review-copy.sqlite \
  --manifest data/official-evidence/ine-dailydata-20200101-20260818.tsv \
  --close-today-rules data/official-evidence/ine-close-today-rules.tsv \
  --snapshot-dir data/official-evidence \
  --from 2020-01-01
```

平今规则 TSV 列为 `scope`、`valid_from`、`valid_to`、`close_today_kind`、
`close_today_value`、`canonical_url`、`sha256`。`scope` 仅接受 `INE.<品种>` 或
`INE.<具体合约>`。导入器要求每个参数观察命中唯一有效规则，并将两份官方来源记录为
`paired_official`；不得把一般手续费默认复制为平今。

经人工复核的官方证据只能先应用到审阅副本：

```bash
cargo run -p future-meta-daemon -- apply-verified-official \
  --db path/to/review-copy.sqlite \
  --evidence-db data/official-evidence.sqlite
```

该命令不在定时 workflow 中。必须先完成[证据门控更新验证](evidence-gated-update-validation.md)规定的二十四个月回放与十四天影子运行；在此之前不得对生产 SQLite、发布产物或 Cloudflare 部署执行该操作。

审阅副本导出前必须通过严格覆盖审计：

```bash
cargo run -p future-meta-daemon -- audit-coverage \
  --db path/to/review-copy.sqlite \
  --from 2020-01-01 \
  --through 2026-08-24 \
  --out /tmp/future-meta-coverage.json \
  --strict
```

审计检查范围内合约的官方上市/到期边界、三腿手续费、合约乘数、最小变动价位、
版本连续性及 `official` 溯源。失败时保留完整 JSON 缺口清单，不得宣称请求期间严格完整。采集重建库按下文观测覆盖发布，并公开未知区间。

## GitHub 定时更新

定时任务每天北京时间 18:45 运行，对应 GitHub Actions cron `45 10 * * *`。

GitHub Actions 不从零构建历史库。每次运行：

生产 seed 已迁移为交易所采集输出全新重建库。`rebuild_summary.source` 必须为
`fresh_collector_outputs_no_seed`；工作流不再恢复或迁移到 V18 旧库。

1. 下载并解压 `ops/future-meta.sqlite.gz`。
2. 运行 `verify-collected`，验证采集来源标记、逐日观察关联、保证金、官方 URL、版本证据、SQLite 完整性和外键，并生成 `public/coverage.json`。
3. 运行 `scan-announcements` 发现候选，周五额外扫描华泰。
4. `update-latest --require-seed` 只读诊断。无差异时导出当前官方库；需审阅的候选冻结现有 artifact。抓取或解析错误仍使工作流失败。
5. 只有通过检查的 seed 可重新压缩并部署。任何失败都不回退旧 SQLite。

`coverage.json` 公布观测起止日、上市日至首条观测的未知区间、无日记录合约数和推算到期日数。
逐条来源一致不表示每个合约自上市起完整覆盖；未知区间保持无值。2026-10-18 发布覆盖
2020-01-02 至 2026-10-15，16 个合约存在观测前空档，147 个合约没有日参数记录，
426 个到期日使用用户授权的日历推断。不能宣称这些日期均为交易所已确认。

最新截面来自 9qihuo 总页 HTML 的 `table#heyuetbl`。页面上的 Excel 按钮是 `tableToExcel('heyuetbl', ...)` 生成，不存在稳定的 `heyue=all` CSV 下载端点，因此 daemon 直接解析 HTML 表格。此日更路径只诊断候选，不为历史回填或发布提供证据。

Jin10 HTTP 成功且响应结构正常、但请求窗口没有快照时，按零份佐证继续候选诊断。
变化候选仍必须有同日匹配或官方证据，否则延期并冻结客户端档案；无变化时不需要外部佐证。
不会扩大近期窗口或借用旧快照。非正常状态、网络错误、解析错误，以及有快照却无可用行仍失败。
日志明确记录请求日期及无快照状态。

Jin10 只作为 9qihuo 最新截面候选的同日交叉确认，不会独立写入生产费率版本，也不能作为 9qihuo 失败时的自动替代源。固定值/成交金额比例切换、平昨/平今字段置换、零费率切换、单腿超过两倍的跳变、超过 12 条的同批变更，以及疑似 `0.1 元`占位或统一 `+0.01/+0.09/+0.1` 固定费偏移都会在交叉核验阶段明确标记风险；但即使普通候选双源一致，也必须进入交易所官方证据与人工审阅流程。实时路径不会以产品众数或固定费偏移规则覆写已有费率。

新上市 symbol 也需要完整官方手续费、生命周期和规格证据，随后仅在审阅副本创建前向生效的具体合约记录。Jin10 覆盖只能辅助核对，不能代替官方证据。

archive schema v2 导出 `contract_spec_versions`，用于查询历史乘数和最小变动价位。可在审阅副本显式运行已审阅的交易所规格迁移：

```bash
cargo run -p future-meta-daemon -- migrate-contract-specs \
  --db path/to/review-copy.sqlite
```

当前内置迁移覆盖 DCE 棕榈油/豆油、GFEX 碳酸锂和 INE 集运欧线的官方调整，并逐合约保留有效期和公告 URL。schema v2 客户端继续兼容读取 schema v1 artifact。

## 人工审阅发布

发布只能从隔离审阅副本开始，不能原地修复线上 seed。先完成交易所原文、附件、SHA-256、
完整三腿手续费与生效日的人工复核，并通过 `import-official-history` 或交易所参数导入
物化历史。旧库若出现历史哈希断链或 `first_seen_at > last_seen_at`，必须先提供逐条时间
修正 JSON，明确确认审阅副本后执行：

```bash
cargo run -p future-meta-daemon -- repair-review-history \
  --db path/to/review-copy.sqlite \
  --time-repairs path/to/observation-time-repairs.json \
  --confirm-review-copy
```

该命令重算仅由三腿费率的哈希、重接保留 evidence，并在
`review_fee_history_repairs` 记录每项修复；不会删除 evidence，也不会接受未列入 JSON 的
时间修正。它不是日常任务，也不替代官方证据复核。

当前采集重建库按原件观测范围发布，不沿用旧库的 2010–2019 历史或严格完整声明。
发布前验证采集关联并公开覆盖报告：

```bash
cargo run -p future-meta-daemon -- verify-collected \
  --db path/to/review-copy.sqlite --out public/coverage.json
cargo run -p future-meta-daemon -- export \
  --db path/to/review-copy.sqlite --out public
gzip -c path/to/review-copy.sqlite > public/ops/future-meta.sqlite.gz
```

schema 3 需要 `future-meta` 0.4.0 客户端；0.4.0 兼容读取旧 schema 1/2。升级说明见
[官方重建说明](official-rebuild.md)。

`export` 以只读方式打开数据库，并拒绝孤儿 fee evidence、倒置观察时间、非法或重叠
区间、以及早于合约上市日的费率版本。只有这些检查通过后，才可在取得发布授权后部署
`public/`。

## 维护库的无损空间整理

维护库对证据文档和重建溯源 JSON 去重，保留全部历史、日观测和官方来源。
使用当前 daemon 显式生成迁移副本：

```bash
cargo run -p future-meta-daemon -- compact-seed \
  --db path/to/downloaded-seed.sqlite --out path/to/compacted-seed.sqlite
```

输入只读，输出必须不存在。命令在临时副本中共享重复的溯源 JSON 属性值，
将三类证据的 URL、SHA 保存为共享文档记录。原表名保留为兼容视图，JSON 读取结果
逐字节保留；证据视图支持插入、更新和删除，实际记录表使用 `WITHOUT ROWID`。
命令双向核对记录内容和数量，验证历史、外键及 SQLite 完整性，再生成目标文件；
重复执行支持已精简的库。日常恢复 seed 不自动迁移或反复整理空间。

维护精简库需要包含证据视图写入兼容改动的 daemon；SQLite UPSERT 不能直接以视图
为目标，维护代码已使用事务内兼容写法。客户端 archive 格式和下载接口不变。

发布前还需与下载的生产库核对所有表及查询结果，并比较 gzip 后的体积。
仅改变物理布局时，沿用线上客户端 manifest 与 artifact，更新 `ops/future-meta.sqlite.gz`。

## Required GitHub Secrets

- `CLOUDFLARE_API_TOKEN`: token allowed to deploy Cloudflare Pages.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account id.

## 数据安全边界

- 不提交 `data/future-meta.sqlite`。
- 不提交原始证据文件或 `data/official-evidence.sqlite`。
- 不提交 `public/` 生成物。
- 不存储或发布价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费等派生字段。
- latest HTML 中不是普通期货合约的代码会跳过，例如源站的月均价类 `l2607F`。
- latest HTML 不提供上市日、到期日、每手数量、最小跳动时，daemon 优先从 seed 补齐；seed 不认识的新 symbol 必须通过上述受控准入，否则整个发布失败，绝不静默跳过。

## Client URL

Default manifest URL:

`https://future-meta.pages.dev/manifest.json`
