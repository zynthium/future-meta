# future-meta Deployment

第一版使用 GitHub Actions 做定时更新，Cloudflare Pages 免费层做静态分发和 daemon seed 托管。

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

## GitHub 定时更新

定时任务每天北京时间 18:45 运行，对应 GitHub Actions cron `45 10 * * *`。

GitHub Actions 不从零构建历史库。每次运行：

1. 从 Cloudflare 下载 `ops/future-meta.sqlite.gz`。
2. 解压为 `data/future-meta.sqlite`。
3. 执行 `future-meta-daemon update-latest --require-seed`。
4. 使用 Jin10 对重叠合约做只读交叉核验；核验失败不阻断 9qihuo 产物发布，差异仅输出到 workflow 日志。
5. 导出 `public/manifest.json` 和 `public/latest.fmeta.zst`。
6. 重新 gzip 更新后的 SQLite seed 到 `public/ops/future-meta.sqlite.gz`。
7. 部署整个 `public/` 到 Cloudflare Pages。

最新截面来自 9qihuo 总页 HTML 的 `table#heyuetbl`。页面上的 Excel 按钮是 `tableToExcel('heyuetbl', ...)` 生成，不存在稳定的 `heyue=all` CSV 下载端点，因此 daemon 直接解析 HTML 表格。此日更路径只维护最新截面，不为历史回填提供证据。

Jin10 不参与生产费率版本写入，仅通过 `validate-jin10` 获取快照并与当前 SQLite 终态比较。Jin10 覆盖不完整，不能作为 9qihuo 失败时的自动替代源。

## Required GitHub Secrets

- `CLOUDFLARE_API_TOKEN`: token allowed to deploy Cloudflare Pages.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account id.

## 数据安全边界

- 不提交 `data/future-meta.sqlite`。
- 不提交原始证据文件或 `data/official-evidence.sqlite`。
- 不提交 `public/` 生成物。
- 不存储或发布价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费等派生字段。
- latest HTML 中不是普通期货合约的代码会跳过，例如源站的月均价类 `l2607F`。
- latest HTML 不提供上市日、到期日、每手数量、最小跳动时，daemon 只从 Cloudflare seed 中已有 contract 元数据补齐；seed 不认识的新 symbol 会跳过，直到下一次本地全量 seed。

## Client URL

Default manifest URL:

`https://future-meta.pages.dev/manifest.json`
