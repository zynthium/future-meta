# future-meta Cron Dispatcher (Cloudflare Worker)

用于替代受 GitHub Actions 高峰期排队延迟影响的内置 cron 定时器，在官方准入时间点准时触发 `update-fee-data.yml` 采集工作流。

## 定时计划（Asia/Shanghai 时间）

| 表达式 (UTC) | 北京时间 | 业务作用 |
| :--- | :--- | :--- |
| `50 10 * * 1-5` | 18:50 | 夜盘前日度参数采集主窗口 |
| `25 11 * * 1-5` | 19:25 | 夜盘前日度参数采集兜底窗口 |
| `5 0 * * 1-5` | 08:05 | 法定节假日休市后首个早晨准入窗口 |
| `17 12 * * 1-5` | 20:17 | 夜盘前只读健康检查 |

## 配置与部署

```bash
cd infra/cron-dispatcher

# 1. 写入 GitHub Token 密钥 (具有 repo 及 workflow 权限)
echo "<YOUR_GITHUB_TOKEN>" | wrangler secret put GH_TOKEN

# 2. (可选) 写入 HTTP 触发密钥
echo "<CUSTOM_SECRET>" | wrangler secret put DISPATCH_SECRET

# 3. 部署
wrangler deploy
```

## HTTP 测试接口

部署完成后，支持通过 HTTP 端点进行状态检查与烟测触发：

- `GET /`：查看服务运行状态与定时计划
- `POST /dispatch?smoke=true` 或 `GET /dispatch?smoke=true`：触发一次离线烟测（合成数据，不触碰交易所）
