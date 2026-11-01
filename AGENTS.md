# AGENTS.md

## 项目与分发

`future-meta` 是 Rust 2024 工作区，提供中国期货手续费、保证金和规格历史的本地 as-of 查询。

- `crates/future-meta`：公开模型、schema 4 archive 编解码、symbol、查询及可选 `download` 缓存。
- `crates/future-meta-daemon`：基于 archive 的业务历史增量；保留本地 SQLite 重建、证据审阅和导出工具。
- 自动更新只使用上一版客户端 archive、轻量检查点和小规则配置，不使用线上 SQLite 或原件 bootstrap。
- 客户端文件作为不可变 `data-*` Release 附件；固定索引为 `https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json`。
- 独立 `data` 分支只存 `manifest.json` 与 `update-state.json`。数据 Release 使用 `--latest=false`，不改变 SDK Latest。
- 不再使用 Pages。历史证据与维护 SQLite 只在本地保存，不重新引入线上维护包或兼容旧托管地址。

## 数据硬约束

- 合约标识只用 TqSdk 风格具体 futures symbol，例如 `SHFE.cu2701`、`CZCE.SR701`。`KQ.m@...`、`KQ.i@...` 不返回手续费或规格。
- 支持 DCE 的 `l/pp/v` 月均价合约，保留大写 `F` 尾缀。SDK 品种标识沿用 TqSdk 的 `DCE.l/pp/v`，内部规则使用 `l-F/pp-F/v-F`，不得借用普通合约规则或删除尾缀。日更从同一 DCE 响应获取其费率，不增加官网请求。
- 不把可派生的品种/交易所名称、展示价格、每手保证金、每跳盈亏等字段写入客户端 archive。
- 手续费和保证金只接受六所官方参数。日期错误、反爬页、空表、不完整交易日或元数据必须失败，不能以旧值冒充当日数据。
- archive 是已批准业务历史基线；保留合约 ID 和既有版本，仅在业务变化时追加/关闭区间。
- 缺失交易日必须按顺序补齐，不能只比较最新截面而漏掉中间变化与恢复。
- 新合约使用明确适用的官方规则，不能借用邻近合约或品种默认值。未知费率与零值严格区分。
- 规格、生命周期及已授权假期假设必须保留标签。日更复用已批准规格，不声称当日重新核验；全量原件/规格目录由独立手动审阅入口核对，发现无法解释的变化时停止审阅，先在本地修订小规则配置。
- 中金所可在核对官方目录及现有快照 SHA 后延用有效参数；历史同日期原文修订不得忽略。
- 原件正文与完整逐日观测不属于在线检查点。CI 原件只作本次临时输入，不作为长期 artifact 上传。

## 更新时间语义

`update-state.json` 区分：

- `last_attempt`：最近开始/结束、结果、时段和 run URL；硬终止可能留 `running`。
- `last_success_at`：最近日度参数检查成功，无业务变化也推进，不表示规格文档重新核验。
- `checked_through`：六所日度参数完整核验到的交易日，原子推进。
- `archive_sha256` 和当前/上一版 Release：检查点对应的业务基线。
- `exceptions` / `source_versions`：必要未知、假设和当前来源版本。
- `source_versions.specifications`：规则配置 SHA、批准截止日与 `live_check_performed=false`；独立规则审阅时间写在本地报告，不随日更推进。

无业务变化时 archive 字节、SHA、生成时间、manifest 和数据 Release 不变，只更新状态。失败只记录尝试，不推进成功检查点。`manifest.history_end` 不是核验截止日。

## 入口与文件

- `crates/future-meta-daemon/src/archive_update.rs`：完整六所批次校验及 archive 版本合并。
- `scripts/update_official.py`：时段复核、状态、采集、合并与发布。
- `scripts/incremental_inputs.py`：日期、规格、生命周期及官方采集适配。
- `scripts/audit_official_rules.py`：独立手动规则审阅，调用 `catalog_guard.py` 检查现行入口；不由日更触发，不发布，不推进日更检查点。不能用旧附件仍可访问证明没有新规则。
- `scripts/official_access.py`：HTTP 重定向及浏览器逐请求截止控制。
- `scripts/data_branch.py`：同一 commit 读取索引/状态，Git API 非强制提交。
- `scripts/publish_data.py`：不可变附件发布、公开 SHA 验证和退役数据清理。
- `scripts/collect_settle_params.py`：六所官方采集，可供独立本地历史采集。
- `config/official-rules.json`：已批准且有适用范围的轻量规则及来源。
- `config/update-calendar.json`：已核验官方准入日历。
- `.github/workflows/update-fee-data.yml`：生产更新。

