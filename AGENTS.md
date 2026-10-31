# AGENTS.md

## 项目概览

`future-meta` 是一个 Rust 2024 工作区，用于维护中国期货合约手续费历史，并给程序提供本地高性能 as-of 查询 API。

工作区只有两个 crate：

- `crates/future-meta`：客户端库。定义公开模型、压缩二进制 archive 编解码、TqSdk 风格 `symbol` 解析、as-of 查询索引，以及可选 `download` feature 的 Cloudflare artifact 下载缓存。
- `crates/future-meta-daemon`：维护进程。负责从官方采集输出重建或增量合并 SQLite 历史库、审阅导入官方证据、导出 `manifest.json` 和 `latest.fmeta.zst`。

部署架构是 GitHub Actions 官方参数增量更新，Cloudflare Pages 免费层静态分发。源码配置与线上验收状态应分别核实：

- 客户端下载 `https://future-meta.pages.dev/manifest.json` 和 `latest.fmeta.zst`。
- daemon seed 是 `https://future-meta.pages.dev/ops/future-meta.sqlite.gz`，配套官方元数据 bootstrap 索引为 `ops/official-inputs.json`，只给后续维护使用，客户端不下载。

## 数据模型硬约束

- 合约唯一标识只使用 TqSdk 风格 `symbol`，例如 `SHFE.cu2607`、`CZCE.SR903`、`KQ.m@SHFE.cu`。
- 不要把品种代码、品种名称、合约代码、合约名称、交易所代码、交易所名称等可由 `symbol` 或上下文派生的字段持久化进客户端 archive。
- 不要存储或发布源站展示用派生字段，例如价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费、手续费折算金额。
- 手续费和保证金只接受六所官方逐日参数及保留原件；初始历史由官方采集输出从零重建，不从旧 SQLite 或第三方数据补值。
- 生命周期与规格需要独立的官方来源账本。用户明确授权的日历假设必须保留标签，不能冒称交易所确认日期。
- GitHub Actions 只补维护库截止日后的官方交易日；维护库与 bootstrap 必须有相同截止日。参数未发布、请求失败或元数据不完整均失败，不以旧值冒充当日数据。
- 公告扫描保留发现与人工审计队列，不作为官方日参数日更的候选门槛。旧第三方抓取和更新 CLI 已移除；遗留来源记录仅供历史审计。

## 仓库结构

- `Cargo.toml`：工作区成员、共享依赖和 lint 配置。
- `crates/future-meta/src/model.rs`：archive 和 manifest 的公开数据结构。
- `crates/future-meta/src/archive.rs`：`bincode` + `zstd` archive 编解码和 SHA-256 helper。
- `crates/future-meta/src/query.rs`：`FutureMeta` as-of 查询索引。
- `crates/future-meta/src/symbol.rs`：TqSdk 风格 symbol 解析和 normalization。
- `crates/future-meta/src/download.rs`：可选下载/cache API，只在 `download` feature 下编译。
- `crates/future-meta-daemon/src/model.rs`：daemon 内部共享的参数行与静态元数据模型。
- `crates/future-meta-daemon/src/db.rs`：SQLite schema、版本维护及历史审计兼容。
- `crates/future-meta-daemon/src/rebuild.rs`：官方采集输出的新库重建及来源关联。
- `crates/future-meta-daemon/src/update_collected.rs`：短区间官方采集库向维护副本的增量合并。
- `crates/future-meta-daemon/src/export.rs`：从 SQLite 导出 Pages artifacts。
- `scripts/collect_settle_params.py`：六所官方结算参数采集。
- `scripts/build_lifecycles.py`、`scripts/collect_contract_specs.py`：官方生命周期与规格生成。
- `scripts/update_official.py`：日历判断、增量采集、重建、合并、验证和打包 orchestration。
- `scripts/package_official_inputs.py`：可移植官方元数据 bootstrap 的打包与摘要校验恢复。
- `docs/deployment.md`：Cloudflare Pages 和 GitHub Actions 部署细节。
- `.github/workflows/update-fee-data.yml`：每天北京时间 18:45 的定时更新 workflow。

## 环境与依赖

客户端和 daemon 日常构建只需要稳定版 Rust。官方采集与日更脚本需要 Python、`requests` 和 Playwright Chrome；Linux 大商所浏览器使用 `xvfb-run`，详见 `scripts/README.md`。

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

从官方采集输出创建新库：

```bash
cargo run -p future-meta-daemon -- rebuild-collected \
  --metadata-dir path/to/metadata --settle-dir path/to/settle_params \
  --settlement-sources path/to/evidence/settlement_sources.json \
  --out path/to/new.sqlite
```

