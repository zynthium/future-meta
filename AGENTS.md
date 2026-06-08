# AGENTS.md

## 项目概览

`future-meta` 是一个 Rust 2024 工作区，用于维护中国期货合约手续费历史，并给程序提供本地高性能 as-of 查询 API。

工作区只有两个 crate：

- `crates/future-meta`：客户端库。定义公开模型、压缩二进制 archive 编解码、TqSdk 风格 `symbol` 解析、as-of 查询索引，以及可选 `download` feature 的 Cloudflare artifact 下载缓存。
- `crates/future-meta-daemon`：维护进程。负责从 9qihuo 抓取数据、维护 SQLite 历史库、增量更新最新截面、导出 `manifest.json` 和 `latest.fmeta.zst`。

当前部署形态是 GitHub Actions 定时更新，Cloudflare Pages 免费层静态分发：

- 客户端下载 `https://future-meta.pages.dev/manifest.json` 和 `latest.fmeta.zst`。
- daemon seed 是 `https://future-meta.pages.dev/ops/future-meta.sqlite.gz`，只给 GitHub Actions 后续更新用，客户端不下载它。

## 数据模型硬约束

- 合约唯一标识只使用 TqSdk 风格 `symbol`，例如 `SHFE.cu2607`、`CZCE.SR903`、`KQ.m@SHFE.cu`。
- 不要把品种代码、品种名称、合约代码、合约名称、交易所代码、交易所名称等可由 `symbol` 或上下文派生的字段持久化进客户端 archive。
- 不要存储或发布源站展示用派生字段，例如价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费、手续费折算金额。
- 历史 seed 使用单品种 CSV 下载接口：`https://www.9qihuo.com/shouxufeixz?heyue=<code>`。
- 增量更新使用总表页 `https://www.9qihuo.com/qihuoshouxufei` 中的 `table#heyuetbl`。该页的 Excel 按钮是浏览器端 `tableToExcel(...)` 从 HTML 生成，不是稳定的全合约 CSV 下载端点。
- GitHub Actions 只做增量更新，不从零构建历史库。初始全量历史 seed 应在本地执行，便于手动切换代理/IP 应对源站 503 或限频。

## 仓库结构

- `Cargo.toml`：工作区成员、共享依赖和 lint 配置。
- `crates/future-meta/src/model.rs`：archive 和 manifest 的公开数据结构。
- `crates/future-meta/src/archive.rs`：`bincode` + `zstd` archive 编解码和 SHA-256 helper。
- `crates/future-meta/src/query.rs`：`FutureMeta` as-of 查询索引。
- `crates/future-meta/src/symbol.rs`：TqSdk 风格 symbol 解析和 normalization。
- `crates/future-meta/src/download.rs`：可选下载/cache API，只在 `download` feature 下编译。
- `crates/future-meta-daemon/src/source.rs`：9qihuo HTTP client、发现入口和 CSV URL 生成。
- `crates/future-meta-daemon/src/latest.rs`：总表页 latest HTML 解析。
- `crates/future-meta-daemon/src/parse.rs`：单品种 CSV 解析，只保留允许字段。
- `crates/future-meta-daemon/src/db.rs`：SQLite schema、seed 检查、SCD2 风格手续费版本维护。
- `crates/future-meta-daemon/src/refresh.rs`：历史刷新和 latest 增量更新 orchestration。
- `crates/future-meta-daemon/src/export.rs`：从 SQLite 导出 Pages artifacts。
- `docs/deployment.md`：Cloudflare Pages 和 GitHub Actions 部署细节。
- `.github/workflows/update-fee-data.yml`：每天北京时间 18:45 的定时更新 workflow。

## 环境与依赖

日常开发只需要稳定版 Rust。普通开发不依赖 Node 或 Python。

```bash
cargo metadata --no-deps --format-version 1
cargo build --workspace
```

客户端下载 API 受 `download` feature 控制：

```bash
cargo build -p future-meta --features download
```

## 日常开发命令

除非特别说明，所有命令都从仓库根目录执行。

