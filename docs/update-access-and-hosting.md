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

## Cloudflare 功能能否由 GitHub 替代

可以。当前 Cloudflare 仅做静态文件分发，没有 Workers、R2、D1 或动态 API。GitHub 仓库公开，目前尚未启用 GitHub Pages；现有完整发布包约 52.5 MiB。

| 现有用途 | GitHub 选择 | 适用性 |
| --- | --- | --- |
| manifest、客户端 archive、覆盖报告 | GitHub Pages | 可保持现有相对路径和静态读取模式 |
| SQLite gzip、官方元数据 bootstrap | GitHub Pages | 当前整包大小满足配额，可与客户端文件在同次部署发布 |
| 更大的二进制维护输入 | GitHub Releases | 支持单文件小于 2 GiB；需设计完整版本发布和指针切换 |
| 诊断留存 | Actions artifacts | 适合限期诊断；有保留期及读取权限要求，不作为公开客户端长期下载地址 |

GitHub Pages 的已发布站点上限为 1 GB，月带宽软限制 100 GB；自定义 Actions 部署不受每小时十次构建的软限制。当前体量足够，见 [Pages 配额](https://docs.github.com/en/pages/getting-started-with-github-pages/github-pages-limits)。使用 [GitHub Pages 自定义 workflow](https://docs.github.com/en/pages/getting-started-with-github-pages/using-custom-workflows-with-github-pages) 即可部署 `public/`，不必把生成数据提交到 Git 历史。

Releases 的文件配额见 [官方说明](https://docs.github.com/en/repositories/releasing-projects-on-github/about-releases#storage-and-bandwidth-quotas)。本仓库已有 SDK Release `v0.5.0`，数据 Release 若占用 `latest`，会混淆 SDK 最新版本入口；不能直接把客户端依赖到通用 `/releases/latest/download/` 而忽略这一冲突。[Actions artifacts](https://docs.github.com/en/actions/how-tos/manage-workflow-runs/download-workflow-artifacts) 也不能充当永久匿名 CDN。

推荐迁移方向是 GitHub Pages，预计基址为 `https://zynthium.github.io/future-meta/`。迁移需要启用 Pages、改部署 action/权限、改 seed/bootstrap 恢复地址、客户端下载默认 manifest URL，以及 exporter 写入的 artifact mirror。archive schema 和查询 API 无须因此改变。

旧版客户端的默认地址固定为 `future-meta.pages.dev`，立即撤销 Cloudflare 会中断它们。应先发布 GitHub 镜像并实测文件、下载耗时及网络可达性，再发布客户端地址变更，安排旧地址兼容期。当前只完成可行性评估，没有迁移生产域名或删除 Cloudflare secrets。
