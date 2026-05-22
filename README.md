# Cool Proxy

OpenAI Codex / ChatGPT 账号轮询代理。Rust 后端 + Vue 管理面板，账号文件兼容 [CLIProxyAPI](https://github.com/router-for-me/CLIProxyAPI) 的 `codex-*.json`。

## 功能

- 多账号 round-robin 轮询，失败自动冷却 + 重试
- OAuth refresh_token 自动续期（后台每 60s 扫描，到期前 5 分钟刷新）
- OpenAI 兼容反代（`/v1/*` 透传到 `https://chatgpt.com/backend-api/codex`）
- 流式 / 非流式响应都直接管道，不缓冲
- Vue 管理面板：账号上传 / 启用禁用 / 手动刷新 / 清除冷却 / 实时统计 / 请求日志

## 目录

```
backend/   Rust + axum 后端
frontend/  Vue 3 + Vite + naive-ui 管理面板
```

## 快速上手

### 1. 后端

```bash
cd backend
cp config.example.yaml config.yaml
# 改 config.yaml 里的 admin_token 和 api_keys
cargo run --release
# 默认监听 0.0.0.0:8317
```

把已有的 `codex-*.json` 认证文件拷到 `backend/auths/`，后端启动后会自动加载（也可以等会儿在 UI 里上传）。

### 2. 前端（开发模式）

```bash
cd frontend
npm install
npm run dev
# 浏览器打开 http://localhost:5173
```

Vite 会把 `/api` 请求代理到 `http://localhost:8317`，零配置开发。

### 3. 前端（生产构建）

```bash
cd frontend
npm run build
# 产物在 frontend/dist/，nginx 或任意静态服务托管
```

## 客户端接入

```bash
export OPENAI_BASE_URL=http://<你的服务器>:8317/v1
export OPENAI_API_KEY=<config.yaml 里 api_keys 任意一个>

curl $OPENAI_BASE_URL/chat/completions \
  -H "Authorization: Bearer $OPENAI_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"gpt-5","messages":[{"role":"user","content":"hi"}]}'
```

## 管理面板

打开前端地址，输入 `config.yaml` 里的 `admin_token` 登录。

- **账号管理**：上传 / 启用禁用 / 手动刷新 token / 清除冷却 / 删除
- **请求日志**：最近 500 条请求（内存环形缓冲，重启丢失）
- **设置**：查看运行时配置和接入示例

## 配置说明

详见 `backend/config.example.yaml`。关键项：

| 字段 | 含义 |
| --- | --- |
| `api_keys` | 客户端调用 `/v1/*` 时使用的密钥（任选一个即可） |
| `admin_token` | 管理面板登录令牌 |
| `auth_dir` | 认证文件存放目录，扫描 `codex-*.json` |
| `retry.cooldown_seconds` | 单次失败冷却时长 |
| `retry.long_cooldown_seconds` | 连续失败 N 次后的长冷却 |
| `retry.failure_threshold` | 触发长冷却的连续失败阈值 |
| `token_refresh.scan_interval_seconds` | token 刷新扫描间隔 |
| `token_refresh.refresh_before_expire_seconds` | 提前多少秒刷新 |

## 安全注意

- `admin_token` 和 `api_keys` 不要泄露
- `auths/` 下是 OAuth 凭据，权限相当于账号本身
- 默认监听 `0.0.0.0`，生产环境建议放在反向代理后只允许内网访问，或用 TLS

## 许可

MIT
