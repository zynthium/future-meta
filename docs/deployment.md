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
版本连续性及 `official` 溯源。失败时仍会保留完整 JSON 缺口清单，但不得导出或部署。

## GitHub 定时更新

定时任务每天北京时间 18:45 运行，对应 GitHub Actions cron `45 10 * * *`。

GitHub Actions 不从零构建历史库。每次运行：

Cloudflare seed 的 `baseline_state.source_sha256` 必须匹配仓库内的
`assets/future-meta-v18-reviewed.sqlite.gz` 内记录的审阅基线指纹。V18 指纹为
`5690905cd18dcd9d5e32c9ffca9f8eb998978432dd67f0129dc5f9c7e0c7c242`；旧 seed
只在该指纹不匹配时迁移一次，匹配后继续沿用线上 seed，避免定时任务覆盖后续经官方证据批准的增量。V18 在 V17 及线上增量基础上修正了 6 个大商所合约的固定手续费偏移，并合并 5 条无实质变化的冗余版本。

1. 从 Cloudflare 下载 `ops/future-meta.sqlite.gz`。
2. 解压为 `data/future-meta.sqlite`。
3. 每日先执行 `future-meta-daemon scan-announcements`：中信期货为主发现源；中信无法完成有效扫描时同轮切换华泰期货；每周五额外执行华泰完整补漏扫描。扫描仅保存券商正文、哈希、交易所原文链接和手续费候选，绝不直接改变 `fee_versions`。
4. 执行 `future-meta-daemon update-latest --require-seed`。
5. 将 9qihuo 候选与 Jin10 交叉核验；候选无论是否一致都需要精确交易所原文/附件和人工审阅，第三方不会创建、覆盖或回写 `fee_versions`。
6. 固定写入 `publish=false`。不会导出 artifact、gzip seed 或部署 Pages。

`update-latest` 是只读诊断：无差异返回 `Noop`；候选、新具体合约或安全门控拒绝以
`Deferred` 原因非零退出。workflow 将该退出视为预期的“不发布”，绝不触发发布。生产
artifact 与 daemon seed 因此保持上一个审阅通过版本。

最新截面来自 9qihuo 总页 HTML 的 `table#heyuetbl`。页面上的 Excel 按钮是 `tableToExcel('heyuetbl', ...)` 生成，不存在稳定的 `heyue=all` CSV 下载端点，因此 daemon 直接解析 HTML 表格。此日更路径只诊断候选，不为历史回填或发布提供证据。

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

当前可声明的严格完整覆盖边界是 `2020-01-01`。2010–2019 数据可以留存，但在补齐
官方费率、生命周期和规格证据前不得宣称该期间完整。发布前依次完成：

```bash
cargo run -p future-meta-daemon -- audit-coverage \
  --db path/to/review-copy.sqlite \
  --from 2020-01-01 --through YYYY-MM-DD \
  --out /tmp/future-meta-coverage.json --strict

cargo run -p future-meta-daemon -- export \
  --db path/to/review-copy.sqlite --out public
gzip -c path/to/review-copy.sqlite > public/ops/future-meta.sqlite.gz
```

`export` 以只读方式打开数据库，并拒绝孤儿 fee evidence、倒置观察时间、非法或重叠
区间、以及早于合约上市日的费率版本。只有这些检查通过后，才可在取得发布授权后部署
`public/`。

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
