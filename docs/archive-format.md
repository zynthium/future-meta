# Archive schema 4 与 0.5.0 迁移

0.5.0 客户端对应 Git tag `v0.5.0`。daemon 默认导出 schema 4；使用方应先升级客户端，再加载 schema 4 artifact。线上版本以 manifest 为准。

## 传输格式与查询模型

schema 4 继续使用 bincode standard 配置和 zstd 等级 19。传输记录改为：

1. 格式版本、生成时间、历史起止时间。
2. 按字面值排序且不重复的日期/时间文本字典，保留原始精度、时区和格式。
3. 合约元数据；只有 TqSdk `symbol` 是合约字符串标识，静态规格和日期使用字典引用。
4. 规格数值字典及完整合约规格版本引用。
5. 手续费、多空保证金和交易状态/来源时间字典。
6. 按合约保存的联合参数变更序列：起点字典索引增量，以及各参数字典引用。
7. 每个合约最后一个结束日期；中间结束日期与下一起点不同时，使用显式覆盖保留真实空档和未知结束值。
8. 非规范输入顺序的可选排列映射，确保公开编码 API 不改变调用方记录顺序；普通 daemon 导出为空。

不会根据品种众数补值、删除历史或合约例外、合并未知值与零，也不保存可由 symbol 派生的品种代码。规则哈希、`raw_text`、四项手续费、双向保证金、状态及所有规格边界保持原值。数值字典按序列化位模式去重。

解码后仍返回公开 `FeeArchiveV3`，其 `schema_version` 归一为当前版本 4。类型名保留用于源码兼容；它表示物化查询模型，不代表当前传输格式仍为 schema 3。`FutureMeta`、handle、as-of、预编译手续费和 cursor API 均不改变。加载后拥有所有权的数据模型仍相同，本次不声称减少常驻堆内存。

## 编解码兼容

- `decode_archive_bytes` 接受 schema 1/2/3/4，返回当前物化模型。
- schema 1/2 的日内开仓费仍为 `None`；schema 1 的规格历史只能由原静态规格转换，不能恢复原本没有的数据。
- `encode_archive_bytes` 对当前版本 4 的物化模型执行紧凑编码；显式使用 schema 1/2/3 标记的合法旧模型仍按原布局编码。
- `FutureMeta::from_archive` 也接受手工构造、标记为 3 的现有 `FeeArchiveV3`，加载时归一版本字段。
- 0.4.0 客户端不能读取 schema 4；manifest 的版本检查不应被绕过。
- 解码校验所有引用、已知/重复合约、日期字典排序、增量溢出、结束覆盖及顺序排列，非法输入返回 `CorruptArchive`；尾随 bincode 字节继续拒绝。

schema 4 的私有字段顺序定义在 `compact_archive.rs`。后续改变该顺序或编码含义必须升级格式，不能直接复用版本 4。

## 导出与下载

daemon 的 manifest 和版本 artifact 路径均使用 `SCHEMA_VERSION`，例如 `artifacts/future-meta-fees-v4-<version>.fmeta.zst`。稳定下载入口仍为 `manifest.json` 和 `latest.fmeta.zst`。Actions 已使用 manifest 中的版本生成文件名，无需另设固定 schema 号。

下载器接受已支持的 manifest 版本，按原 SHA-256 校验并更新缓存。旧缓存的 schema 3 在新客户端中仍可加载；线上切换后 SHA-256 改变时自动重新下载。不要求用户删除缓存，也不下载维护用 SQLite。

## 本地升级和核对

```sh
cargo run --release -p future-meta --example archive_upgrade -- \
  old.fmeta.zst upgraded.fmeta.zst
```

输出必须不存在。该工具使用正式编解码器，核对全部物化字段，并执行实际 as-of 查询；它只升级本地客户端文件，不改变历史来源的权威性，也不会发布文件。

正式导出仍使用已有命令：

```sh
cargo run --release -p future-meta-daemon -- export \
  --db data/future-meta.sqlite --out data/archive-v4/public
```

可将导出文件作为第三个参数传给 `archive_upgrade`，比较两份数据；仅忽略导出时间差。在线 smoke example 支持可选 manifest URL 与缓存目录参数，便于在本地 HTTP 服务验证下载和缓存链路。

维护库没有因为此格式迁移而修改。客户端体积收益以实际 artifact 为准，不能用 SQLite 压缩比例代替。
