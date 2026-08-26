# 证据门控更新验证

本流程用于验证公告发现、第三方交叉核对与官方证据受控应用的组合更新链路。它是上线前的验证要求，不是生产更新流程。

## 安全边界

- 不修改 `data/future-meta.sqlite`、`public/`、Cloudflare Pages 或 GitHub Actions。
- 每次验证都从基线数据库创建新的临时副本；临时目录在验证结束后可删除。
- `scan-announcements` 只会在临时副本写入公告正文、快照、候选和源状态，绝不写 `fee_versions`。
- 回放期间不得执行 `apply-verified-official`、`export`，也不得部署；官方应用仅在独立审阅副本中人工确认后另行验证。

准备临时目录：

```bash
REPLAY_DIR="$(mktemp -d)"
cp data/future-meta.sqlite "$REPLAY_DIR/future-meta.sqlite"
```

记录每轮命令、退出状态、标准错误、Jin10 输出和 SQLite 检查结果。不要把临时数据库或原始网页快照提交到 Git。

## 二十四个月回放

覆盖从当前基线向前连续二十四个自然月。每月选一个有交易的工作日；优先选择该月有公告、风控或手续费变更的日期。选定日期及理由须记录在回放报告中，不能用周末或节假日代替。

每个日期都使用新的临时副本，避免一个月的公告水位、候选状态或最新截面影响另一个月：

```bash
MONTH_DIR="$(mktemp -d)"
cp data/future-meta.sqlite "$MONTH_DIR/future-meta.sqlite"

cargo run -p future-meta-daemon -- validate-jin10 \
  --db "$MONTH_DIR/future-meta.sqlite" \
  --from YYYY-MM-DD --to YYYY-MM-DD \
  --out "$MONTH_DIR/jin10-YYYY-MM-DD.json"

# 周五样本或公告源回归样本使用 --reconcile-htfc。
cargo run -p future-meta-daemon -- scan-announcements \
  --db "$MONTH_DIR/future-meta.sqlite" --reconcile-htfc

cargo run -p future-meta-daemon -- inspect \
  --db "$MONTH_DIR/future-meta.sqlite"
```

Jin10 结果仅用于比较，不会写入费率历史。对每一个 mismatch，检查符号、开/平昨/平今语义、固定值或成交金额比例类型、以及当日官方公告；不能将缺少 Jin10 覆盖当作费率错误。

回放至少要包含下列锚点：

- 2026-03：SHFE/INE 的 SC、LU、FU 风控调费。
- 2026-03-27：曾出现的 `0.1 元/手` 占位污染，确认不会重新写入。
- Jin10 平昨/平今字段语义切换前后各至少一个日期。
- 9qihuo 曾改写历史的品种和日期，确认不会改写既有历史版本。
- 2015-09-07 至 2017-01-23：CFFEX 股指平今 `23%%` 的高倍但合法变化，确认门控不会误拒绝。

每个月的记录至少包含：日期、公告主源是否成功、是否触发华泰接管、候选数与最老未决年龄、Jin10 比较数与差异数、`fee_versions` 的行数和哈希、以及人工结论。除经完整官方证据受控应用的独立测试外，回放中 `fee_versions` 必须与副本初始值一致。

## 十四天影子运行

连续十四个自然日每天运行一次，与计划中的每日任务顺序相同，但每一天都从干净临时副本开始，且禁止 export/deploy：

```bash
DAY_DIR="$(mktemp -d)"
cp data/future-meta.sqlite "$DAY_DIR/future-meta.sqlite"

cargo run -p future-meta-daemon -- scan-announcements \
  --db "$DAY_DIR/future-meta.sqlite"

cargo run -p future-meta-daemon -- update-latest \
  --db "$DAY_DIR/future-meta.sqlite" --require-seed
```

周五运行必须附加 `scan-announcements --reconcile-htfc`。`update-latest` 只读诊断：无候选时
返回 `Noop`；有候选、新合约或安全门控拒绝时以 `Deferred` 原因非零退出，且不会写
`fee_versions`、合约状态、保证金、主力标记或 source state。无论结果如何，不导出、压缩
或发布 artifact。

每天记录公告源、源错误、候选状态、9qihuo/Jin10 可用性、命令退出状态，以及运行前后
`fee_versions` 的差异。发现任一非官方来源创建或改写手续费版本、最新诊断写入状态字段、
过期未决候选未阻止更新、或主备公告扫描同时失败但更新仍继续时，该日影子运行失败。

## 官方受控应用测试

在与回放副本分离的审阅副本中，准备已人工复核的证据库。每个调整必须包含精确合约、完整开/平昨/平今三腿、官方 URL、不可变正文或附件快照 SHA-256，以及前向生效时间。执行：

```bash
cargo run -p future-meta-daemon -- apply-verified-official \
  --db path/to/review-copy.sqlite \
  --evidence-db data/official-evidence.sqlite
```

核对新版本的 `source_kind='official'`、证据 URL 与 SHA-256 匹配、SCD2 时间边界连续、没有语义重复版本，并确认相应公告候选已被解析为已处理。缺腿、哈希不匹配、非官方域名、或未标记追溯的历史生效时间必须被拒绝。

## 启用条件与报告模板

只有同时满足以下条件，才可提出将流程接入生产定时任务的变更：

1. 二十四个月每月回放均完成并人工审阅。
2. 连续十四天影子运行全部安全完成。
3. 没有第三方来源创建、覆盖或回写 `fee_versions`。
4. 官方应用全部具备完整费率、不可变快照和官方来源，且只产生 `official` 溯源版本。
5. 中信主源、华泰接管和周五补漏的日志均已审核；主备同时失败与超过二十四小时未决候选都确实阻止后续更新。

建议每轮使用如下报告格式：

```text
日期：
阶段：24月回放 / 第N天影子运行 / 官方受控应用
临时数据库初始 fee_versions：行数=，SHA-256=
公告扫描：中信=成功/失败；华泰=未调用/接管/补漏；候选=；最老未决=
Jin10：比较=；差异=；人工核对结论=
更新：unchanged / 安全失败；原因=
临时数据库结束 fee_versions：行数=，SHA-256=；第三方写入=否
审阅人和结论：
```

完成这份报告不等于生产上线。生产切换仍须单独审阅、提交和明确授权。
