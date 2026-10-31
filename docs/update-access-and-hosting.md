# 更新访问与 GitHub 托管评估

## 交易所访问

原流程每天（包括周末）触发，并先联网读取 CFFEX 日历才判断无交易日；缺少节后早盘入口。自动补采还继承了历史采集工具的十线程默认值。

现在按北京时间设置两个工作日候选时刻，先在独立 admission job 中读取本地官方日历，再决定是否启动更新：

| 场景 | 18:45 候选 | 08:05 候选 |
| --- | --- | --- |
| 正常工作日 | 当晚有夜盘则运行 | 跳过 |
| 正常周五、周一 | 周五夜盘正常运行 | 普通周一早晨跳过 |
| 节前最后交易日 | 无夜盘，跳过 | 跳过 |
| 休市及周末 | 不采集；周末没有 cron | 不采集；周末没有 cron |
| 法定休市后首个交易日 | 当晚正常夜盘可运行 | 补跑节前尚未采集的已完成交易日 |

这里的假期指交易所工作日休市，不能把政府调休上班的周六、周日当作交易日。节后早晨和当晚属于两个不同开盘前时段，各自只补维护库尚缺的完成交易日。

`scripts/update_schedule.py` 只依赖 Python 标准库，不导入采集器，不请求任何服务。准入时段为 18:30–20:00 和 08:00–08:20；`update_official.py --scheduled` 在执行前重查，更新 job 限时 30 分钟，迟到直接跳过。GitHub 的 cron 可能延迟或丢失运行，不能把上述时刻当作开盘前必定完成的 SLA，见 [GitHub schedule 文档](https://docs.github.com/en/actions/reference/workflows-and-actions/events-that-trigger-workflows#schedule)。

`config/update-calendar.json` 的 2026 休市日由已保留的十二个月 CFFEX 官方原件核验，来源 URL、SHA 均记录在配置中。新年度必须由官方原件补充；缺失年份报错停止，不使用已授权的生命周期假期假设来推测实际开市。

采集阶段仅请求缺失交易日，`--workers 1` 串行执行；复用 bootstrap 中 SHA 已核验的月历，只有缺失月份才获取新月历。普通 HTTP 最多五次尝试，CBI 耗尽后仅一次 Chrome 导航；没有定时轮询或无限重试。一次采集仍需多个官方文件以及 DCE 页面资源，不能把“一轮更新”说成“一个 HTTP 请求”。

日参数按已完成交易日归档。节后早晨补跑不能证明当天尚未发布的结算参数、或下一个交易时段的费率已核验；这些需要对应官方发布证据。

## Release 与 Pages 分工

客户端 archive 作为独立 `data-<UTC时间>-<SHA前缀>` Release 的附件，使用固定版本 URL。Pages 基址 `https://zynthium.github.io/future-meta/` 保留 `manifest.json`、`coverage.json`，以及 `ops/` 下的 seed、官方输入索引/归档和发布记录；不保留 archive 副本或历史 artifact 目录。

SDK v0.6.0 继续读取同一个 Pages manifest，并跟随其中的 `mirrors` 下载 Release 附件、校验 SHA 和缓存。SDK 版本与每日数据版本独立；数据 Release 设置 `--latest=false`，SDK Release 保持 Latest。

发布顺序为：验证本地输入，上传并从公开 URL 校验 Release 附件，然后通过一次 Pages 部署切换 manifest 与匹配的维护输入。部署后核验生产 manifest 和 `ops/publication.json`，只清理专用标记的数据 Release，保留当前及被替换的一版。该记录用于准确重跑，不按 Release 创建时间猜上一版，也不删除 SDK Release。

发布 job 使用内置 `GITHUB_TOKEN` 的 `contents: write`、`pages: write`、`id-token: write`，checkout 不保存凭据。原始暂存目录为 `public/`；`scripts/publish_data.py prepare` 会真实创建 Release，并生成仅含 Pages 资产的 `data/pages-site/`。

手动 `republish_current=true` 仅重新导出已有 Pages seed 并发布，不启动采集器；正常运行保持上文的交易所访问门禁。`ops/` 是公开但仅供维护使用的静态文件，不是私有存储。

官方输入继续使用 schema 2 单包及完整 SHA 校验。维护资产约 52 MiB，客户端附件约 135 KiB，满足 [Pages 配额](https://docs.github.com/en/pages/getting-started-with-github-pages/github-pages-limits)。Pages 使用 [官方自定义 workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages)，不提交生成数据；Actions artifacts 只保存诊断。
