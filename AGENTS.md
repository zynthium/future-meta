# AGENTS.md

## Project Overview

`future-meta` 是一个 Rust 2024 workspace，用于维护中国期货合约手续费历史，并给程序提供本地高性能 as-of 查询 API。

Workspace 只有两个 crate：

- `crates/future-meta`: client library。定义公开模型、压缩二进制 archive 编解码、TqSdk 风格 `symbol` 解析、as-of 查询索引，以及可选 `download` feature 的 Cloudflare artifact 下载缓存。
- `crates/future-meta-daemon`: maintenance daemon。负责从 9qihuo 抓取数据、维护 SQLite 历史库、增量更新最新截面、导出 `manifest.json` 和 `latest.fmeta.zst`。

当前部署形态是 GitHub Actions 定时更新，Cloudflare Pages 免费层静态分发：

- Client 下载 `https://future-meta.pages.dev/manifest.json` 和 `latest.fmeta.zst`。
- Daemon seed 是 `https://future-meta.pages.dev/ops/future-meta.sqlite.gz`，只给 GitHub Actions 后续更新用，client 不下载它。

## Core Data Rules

- 合约唯一标识只使用 TqSdk 风格 `symbol`，例如 `SHFE.cu2607`、`CZCE.SR903`、`KQ.m@SHFE.cu`。
- 不要把品种代码、品种名称、合约代码、合约名称、交易所代码、交易所名称等可由 `symbol` 或上下文派生的字段持久化进 client archive。
- 不要存储或发布源站展示用派生字段，例如价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费、手续费折算金额。
- 历史 seed 使用单品种 CSV 下载接口：`https://www.9qihuo.com/shouxufeixz?heyue=<code>`。
- 增量更新使用总表页 `https://www.9qihuo.com/qihuoshouxufei` 中的 `table#heyuetbl`。该页的 Excel 按钮是浏览器端 `tableToExcel(...)` 从 HTML 生成，不是稳定的全合约 CSV 下载端点。
- GitHub Actions 只做增量更新，不从零构建历史库。初始全量历史 seed 应在本地执行，便于手动切换代理/IP 应对源站 503 或限频。

## Repository Layout

- `Cargo.toml`: workspace members、共享依赖和 lint 配置。
- `crates/future-meta/src/model.rs`: archive 和 manifest 的公开数据结构。
- `crates/future-meta/src/archive.rs`: `bincode` + `zstd` archive 编解码和 SHA-256 helper。
- `crates/future-meta/src/query.rs`: `FutureMeta` as-of 查询索引。
- `crates/future-meta/src/symbol.rs`: TqSdk 风格 symbol 解析和 normalization。
- `crates/future-meta/src/download.rs`: 可选下载/cache API，只在 `download` feature 下编译。
- `crates/future-meta-daemon/src/source.rs`: 9qihuo HTTP client、发现入口和 CSV URL 生成。
- `crates/future-meta-daemon/src/latest.rs`: 总表页 latest HTML 解析。
- `crates/future-meta-daemon/src/parse.rs`: 单品种 CSV 解析，只保留允许字段。
- `crates/future-meta-daemon/src/db.rs`: SQLite schema、seed 检查、SCD2 风格手续费版本维护。
- `crates/future-meta-daemon/src/refresh.rs`: 历史刷新和 latest 增量更新 orchestration。
- `crates/future-meta-daemon/src/export.rs`: 从 SQLite 导出 Pages artifacts。
- `docs/deployment.md`: Cloudflare Pages 和 GitHub Actions 部署细节。
- `.github/workflows/update-fee-data.yml`: 每天北京时间 18:45 的定时更新 workflow。

## Setup Commands

Use stable Rust. There is no Node/Python dependency for normal development.

```bash
cargo metadata --no-deps --format-version 1
cargo build --workspace
```

Optional client download API is feature-gated:

```bash
cargo build -p future-meta --features download
```

## Development Workflow

Run commands from the repository root.

Common local checks:

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p future-meta --features download
```

Targeted tests:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline
cargo test -p future-meta --test client_archive
cargo test -p future-meta symbol::tests
```

Run the online client smoke test against Cloudflare:

```bash
cargo run -p future-meta --features download --example online_smoke SHFE.cu2607 2026-06-08T10:48:06Z
```

## Daemon Commands

Inspect an existing SQLite seed:

```bash
cargo run -p future-meta-daemon -- inspect --db data/future-meta.sqlite
```

Discover 9qihuo variety sources:

```bash
cargo run -p future-meta-daemon -- discover --out data/sources.json
```

Build or refresh the local full history seed. This is network-heavy and should normally be done manually from a stable local/proxy environment:

```bash
cargo run -p future-meta-daemon -- seed-history --db data/future-meta.sqlite --force-full
```