常用本地检查：

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p future-meta --features download
```

定向测试：

```bash
cargo test -p future-meta-daemon --test daemon_pipeline
cargo test -p future-meta --test client_archive
cargo test -p future-meta symbol::tests
```

针对 Cloudflare 已发布 artifact 的在线客户端 smoke test：

```bash
cargo run -p future-meta --features download --example online_smoke SHFE.cu2607 2026-06-08T10:48:06Z
```

## Daemon 命令

检查已有 SQLite seed：

```bash
cargo run -p future-meta-daemon -- inspect --db data/future-meta.sqlite
```

发现 9qihuo 品种数据源：

```bash
cargo run -p future-meta-daemon -- discover --out data/sources.json
```

构建或刷新本地全量历史 seed。这个操作网络请求较多，通常应在稳定的本地/代理环境里手动执行：

```bash
cargo run -p future-meta-daemon -- seed-history --db data/future-meta.sqlite --force-full
```

在已有 seed 上应用最新截面增量：

```bash
cargo run -p future-meta-daemon -- update-latest --db data/future-meta.sqlite --require-seed
```

导出 Cloudflare Pages artifacts：

```bash
cargo run -p future-meta-daemon -- export --db data/future-meta.sqlite --out public
```

为 Cloudflare Pages 准备 daemon seed：

```bash
mkdir -p public/ops
gzip -c data/future-meta.sqlite > public/ops/future-meta.sqlite.gz
```

包含 `?heyue=...` 的 shell URL 必须加引号；否则 zsh 会尝试把 query string 当作 glob。

## 构建与部署

`public/` 是 Cloudflare Pages 的部署目录，包含：

- `manifest.json`
- `latest.fmeta.zst`
- `artifacts/*.fmeta.zst`
- `ops/future-meta.sqlite.gz`

仅在需要时手动部署：

```bash
wrangler pages deploy public --project-name=future-meta --branch=main --commit-dirty=true
```

GitHub Actions 工作流：

- 文件：`.github/workflows/update-fee-data.yml`
- 定时：`45 10 * * *`，对应北京时间 18:45。
- 运行时：Node 24 actions，使用 `actions/checkout@v6`、`cloudflare/wrangler-action@v4` 和 `FORCE_JAVASCRIPT_ACTIONS_TO_NODE24=true`。
- 必需 secrets：`CLOUDFLARE_API_TOKEN`、`CLOUDFLARE_ACCOUNT_ID`。

工作流顺序：

1. 运行测试。
2. 从 Cloudflare 下载 `ops/future-meta.sqlite.gz`。
3. 运行 `future-meta-daemon update-latest --require-seed`。
4. 导出 `public/manifest.json` 和 `public/latest.fmeta.zst`。
5. 替换 `public/ops/future-meta.sqlite.gz`。
6. 部署 `public/` 到 Cloudflare Pages。

除非用户明确要求，不要把本项目切到 Workers、R2 或 D1。当前已接受的架构是 Cloudflare Pages 免费层静态 artifact 分发。

## 代码风格

- Rust edition 为 2024。
- 工作区 lint 禁止 `unsafe_code`。
- Clippy `all` 和 `pedantic` 配置为 warning。新增代码应保持 `cargo clippy --workspace --all-targets --all-features` 下干净。
- 优先使用与现有 crate 边界一致的小模块和显式逻辑。不要把只属于 daemon 的抓取或 SQLite 逻辑移动到客户端 crate。
- 保持 `FutureMeta` 查询 API 的定位：archive 加载后，查询应是低分配的本地读取。
- 解析源站数据或 URL 时优先使用结构化 parser，例如 `csv`、`scraper`、`reqwest::Url`，不要做脆弱的临时字符串切分。
- archive 兼容性必须谨慎处理。修改 `SCHEMA_VERSION`、公开模型字段或 archive 编码方式时，需要补测试和迁移说明。

## 测试要求

合并代码改动前，至少运行：

```bash
cargo fmt --all -- --check
cargo test --workspace
cargo test -p future-meta --features download
cargo check --workspace --all-targets
```

涉及 parser、updater、DB 或 export 的改动，还要运行：

```bash
cargo test -p future-meta-daemon --test daemon_pipeline
```

涉及 download/cache 或 as-of 查询的改动，还要运行：

```bash
cargo test -p future-meta --features download --test client_archive
```

涉及 GitHub Actions 或部署的改动，要触发并观察 workflow：

```bash
gh workflow run update-fee-data.yml --ref main
gh run watch <run-id> --exit-status
```

然后验证 Cloudflare：

```bash
curl -L -s https://future-meta.pages.dev/manifest.json
curl -L -s -I https://future-meta.pages.dev/ops/future-meta.sqlite.gz
```

## 生成物与密钥

除非用户明确要求，不要提交本地或生成数据。

已忽略的生成路径包括：

- `target/`
- `data/`
- `public/`
- `.env`
- `.codex/`
- `.mcp.json`

不要把 Cloudflare token、GitHub token、代理凭据或源站 cookie 写入已跟踪文件。

## 故障排查

- 如果 GitHub Actions 在 `Refresh data` 因源站超时失败，先用 `gh run view <run-id> --log-failed` 查看失败日志。
- 9qihuo 可能返回 503、慢响应或反爬 HTML。发现源为空或 latest 表格缺失时必须硬失败，不要发布过期或空 artifact。
- 如果 `update-latest --require-seed` 因 DB 未 seeded 失败，从 `https://future-meta.pages.dev/ops/future-meta.sqlite.gz` 恢复 `data/future-meta.sqlite`，或重新构建本地全量 seed。
- latest HTML 可能包含月均价合约等非普通期货行。不支持的 symbol 应跳过，不要宽松 normalization。
- latest 行缺少静态元数据时，只能从现有 seed 补齐。seed 不认识的新 symbol 等下一次本地全量 seed 处理。

## 提交与协作规则

- 只 stage 与当前任务相关的文件。
- 除非任务明确要求发布 artifact，不要把生成的 `data/` 和 `public/` 文件放进 commit。
- 使用简洁的 conventional commit 风格提交信息，例如 `fix: retry source body read timeouts` 或 `chore: run fee update workflow on node 24 actions`。
- 如果改动触及部署或更新行为，最终报告必须包含实际运行过的命令和 GitHub Actions run URL。
