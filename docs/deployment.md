# future-meta Deployment

部署架构使用 GitHub Actions 采集官方日参数，客户端 archive 放在版本化数据 Release，Pages 托管稳定 manifest、覆盖报告和维护输入。SDK 与每日数据独立，v0.6.0 下载器可直接跟随新附件 URL。

## 验证与上线状态

[分发拆分 workflow 41195519499](https://github.com/zynthium/future-meta/actions/runs/41195519499) 已通过完整测试、数据 Release 上传与公开读回校验、Pages 部署及版本清理。该运行使用 `republish_current=true`，官方采集步骤 skipped，全程只恢复已有发布数据。

当前客户端数据附件见 [data-20261031T110854Z-b4fb0dcd0469](https://github.com/zynthium/future-meta/releases/tag/data-20261031T110854Z-b4fb0dcd0469)。生产 manifest 指向固定版本 Release URL，附件大小与 SHA 一致；Pages 的 `latest.fmeta.zst` 和旧 `artifacts/` 文件已不再提供。

SDK Latest 保持 v0.6.0；客户端 crate 与锁文件相对该 tag 无改动。实际运行原 SDK，空缓存下载及已有旧缓存替换两种情况均成功，缓存 SHA 与数据 Release 匹配。下载的维护 SQLite 与原先已验收 seed 逐字节一致、完整性 `ok`；519 份官方原件、截止日、整体及逐文件 SHA 保持一致，观测仍截至 2026-10-30。

覆盖边界继续保留：12 个合约没有日观测，461 个合约保留既有 2027 假期假设标签，待复核公告 `citics:826232` 未被清空。

此前的 [GitHub 托管迁移验收](https://github.com/zynthium/future-meta/actions/runs/41185460063) 已完成；原云项目、旧云 secrets 和一次性迁移工具均已删除。当前仅使用 Release 数据附件与 Pages 索引/维护输入。

## 当前 GitHub Pages 部署

- Repository: `zynthium/future-meta`
- Publishing source: GitHub Actions
- Production URL: `https://zynthium.github.io/future-meta`
- Manifest URL: `https://zynthium.github.io/future-meta/manifest.json`
- Daemon seed URL: `https://zynthium.github.io/future-meta/ops/future-meta.sqlite.gz`

## 发布内容

`public/` 是导出暂存目录：archive 只在本地准备，下载 URL 由发布脚本核验后填写。实际部署目录 `data/pages-site/` 仅包含：

- `manifest.json`
- `ops/future-meta.sqlite.gz`
- `coverage.json`
- `ops/official-inputs.json`
- `ops/official-inputs.tar.gz`
- `ops/publication.json`：当前与上一版数据 Release 的发布记录。

`latest.fmeta.zst` 上传到 `data-<UTC时间>-<SHA前缀>` Release 附件；Pages manifest 的 `mirrors` 指向固定版本 URL，不使用 SDK 的 `/releases/latest/download/`。数据 Release 始终设置 `--latest=false`，SDK 的 Latest 独立保留。

`ops/future-meta.sqlite.gz` 和官方输入只供后续更新使用；客户端不下载。`ops/` 仍是公开静态资源，并不提供私有存储。

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

公告候选证据 SQLite 与 `data/future-meta.sqlite` 完全隔离，且不属于 Pages 发布物；日更所需的已验证官方元数据原件则通过下文 bootstrap 发布。满足完整、连续、人工复核条件前，禁止把候选材料写入生产历史。具体标准见 [官方历史证据流程](official-evidence.md)。

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

该命令不在定时 workflow 中。必须先在隔离副本核对完整三腿、原件摘要、生效区间和审阅结果，再按用户授权发布。旧的[证据门控更新验证](evidence-gated-update-validation.md)已归档，其第三方候选回放、影子运行与二十四小时公告门槛不适用于当前官方参数日更。

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
版本连续性及 `official` 溯源。失败时保留完整 JSON 缺口清单，不得宣称请求期间严格完整。采集重建库按下文观测覆盖发布，并公开未知区间。

## GitHub 定时更新

候选时刻为工作日北京时间 18:45 (`45 10 * * 1-5`) 和 08:05 (`5 0 * * 1-5`)；周末不触发。
轻量 admission job 只读本地已核验官方日历：18:45 仅在当晚有正常夜盘时更新，08:05 仅在法定休市后的首个早晨补跑。
正常周五夜盘保留；普通周一早晨、节前无夜盘及休市日跳过。更新源为六所官方逐日参数，不再使用
9qihuo/Jin10 候选触发历史写入或冻结客户端 artifact。CI 只补缺失区间，不从零采集历史。

启动前需要把同一观测截止日的两份文件发布到 Pages `ops/`：

- `future-meta.sqlite.gz`：通过 `verify-collected` 的官方采集重建维护库。
- `official-inputs.json`：可移植的官方规格/生命周期/CBI/日历清单及其原始证据；不含 SQLite
  或旧手续费 CSV。bootstrap 清单包含截止日、每个文件 SHA-256、显式生命周期假设配置。传输索引 schema 2 引用同一 Pages `ops/` 中的单个 `official-inputs.tar.gz`，记录 SHA-256 与大小。旧分片索引直接拒绝。

本地生成 bootstrap（示例中的假期参数仅复用已授权的 2027 配置）：

```bash
python3 scripts/package_official_inputs.py package \
  --evidence-dir data/official-evidence \
  --provenance data/contracts_meta_provenance.json \
  --through 2026-10-30 --assume-2027-holidays \
  --out public/ops/official-inputs.json
```

工作流恢复并校验两份输入，要求截止日一致。从 `rebuild_summary.through` 的次日开始，
复用已校验 SHA 的官方 CFFEX 月度交易日历，只有缺失月份才请求，跳过有证据的休市日；日历请求失败直接失败。没有新交易日
时正常结束、不部署。18:30 中国时间前只允许采到前一天，之后可请求当天已完成的结算数据；
任何交易所尚未发布完整参数都会失败，保留诊断，不以旧日参数伪装当天结果。
更新脚本通过 `--scheduled` 在实际执行前再次检查时间，避免排队跨过允许时段。准入窗口为 18:30–20:00、08:00–08:20，更新 job 最长 30 分钟；过窗直接跳过。
定时补采使用 `--workers 1`，只补维护库缺失的交易日；已是最新截止日则不请求交易所。重试有限，不做循环轮询。
节后早晨只补已完成交易日的数据，不代表已获知当天尚未发布的结算参数或未来生效手续费。

门禁日历位于 `config/update-calendar.json`，包含官方来源 URL 和原件 SHA。当前已核验 2026 年；跨年需要补充新年度官方日历，覆盖不足时明确报错并停止，不沿用生命周期中的假期假设。
调度边界和 GitHub 托管替代评估见 [更新访问与托管评估](update-access-and-hosting.md)。
上期所和能源中心合约元数据请求采用有限次数 HTTP 重试；耗尽后用 Chrome 读取同一官方 URL。
两条路径都保留响应原始字节，并执行相同的日期、内容和 SHA-256 校验；浏览器请求失败仍停止发布。

脚本采集六所缺失区间，保留逐份原件和 SHA，按新增交易日抓取 SHFE/INE ContractBaseInfo，
用原有官方规则再生生命周期，然后严格生成规格元数据和短区间 `rebuild-collected` 库。
`update-collected --db SEED --delta DELTA --out NEW` 合并到新维护副本，保留旧历史及公告状态，
冲突、缺失元数据、已观测且未到期合约在后续交易日缺行都会硬失败；首次观测前的未知区间继续保留。
随后执行 `verify-collected`、`export`、`compact-seed`，生成新的
SQLite gzip 和下一次运行所需的单包 bootstrap。GitHub Pages 站点总量限制为 1 GB；当前完整发布约 52.5 MiB。

既有 `user_assumed_holiday_calendar`、`assumed_lifecycle` 和 `official_only=false` 标签继续保留。
只有 bootstrap 显式记录既有授权时才启用 `--assume-2027-holidays`；不会自动新增年份假期。
当官方日历尚不足以生成新合约生命周期时，更新失败，待补充原件后重跑。

`scan-announcements` 是补充审计，待复核候选保留在维护库及 `pending-announcements.json`，
不会因官方日参数更新被标记为已解决，也不使用旧二十四小时公告门槛阻断官方日更。
公告扫描失败会留下显式审计错误报告，既有待复核队列继续保留；不会阻断已验证的官方日参数更新。工作流以 concurrency 串行恢复最新线上 seed，避免并发旧运行覆盖新历史。
所有运行（包括失败）上传诊断、原始输入、元数据缺口/来源报告和覆盖报告，保留三天；DCE 浏览器在 Linux
上由 Chrome + `xvfb-run` 运行。后续源码改动仍须独立验证发布结果，当前验收记录见上文。

手动验证同一流程，不触发部署：

```bash
xvfb-run -a python3 scripts/update_official.py \
  --db data/future-meta.sqlite --inputs data/official-inputs.json \
  --work-dir /tmp/future-meta-official-update --public-dir /tmp/future-meta-pages
```

每次使用新的 work-dir。失败诊断位于其 `diagnostics/`；原始输入库保持只读。
`coverage.json` 保留未知观察区间和推算到期日计数，逐条来源一致不等于自上市日起完整覆盖。

当前 schema 4 完整导出规格历史，客户端可查询历史乘数和最小变动价位。旧审阅库可显式运行已审阅的交易所规格迁移；官方采集重建库的规格来源应通过采集流程维护：

```bash
cargo run -p future-meta-daemon -- migrate-contract-specs \
  --db path/to/review-copy.sqlite
```

当前内置迁移覆盖 DCE 棕榈油/豆油、GFEX 碳酸锂和 INE 集运欧线的官方调整，并逐合约保留有效期和公告 URL。该兼容维护命令不替代 `collect_contract_specs.py` 的完整来源与覆盖校验。

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

当前采集重建库按原件观测范围发布，不沿用旧库的 2010–2019 历史或严格完整声明。
发布前验证采集关联并公开覆盖报告：

```bash
cargo run -p future-meta-daemon -- verify-collected \
  --db path/to/review-copy.sqlite --out public/coverage.json
cargo run -p future-meta-daemon -- export \
  --db path/to/review-copy.sqlite --out public
gzip -c path/to/review-copy.sqlite > public/ops/future-meta.sqlite.gz
```

客户端 archive 仍为 schema 4，SDK v0.6.0 不变。每次刷新读取固定 Pages manifest，按其中的版本化 Release URL 与 SHA 获取数据；兼容格式的每日数据更新不要求升级 SDK。格式说明见 [archive 格式说明](archive-format.md)。本地导出不会自动发布。

`export` 以只读方式打开数据库，并拒绝孤儿 fee evidence、倒置观察时间、非法或重叠
区间、以及早于合约上市日的费率版本。只有这些检查通过后，才可在取得发布授权后部署
`public/`。

## 维护库的无损空间整理

维护库对证据文档和重建溯源 JSON 去重，保留全部历史、日观测和官方来源。
使用当前 daemon 显式生成迁移副本：

```bash
cargo run -p future-meta-daemon -- compact-seed \
  --db path/to/downloaded-seed.sqlite --out path/to/compacted-seed.sqlite
```

输入只读，输出必须不存在。命令在临时副本中共享重复的溯源 JSON 属性值，
将三类证据的 URL、SHA 保存为共享文档记录。原表名保留为兼容视图，JSON 读取结果
逐字节保留；证据视图支持插入、更新和删除，实际记录表使用 `WITHOUT ROWID`。
命令双向核对记录内容和数量，验证历史、外键及 SQLite 完整性，再生成目标文件；
重复执行支持已精简的库。日常恢复 seed 不自动迁移或反复整理空间。

维护精简库需要包含证据视图写入兼容改动的 daemon；SQLite UPSERT 不能直接以视图
为目标，维护代码已使用事务内兼容写法。客户端 archive 格式和下载接口不变。

发布前还需与下载的生产库核对所有表及查询结果，并比较 gzip 后的体积。
仅改变物理布局时，沿用线上客户端 manifest 与 artifact，更新 `ops/future-meta.sqlite.gz`。

## GitHub Pages 发布权限

仓库 Settings → Pages 的发布源设为 GitHub Actions。部署使用 `actions/configure-pages@v5`、`actions/upload-pages-artifact@v4` 和 `actions/deploy-pages@v4`。
发布 job 绑定 `github-pages` environment，使用内置 `GITHUB_TOKEN` 的 `contents: write`（数据 Release）、`pages: write`、`id-token: write`；checkout 不持久化凭据，无外部云 secrets。

先创建带专用标记的数据 Release 并上传客户端 archive，发布后从公开附件 URL 读回验证 SHA；完成后才准备 Pages manifest、coverage、维护 seed/bootstrap 和发布记录。附件失败时不切换 Pages。

同一次 Pages 部署切换 manifest 与维护输入。部署后仅在线上 manifest 和发布记录都匹配时清理旧版本，保留当前数据 Release 与实际被替换的一版；SDK Release 不参与清理。发布记录使失败后的重跑仍能保留正确的上一版。生成目录不提交到 Git 历史。

手动重发当前已核验数据（包括周末），不访问交易所：

```bash
gh workflow run update-fee-data.yml --ref main -f republish_current=true
```

普通运行与定时运行继续使用原离线门禁。重发入口只恢复 Pages 现有输入并本地导出，采集步骤明确跳过。

## 数据安全边界

- 不提交 `data/future-meta.sqlite`。
- 不提交原始证据文件或 `data/official-evidence.sqlite`。
- 不提交 `public/` 生成物。
- 不存储或发布价格、涨跌停、每手保证金、每跳盈亏、开平合计手续费等派生字段。
- 官方参数中的期权和 DCE 月均价 F 合约不属于当前普通期货重建范围，不宽松转换为普通合约。
- 新合约缺少官方生命周期、规格或参数时保留诊断并失败，不能从旧 SQLite、相邻合约或品种默认值补齐，也不能静默跳过。

## Client URL

Default manifest URL:

`https://zynthium.github.io/future-meta/manifest.json`