Apply latest incremental data on top of an existing seed:

```bash
cargo run -p future-meta-daemon -- update-latest --db data/future-meta.sqlite --require-seed
```

Export Pages artifacts:

```bash
cargo run -p future-meta-daemon -- export --db data/future-meta.sqlite --out public
```

Prepare the daemon seed for Cloudflare Pages:

```bash
mkdir -p public/ops
gzip -c data/future-meta.sqlite > public/ops/future-meta.sqlite.gz
```

Quote shell URLs that contain `?heyue=...`; zsh will otherwise try to glob query strings.

## Build And Deployment

`public/` is the Cloudflare Pages payload and contains:

- `manifest.json`
- `latest.fmeta.zst`
- `artifacts/*.fmeta.zst`
- `ops/future-meta.sqlite.gz`

Deploy manually only when needed:

```bash
wrangler pages deploy public --project-name=future-meta --branch=main --commit-dirty=true
```

GitHub Actions workflow:

- File: `.github/workflows/update-fee-data.yml`
- Schedule: `45 10 * * *`, which is 18:45 Asia/Shanghai.
- Runtime: Node 24 actions via `actions/checkout@v6`, `cloudflare/wrangler-action@v4`, and `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true`.
- Required secrets: `CLOUDFLARE_API_TOKEN`, `CLOUDFLARE_ACCOUNT_ID`.

The workflow sequence is:

1. Run tests.
2. Download `ops/future-meta.sqlite.gz` from Cloudflare.
3. Run `future-meta-daemon update-latest --require-seed`.
4. Export `public/manifest.json` and `public/latest.fmeta.zst`.
5. Replace `public/ops/future-meta.sqlite.gz`.
6. Deploy `public/` to Cloudflare Pages.

Do not switch this project to Workers/R2/D1 unless the user explicitly asks. The current accepted architecture is static artifact distribution on Cloudflare Pages free tier.

## Code Style

- Rust edition is 2024.
- Workspace lint forbids `unsafe_code`.
- Clippy `all` and `pedantic` are configured as warnings. Keep new code clean under `cargo clippy --workspace --all-targets --all-features`.
- Prefer small, explicit modules that match current crate boundaries. Do not move daemon-only fetch/SQLite logic into the client crate.
- Preserve `FutureMeta` query APIs as allocation-light local reads after archive load.
- Use structured parsers (`csv`, `scraper`, `reqwest::Url`) instead of ad hoc string splitting when parsing source data or URLs.
- Keep archive compatibility deliberate: changing `SCHEMA_VERSION`, public model fields, or archive encoding requires tests and migration notes.

## Testing Expectations

Before merging code changes, run at least:

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo test -p future-meta --features download
cargo check --workspace --all-targets
```

For parser, updater, DB, or export changes, also run:

```bash
cargo test -p future-meta-daemon --test daemon_pipeline
```

For download/cache or as-of query changes, also run:

```bash
cargo test -p future-meta --features download --test client_archive
```

For GitHub Actions or deployment changes, trigger and watch the workflow:

```bash
gh workflow run update-fee-data.yml --ref main
gh run watch <run-id> --exit-status
```

Then verify Cloudflare:

```bash
curl -L -s https://future-meta.pages.dev/manifest.json
curl -L -s -I https://future-meta.pages.dev/ops/future-meta.sqlite.gz
```

## Generated Files And Secrets

Do not commit local/generated data unless the user explicitly requests it.

Ignored generated paths include:

- `target/`
- `data/`
- `public/`
- `.env`
- `.codex/`
- `.mcp.json`

Never put Cloudflare tokens, GitHub tokens, proxy credentials, or source-site cookies into tracked files.

## Troubleshooting

- If GitHub Actions fails in `Refresh data` with source timeout, inspect failed logs first with `gh run view <run-id> --log-failed`.
- 9qihuo may return 503, slow responses, or anti-bot HTML. Empty source discovery and missing latest table should fail hard rather than publish stale or empty artifacts.
- If `update-latest --require-seed` fails because the DB is not seeded, restore `data/future-meta.sqlite` from `https://future-meta.pages.dev/ops/future-meta.sqlite.gz` or rebuild a local full seed.
- latest HTML may contain non-futures rows such as monthly average contracts. Unsupported symbols should be skipped, not normalized loosely.
- latest rows missing static metadata must be completed from the existing seed. Unknown new symbols should wait for the next local full seed.

## Commit And PR Guidelines

- Stage only files relevant to the requested change.
- Keep generated `data/` and `public/` files out of commits unless the task is explicitly about publishing artifacts.
- Use concise conventional commit messages, for example `fix: retry source body read timeouts` or `chore: run fee update workflow on node 24 actions`.
- In final reports, include the commands actually run and the GitHub Actions run URL when deployment/update behavior was touched.