本地历史工具和约束见 `scripts/README.md`、`docs/official-rebuild.md`、`docs/official-evidence.md`。更新与分发见 `docs/deployment.md`。

## 交易所访问

- cron 仅工作日 18:45、08:05 北京时间。18:45 用于夜盘前；08:05 仅法定休市后首个早晨。
- 离线门禁跳过周末、普通早晨、调休工作周末、休市和节前无夜盘；程序在运行时再次检查。
- 只在 18:30–20:00、08:00–08:20 内开始官方请求。同一时段最多一轮，重复触发跳过。
- 串行采集、缓存去重、复用大商所浏览器。HTTP 最多三次，等待 2 秒、8 秒；CBI 两次 HTTP 加一次 Chrome。
- 每次 HTTP 重定向和浏览器网络请求复核截止；不得通过手动日更绕过窗口。
- 当前准入日历只涵盖 2026，缺少年份停止；生命周期假设不能当成采集日历。
- 实际采集包含多份官方文件和浏览器资源，不能把一轮更新说成一个 HTTP 请求。
- 日更不检查全量规则文档/规格目录；中金所日度参数目录及基准快照校验仍必需。独立手动规则审阅同样遵循日历和窗口门禁。

## 常用检查

```bash
cargo fmt --all -- --check
cargo check --workspace --all-targets
cargo test --workspace
cargo test -p future-meta --features download
cargo clippy --workspace --all-targets --all-features -- -D warnings
python3 -m unittest discover -s tests
```

Python 官方采集测试需 `requests`、`playwright`；Linux 在线大商所需要 Chrome 与 `xvfb-run`。普通 Rust 构建不依赖它们。

定向验证：

```bash
cargo test -p future-meta-daemon --test archive_incremental
cargo test -p future-meta-daemon --test daemon_pipeline
cargo test -p future-meta --features download --test client_archive
```

SDK smoke test：

```bash
cargo run -p future-meta --features download --example online_smoke \
  SHFE.cu2701 2026-10-30T10:00:00+08:00
```

包含 F 合约的 archive 需要 SDK v0.7.0；v0.6.0 及更早版本会拒绝这些 symbol，必须升级一次。archive wire schema 保持 4，之后每日数据不要求 SDK 升级；不维护旧客户端兼容包，不改写旧 tag。

## 更新和发布授权

`update_official.py` 会真实写数据分支，并可能创建 Release，须已有更新/发布授权。无交易所访问的线上验证也通过同一入口：

```bash
gh workflow run update-fee-data.yml --ref main -f republish_current=true
gh run watch <run-id> --exit-status
```

此模式下载并验证现有 archive 与公开索引，并写入内容不变的 Git 提交实测发布权限；不启动采集器、不重新生成 archive、不推进交易所检查点。源码检查不等于生产成功，必须观察 Actions 完成并读回固定入口。

GitHub raw 存在 CDN 延迟。先验证附件，再原子切换索引/检查点；固定 URL 读回通过后才清理记录为已退役的受管数据版本。保留当前、上一版及未完成发布的上传，不触碰 SDK Release。不能按创建时间猜上一版。

## 风格、生成物与协作

- Rust 2024、禁止 unsafe，遵循现有 crate 边界。抓取与维护逻辑留在 daemon/Python。
- 解析优先结构化 parser。保持 archive 加载后的低分配本地查询定位。
- schema 4 和公开客户端模型变更必须有测试及迁移说明。
- `data/`、`public/`、`target/`、`.env`、`.codex/`、`.mcp.json` 不提交；不得删除本地历史证据作为清理代码的副作用。
- `config/official-rules.json` 是经审阅的小配置，允许随规则更新提交；大 archive 和原件不进入源码分支。
- 只 stage 当前任务文件，保留无关脏工作；使用简洁 conventional commit。
- 不提交 token、Cookie、代理凭据或密钥。发布只使用内置 `GITHUB_TOKEN` 的 `contents: write`，checkout 不持久化凭据。
- 涉及更新/分发的最终报告包含实际验证命令和 Actions run URL；没有合法窗口的真实采集不能冒称已验证。
