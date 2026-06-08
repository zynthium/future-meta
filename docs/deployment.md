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

## 本地全量 seed

初始历史获取在本地完成，便于手动切换代理/IP，应对源站 503 或限频。

```bash
cargo run -p future-meta-daemon -- seed-history --db data/future-meta.sqlite --force-full
cargo run -p future-meta-daemon -- export --db data/future-meta.sqlite --out public
mkdir -p public/ops
gzip -c data/future-meta.sqlite > public/ops/future-meta.sqlite.gz
wrangler pages deploy public --project-name=future-meta --branch=main --commit-dirty=true
```

本地全量 seed 使用九期网单品种 CSV：

- 发现入口：`https://www.9qihuo.com/qihuoshouxufei`
- 历史/单品种数据：`https://www.9qihuo.com/shouxufeixz?heyue=<code>`

## GitHub 定时更新

GitHub Actions 不从零构建历史库。每次运行：

1. 从 Cloudflare 下载 `ops/future-meta.sqlite.gz`。
2. 解压为 `data/future-meta.sqlite`。
3. 执行 `future-meta-daemon update-latest --require-seed`。
4. 导出 `public/manifest.json` 和 `public/latest.fmeta.zst`。
5. 重新 gzip 更新后的 SQLite seed 到 `public/ops/future-meta.sqlite.gz`。
6. 部署整个 `public/` 到 Cloudflare Pages。

最新截面来自总页 HTML 的 `table#heyuetbl`。页面上的 Excel 按钮是 `tableToExcel('heyuetbl', ...)` 生成，不存在稳定的 `heyue=all` CSV 下载端点，因此 daemon 直接解析 HTML 表格。

## Required GitHub Secrets

- `CLOUDFLARE_API_TOKEN`: token allowed to deploy Cloudflare Pages.
- `CLOUDFLARE_ACCOUNT_ID`: Cloudflare account id.

## 数据安全边界

- 不提交 `data/future-meta.sqlite`。
- 不提交 `public/` 生成物。
- 不存储或发布价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费等派生字段。
- latest HTML 中不是普通期货合约的代码会跳过，例如源站的月均价类 `l2607F`。
- latest HTML 不提供上市日、到期日、每手数量、最小跳动时，daemon 只从 Cloudflare seed 中已有 contract 元数据补齐；seed 不认识的新 symbol 会跳过，直到下一次本地全量 seed。

## Client URL

Default manifest URL:

`https://future-meta.pages.dev/manifest.json`
