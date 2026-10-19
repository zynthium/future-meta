# 从官方采集输出重建 SQLite

`rebuild-collected` 只接受采集输出、来源账本和保留原件；不接受旧 SQLite 作为输入。输出文件必须不存在，完成校验后才从临时文件重命名。

## 生成可追溯的结算输入

```sh
python3 scripts/collect_settle_params.py --offline \
  --start-date 2020-01-01 --end-date 2026-10-15 \
  --output-dir data/rebuild-20261018/settle_params \
  --raw-dir data/settle_params/raw --evidence-dir data/official-evidence \
  --futures-only --forward-fill-cffex \
  --provenance-dir data/rebuild-20261018/evidence
```

采集器保留实际解析的官方响应字节、URL、SHA-256、报告日期及输出 CSV 摘要。CFFEX 的日表引用对应日期之前最近的完整变更快照；INE 分别引用共同手续费源和独立保证金源。原始缓存缺失或摘要不符时中止，不补旧 CSV。

合约元数据及规格使用 `collect_contract_specs.py` 的当前输出。调用者应先完成其来源和覆盖复核。用户授权的2027年推算到期日保留 `calendar_assumption`，不作为交易所确认日期。

## 新建数据库

```sh
cargo run --release -p future-meta-daemon -- rebuild-collected \
  --metadata-dir data --settle-dir data/rebuild-20261018/settle_params \
  --settlement-sources data/rebuild-20261018/evidence/settlement_sources.json \
  --out data/rebuilt-20261018.sqlite
```

导入投机多空保证金率、普通开仓、平昨、平今、日内开仓费。任一已发布费率或投机保证金变化都会创建新版本，价格变化不创建版本。零值与未知值区分；来源未提供日内开仓费时保留 `None`，不擅自等同普通开仓费。费率单位沿用采集 CSV：金额元/手、比例万分比、保证金百分点。

`official_observations` 逐交易日关联版本、输入文件行号、手续费与保证金原件；`rebuild_inputs`、`retained_parameter_documents` 记录保留文件摘要。套保率等额外原始字段仍可由该原件及输入行定位，不冒充客户端已有的投机费率字段。

`rebuild_contract_provenance` 保留每字段来源和推算假设；`rebuild_spec_provenance` 保留完整原始规格行，包含结算阶段、夜盘实际瞬间和适用交易日。明确的瞬间保持原时间；只有“结算后”且无精确时间的规格边界，按原件给出的下一适用交易日进入日度查询索引，不伪造结算钟点。原阶段可从来源表读取。

历史费率按官方参数的交易日观察构建，以北京时间当日零点作为日度索引，不声称恢复了未提供的盘中调整时刻。已到期版本截止于到期日后一天；在采集截止日仍存续的合约保留最新开放版本，后续新数据才关闭该版本。数据的实际观测截止日记录在 `rebuild_summary`。

输出通过 SQLite 完整性、外键约束、版本区间及原件关联检查后才写入目标路径。构建成功不自动替换原库，也不部署。

## 客户端 archive schema 4（0.5.0 待发布）

schema 4 采用共享字典、合约变更序列和日期引用增量编码，解码后仍使用 `FeeArchiveV3`，不改变查询 API。新客户端兼容 schema 1/2/3，旧客户端不能读取 schema 4；详见 [格式迁移说明](archive-format.md)。

schema 3 在 `ContractFee` 中增加 `intraday_open_fee: Option<FeeSpec>`；多空保证金继续使用原字段。`PreparedFee::intraday_open_amount` 返回 `Option<f64>`，未知费率不返回假零值。

新客户端仍能读取 schema 1/2/3；schema 1/2 旧档案的日内开仓费为 `None`。旧载荷类型保留为 `FeeArchiveV1` / `FeeArchiveV2`，当前物化数据模型为 `FeeArchiveV3`；旧费行类型为 `LegacyContractFee`。源码中手工构造当前 `ContractFee` 的调用方需补充新字段。

发布 schema 4 artifact 前需将使用方升级至 0.5.0；0.4.0 不能读取 schema 4。本次本地重建不会自动发布。

```sh
cargo run -p future-meta-daemon -- export --db data/rebuilt-20261018.sqlite --out data/rebuild-20261018/public
cargo run -p future-meta --example inspect_archive -- data/rebuild-20261018/public/latest.fmeta.zst DCE.bb2001 2020-01-02T10:00:00+08:00
```

普通开发的构建检查不需要 Python；Python 只用于已有的离线采集和数据审计步骤。

## 发布校验及覆盖边界

```sh
cargo run -p future-meta-daemon -- verify-collected --db data/rebuilt-20261018.sqlite --out data/rebuild-20261018/public/coverage.json
```

0.4.0 发布 schema 3。工作流验证采集库关联，任何失败均不恢复 V18 旧基线。
2026-10-18 发布含 1,199,203 条观测、47,055 个参数版本；16 个合约在上市日至首条
官方参数观测前留有空档，147 个合约无日记录。2020-01-01 为休市日，首条观测为
2020-01-02。原严格覆盖审计仍可报告缺口；本发布不宣称所有合约自上市起完整覆盖。