完整的采集、生命周期和规格准备顺序见 `scripts/README.md` 与 `docs/official-rebuild.md`。
输入不包含旧 SQLite，输出文件必须不存在。

把已验证的短区间重建库合并到新维护副本：

```bash
cargo run -p future-meta-daemon -- update-collected \
  --db path/to/seed.sqlite --delta path/to/delta.sqlite --out path/to/new.sqlite
```

执行完整官方日更，不部署：

```bash
python3 scripts/update_official.py \
  --db data/future-meta.sqlite --inputs data/official-inputs.json \
  --work-dir /tmp/future-meta-official-update --public-dir /tmp/future-meta-pages
```

每次使用新的 work-dir。Linux 下前置 `xvfb-run -a`。输入维护库只读，
配套 bootstrap 的打包、恢复与截止日要求见 `docs/deployment.md`。

验证并导出官方采集库：

```bash
cargo run -p future-meta-daemon -- verify-collected \
  --db path/to/new.sqlite --out public/coverage.json
cargo run -p future-meta-daemon -- export --db path/to/new.sqlite --out public
```

`verify-collected` 验证原件关联并报告观测覆盖，不证明自上市日起完整。严格历史审阅
另用 `audit-coverage --strict`；人工官方证据导入保留独立流程，见 `docs/official-evidence.md`。

## 构建与部署

`public/` 是 Cloudflare Pages 的部署目录，包含：

- `manifest.json`
- `latest.fmeta.zst`
- `artifacts/*.fmeta.zst`
- `coverage.json`
- `ops/future-meta.sqlite.gz`
- `ops/official-inputs.json` 及其引用的 gzip 分片

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

1. 运行 Rust 和官方日更脚本检查。
2. 恢复 Pages 中截止日一致的 SQLite seed 与官方输入 bootstrap，并验证摘要及采集关联。
3. 独立读取官方交易日历，只采集截止日后缺失的六所参数与新日期合约基础信息。
4. 严格生成生命周期和规格，构建短区间库，合并到新维护副本。
5. 验证、导出、整理维护库空间，生成同截止日的新 seed 和 bootstrap；无新交易日时不部署。
6. 部署新的 `public/`，保留失败与成功运行的诊断。公告扫描错误独立报告，不清空待复核队列。

bootstrap 使用不超过 20 MiB 的分片；维护库 gzip 超过 Pages 25 MiB 限制时失败。
新流程的源码配置不代表 bootstrap 已上线或工作流已经验收，发布状态必须读回确认。

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

涉及 GitHub Actions 或部署的改动，在获得运行及发布授权后触发并观察 workflow。该工作流可能部署 Pages，不能把普通代码检查当作生产发布授权：

```bash
gh workflow run update-fee-data.yml --ref main
gh run watch <run-id> --exit-status
```

然后验证 Cloudflare：

```bash
curl -L -s https://future-meta.pages.dev/manifest.json
curl -L -s https://future-meta.pages.dev/coverage.json
curl -L -s https://future-meta.pages.dev/ops/official-inputs.json
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

- workflow 失败时先用 `gh run view <run-id> --log-failed`，再查看上传的 diagnostics、原始输入、缺口和覆盖报告。
- 官方请求错误、反爬响应或参数尚未发布不能当作休市。DCE 浏览器在 Linux 上需要 Chrome 与 `xvfb-run`；修复运行环境后从干净 work-dir 重跑。
- seed 与 bootstrap 截止日不一致或摘要校验失败时，恢复同一发布的配套输入，不跳过校验。
- 新合约缺少生命周期或规格时补充官方原件，不能从旧 SQLite、相邻合约或品种默认值补齐，也不能跳过缺失合约。
- 首条观测前的空档继续公开为未知；`valid_to=None` 仅表示没有已知后继版本，不能据此声称观测截止日之后已获核验。
- `scan-announcements` 的人工待复核状态与官方日参数更新分开处理；旧二十四小时候选门槛和第三方可用性不再阻断日更。

## 提交与协作规则

- 只 stage 与当前任务相关的文件。
- 除非任务明确要求发布 artifact，不要把生成的 `data/` 和 `public/` 文件放进 commit。
- 使用简洁的 conventional commit 风格提交信息，例如 `fix: retry source body read timeouts` 或 `chore: run fee update workflow on node 24 actions`。
- 如果改动触及部署或更新行为，最终报告必须包含实际运行过的命令和 GitHub Actions run URL。
