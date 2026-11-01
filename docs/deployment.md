# GitHub 增量更新与数据发布

## 分发结构

- 客户端 archive：同仓库不可变 `data-<UTC时间>-<SHA前缀>` Release 附件 `latest.fmeta.zst`。
- 固定索引：`https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json`。
- 检查状态：同目录 `update-state.json`。
- `data` 为独立根提交的数据分支，只存上述两个 JSON。`main` 保存代码及 `config/official-rules.json`，不提交 archive、SQLite 或原件。

数据 Release 设置 `--latest=false`，SDK Release 的 Latest 独立保留。Pages 和线上维护包不再使用。历史原件、逐日观测、审计队列与 SQLite 只在本地留存，日更不依赖它们。

## SDK 与检查状态

archive 保持 schema 4。月均价合约支持需要 SDK v0.7.0 或更新版本，旧客户端会因不识别 F 尾缀而拒绝加载。升级一次后，SDK 版本不随每日数据更新。新版本默认使用当前入口，也可显式配置：

```rust
let config = future_meta::DownloadConfig {
    manifest_url: "https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json".into(),
    ..Default::default()
};
let meta = future_meta::load_or_fetch(config).await?;
```

源码默认地址已切换。旧 SDK tag 不改写，旧 Pages 地址不提供兼容入口。SDK v0.8.0 提供 `DownloadClient::refresh()`：HTTP 304 或 SHA 未变时保留当前 `FutureMeta` 和索引，SHA 变化时校验并替换缓存及实例，此时需重新解析句柄和构建预计算表。`load_or_fetch` 仍用于一次性加载。详见 [客户端 API](client-api.md#periodic-download-refresh)。

`manifest.generated_at` 是当前 archive 的生成时刻。`history_end` 保留业务版本边界的既有语义，不表示最后核验日期。核验进度读取状态文件：

- `last_attempt`：时段标识、开始/结束时间、结果、Actions run URL。硬终止可能留下 `running`，须查看对应 run。
- `last_success_at`：最近日度参数检查成功时刻；无业务变化也更新，不表示规格文档重新核验。
- `checked_through`：六所日度参数完整核验到的交易日，原子推进。
- `archive_sha256`、`current_release`、`previous_release`：检查点对应的数据。
- `exceptions`：既有未知、无观测合约及明确日历假设；不作为已确认历史补值。
- `source_versions.cffex_snapshot`：已核对的有效中金所参数快照。
- `source_versions.specifications`：规格采用 `approved_rules` 模式，记录配置 SHA、`approved_through` 和 `live_check_performed=false`。日更不会推进规格批准日期；独立规则审阅时间保存在本地 `rule-audit.json`。
- `retired_releases`：已被替换且待清理的旧数据版本，不包含未发布完成的上传。

迁移继承已核验截止日；无法从既有资料确定的尝试和成功时间初始化为 `null`，不以迁移时间冒充交易所检查时间。

## 日更流程

工作流 `.github/workflows/update-fee-data.yml` 使用内置 `GITHUB_TOKEN` 的 `contents: write`；checkout 不持久化凭据。生产 workflow 全程串行，旧任务不取消。

1. 纯离线日历准入；进入更新程序后复核实际时段。
2. 从 `data` 分支的同一 commit 读取 manifest 与状态，验证 SHA 绑定。已尝试同一时段或无缺失交易日时跳过。
3. 在访问交易所前提交 `running`。下载并验证约 135 KiB 的客户端 archive，解码为历史基线。
4. 按交易日补齐全部缺失日期，串行获取六所官方参数及上期所/能源中心日期匹配的 CBI，复用单次运行缓存和大商所浏览器会话。
5. 离线应用已批准的小规则配置。日更不请求全量规则文档或规格目录；乘数、最小变动价位和部分生命周期依赖独立维护的规则，并非每个交易日重新核验。新产品或规则不覆盖的合约仍停止整批。
6. 中金所先核对官方目录及既有基准快照 SHA。只有确认发布状态后，才可将仍有效的参数规则延用到后续交易日；历史同日期原文修订需人工核对。
7. 新合约仅使用明确适用的官方规格和生命周期规则；新产品、未知字段、冲突或不完整日表停止整批，不跳过合约、不用邻近值补齐。
8. Rust `update-archive` 验证完整六所快照、生命周期、参数及区间，保留合约 ID 和既有业务历史。按真实变更追加版本，保留“变化后恢复”的中间记录。未知费率不当作零值。
9. 无变化只更新状态，archive、SHA、manifest 和数据 Release 不变。有变化先上传不可变附件并公开读回验证，再一次提交切换 manifest 和成功检查点。
10. 固定 raw 入口可能有 CDN 延迟。公开读回一致后，仅删除记录为已替换的受管旧数据版本；保留当前、上一版、未完成发布的上传及所有 SDK Release。

请求失败、反爬页、旧日期、缺少日表/合约/元数据均不能推进检查点。失败只更新本次尝试状态；下一个合法时段继续补缺失日期。CI 原件仅在工作目录中临时使用，不上传为长期附件。Actions 只保留三天文本日志和更新摘要。

## 交易所访问窗口

候选 cron 为北京时间工作日 18:45、08:05。离线官方日历仅准入夜盘前或法定休市后首个早晨；周末、普通早晨、调休工作周末和节前无夜盘均跳过。

窗口为 18:30–20:00、08:00–08:20。每个日更时段只尝试一轮，重复触发不访问交易所。HTTP 最多三次尝试，间隔 2 秒、8 秒；CBI 最多两次 HTTP 加一次 Chrome。HTTP 每次重定向及浏览器每个网络请求均复核截止时间。一次更新包含多个参数文件、中金所参数目录及浏览器资源，不能表述为一个 HTTP 请求。

准入日历目前只核验至 2026 年；缺少年份停止，不把明确标注的生命周期假期假设用作采集日历。

## 独立规则维护

已留存的 2026-10-30 六所日度原文均只提供手续费、保证金等参数，不提供乘数或最小变动价位；上期所、能源中心的日期匹配 CBI 提供上市与到期日。日更沿用已批准规格，不能自动发现既有产品的规格修订。规则维护需要另行发起；未知新产品会硬失败，但不能将此视为覆盖已知产品变更的自动监控。

需要审阅交易所规格公告或更新规则配置时，显式运行独立检查；它没有定时触发，不读写 GitHub 数据分支，不发布 archive，也不推进日度检查点：

```bash
python3 scripts/audit_official_rules.py
# 可复用上一份成功报告的条件请求校验及目录选择绑定：
python3 scripts/audit_official_rules.py \
  --previous data/rule-audits/2026-11-02-evening/rule-audit.json
```

独立检查也先执行离线日历及实际时段门禁，周末或窗口外跳过，不创建工作目录。报告固定写入 `data/rule-audits/<日期>-<时段>/rule-audit.json`，原子创建目录占用该时段；运行中、失败后再次触发均跳过，不重复访问。保留目录作为尝试记录。完整检查约涉及 76 份文档、91 个规格入口（7 个复用文档核验），即约 160 个请求，另计重定向和重试。此为配置静态估算，不是线上计量；条件请求的 304 仍算请求。

报告单独记录开始、结束、配置 SHA 与成功/失败，失败不生成成功版本。旧附件 SHA 不变不能证明没有新规则：检查同时核对现行入口及其品种绑定。新文档、正文/目录变化、空目录或无法解析均失败；须本地核对生效时间、适用合约后更新配置，不能自动批准。已审阅的目录选择用 `approved_selection` 明确记录。审阅后按真实生效时间维护规格版本及 `approved_through`，不能用检查时间代替生效时间。报告和历史原件留在本地；日更不会将其作为维护包下载。

## 验证与故障诊断

普通运行始终执行时段门禁。手动验证当前公开数据，并写入内容不变的 Git 提交以实测内置令牌发布权限；不启动采集器、不重生成 archive，也不推进交易所检查点：

```bash
gh workflow run update-fee-data.yml --ref main -f republish_current=true
gh run watch <run-id> --exit-status
curl --fail --location https://raw.githubusercontent.com/zynthium/future-meta/data/manifest.json
curl --fail --location https://raw.githubusercontent.com/zynthium/future-meta/data/update-state.json
```

本地直接执行会真实写数据分支，并可能创建数据 Release，仅在已获更新/发布授权时使用：

```bash
xvfb-run -a python3 scripts/update_official.py \
  --repo zynthium/future-meta --work-dir /tmp/future-meta-update
```

`--work-dir` 必须不存在。交易所采集失败时查 run 日志；硬终止时以 Actions 结果为准。原件或现行目录变化时，在本地独立采集、核对规则内容、真实生效时间与合约范围，再更新 `config/official-rules.json`。不把抓取时间当作生效时间，不越过窗口反复试探源站。

首次迁移工具 `migrate_archive_state.py` 只读取已校验本地封存，生成小状态和规则，不联网、不发布。原始 SQLite 和重建、官方证据审阅工具仍供本地诊断。不得将其重新接为自动更新 fallback。
