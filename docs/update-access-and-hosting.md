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

## GitHub 托管

发布与维护输入统一使用 GitHub Pages，基址为 `https://zynthium.github.io/future-meta/`。客户端只读取 manifest 和 archive；更新 workflow 恢复同一站点的 SQLite seed 与官方输入单包。站点通过 GitHub Actions 发布，不把数据提交进 Git 历史。

发布 job 使用官方 `configure-pages`、`upload-pages-artifact`、`deploy-pages` actions，以及内置 `GITHUB_TOKEN` 的 `pages: write`、`id-token: write` 权限。仓库 environment 为 `github-pages`，没有外部云凭据。

官方输入仅使用 schema 2 传输索引，指向 `official-inputs.tar.gz`，校验大小、整体 SHA、逐份原件 SHA 和截止日。旧托管地址、分片索引及回退下载均不保留。

当前完整发布约 52.5 MiB，满足 GitHub Pages 的 1 GB 站点上限；月带宽软限制为 100 GB，见 [Pages 配额](https://docs.github.com/en/pages/getting-started-with-github-pages/github-pages-limits)。部署方式见 [GitHub Pages 自定义 workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages)。

GitHub Releases 继续表示 SDK 版本，不用于日更数据的 latest 指针。Actions artifacts 仅用于诊断留存，不作为永久客户端下载端点。

0.6.0 的客户端下载默认地址和 exporter mirror 均已切换；旧地址停止服务，使用方直接升级当前代码。当前部署验收记录见 [部署说明](deployment.md#验证与上线状态)。
