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

官方证据库与 `data/future-meta.sqlite` 完全隔离，且不属于 Pages 发布物。满足完整、连续、人工复核条件前，禁止把候选材料写入生产历史。具体标准见 [官方历史证据流程](official-evidence.md)。

经人工复核的官方证据只能先应用到审阅副本：

```bash
cargo run -p future-meta-daemon -- apply-verified-official \
  --db path/to/review-copy.sqlite \
  --evidence-db data/official-evidence.sqlite
```

该命令不在定时 workflow 中。必须先完成[证据门控更新验证](evidence-gated-update-validation.md)规定的二十四个月回放与十四天影子运行；在此之前不得对生产 SQLite、发布产物或 Cloudflare 部署执行该操作。

## GitHub 定时更新

定时任务每天北京时间 18:45 运行，对应 GitHub Actions cron `45 10 * * *`。

GitHub Actions 不从零构建历史库。每次运行：

Cloudflare seed 的 `baseline_state.source_sha256` 必须匹配仓库内的
`assets/future-meta-v18-reviewed.sqlite.gz` 内记录的审阅基线指纹。V18 指纹为
`84ffaa15468b8cbdc6203d25f0ecfe668c677193a7d42c32a243143be44d241e`；旧 seed
只在该指纹不匹配时迁移一次，匹配后继续沿用线上 seed，避免定时任务覆盖后续经官方证据批准的增量。V18 在 V17 及线上增量基础上修正了 6 个大商所合约的固定手续费偏移，并合并 5 条无实质变化的冗余版本。

1. 从 Cloudflare 下载 `ops/future-meta.sqlite.gz`。
2. 解压为 `data/future-meta.sqlite`。
3. 每日先执行 `future-meta-daemon scan-announcements`：中信期货为主发现源；中信无法完成有效扫描时同轮切换华泰期货；每周五额外执行华泰完整补漏扫描。扫描仅保存券商正文、哈希、交易所原文链接和手续费候选，绝不直接改变 `fee_versions`。
4. 执行 `future-meta-daemon update-latest --require-seed`。
5. 将 9qihuo 的实质费率候选与同日、同合约的 Jin10 快照交叉核验；无论是否一致，任何候选都必须先由精确的交易所原文/附件完成分阶段官方核验并写入，第三方只能用于交叉核对。未获确认、触发安全门控，或有超过 24 小时未决公告候选时，整个 workflow 失败并保留既有已发布产物。
6. 导出 `public/manifest.json` 和 `public/latest.fmeta.zst`。
7. 重新 gzip 更新后的 SQLite seed 到 `public/ops/future-meta.sqlite.gz`。
8. 部署整个 `public/` 到 Cloudflare Pages。

最新截面来自 9qihuo 总页 HTML 的 `table#heyuetbl`。页面上的 Excel 按钮是 `tableToExcel('heyuetbl', ...)` 生成，不存在稳定的 `heyue=all` CSV 下载端点，因此 daemon 直接解析 HTML 表格。此日更路径只维护最新截面，不为历史回填提供证据。

Jin10 只作为 9qihuo 最新截面候选的同日交叉确认，不会独立写入生产费率版本，也不能作为 9qihuo 失败时的自动替代源。固定值/成交金额比例切换、平昨/平今字段置换、零费率切换、单腿超过两倍的跳变、超过 12 条的同批变更，以及疑似 `0.1 元`占位或统一 `+0.01/+0.09/+0.1` 固定费偏移都会在交叉核验阶段明确标记风险；但即使普通候选双源一致，也必须进入交易所官方证据与人工审阅流程。实时路径不会以产品众数或固定费偏移规则覆写已有费率。

新上市 symbol 不属于手续费“变更”。它必须满足单品种 CSV 的乘数/跳价与总表每跳毛利恒等，并由 Jin10 核验静态规格。优先要求 Jin10 同日同合约手续费一致；Jin10 尚未列出远月时，只允许该费率完整匹配 V11 中已有同品种合约，同时 Jin10 同日同品种规格一致，并记录为 `contract_metadata_admissions.verification_level = 'degraded_product'`。该降级规则绝不适用于已有合约费率变化。

archive schema v2 导出 `contract_spec_versions`，用于查询历史乘数和最小变动价位。部署前可在生产副本显式运行已审阅的交易所规格迁移：

```bash
cargo run -p future-meta-daemon -- migrate-contract-specs \
  --db path/to/review-copy.sqlite
```

当前内置迁移覆盖 DCE 棕榈油/豆油、GFEX 碳酸锂和 INE 集运欧线的官方调整，并逐合约保留有效期和公告 URL。schema v2 客户端继续兼容读取 schema v1 artifact。

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
