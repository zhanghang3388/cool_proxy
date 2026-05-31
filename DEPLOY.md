# VPS 部署与更新说明

本文档按当前服务器部署方式编写：项目目录为 `/opt/cool_proxy`，后端用 `nohup` 直接运行 release 二进制，前端用 `npm run build` 生成静态文件。

## 1. 更新代码

```bash
cd /opt/cool_proxy
git pull
```

如果服务器上有本地修改，先执行：

```bash
git status
```

确认没有需要保留的改动后再继续。不要在不确定的情况下执行 `git reset --hard`。

## 2. 编译后端

```bash
cd /opt/cool_proxy/backend
cargo build --release
```

编译过程中如果只看到 `unused import`、`dead_code` 之类 warning，一般不影响运行。看到 `Finished release profile` 表示编译成功。

## 3. 重启后端

当前启动方式：

```bash
nohup ./target/release/cool_proxy config.yaml > /tmp/cool_proxy.log 2>&1 &
```

更新后重启：

```bash
cd /opt/cool_proxy/backend
pkill -f target/release/cool_proxy
nohup ./target/release/cool_proxy config.yaml > /tmp/cool_proxy.log 2>&1 &
```

检查进程：

```bash
pgrep -af cool_proxy
```

查看日志：

```bash
tail -f /tmp/cool_proxy.log
```

检查健康状态：

```bash
curl http://127.0.0.1:8317/healthz
```

返回 `ok` 表示后端已启动。

## 4. 构建前端

```bash
cd /opt/cool_proxy/frontend
npm run build
```

构建成功后产物在：

```text
/opt/cool_proxy/frontend/dist
```

Vite 提示 `Some chunks are larger than 800 kB` 只是体积警告，不影响部署。

## 5. 发布前端静态文件

如果 nginx 指向的是某个静态目录，需要把 `dist/` 同步过去。例如站点目录是 `/var/www/cool_proxy`：

```bash
rsync -a --delete /opt/cool_proxy/frontend/dist/ /var/www/cool_proxy/
nginx -s reload
```

如果你的 nginx 直接指向 `/opt/cool_proxy/frontend/dist`，则构建完成后通常只需要刷新浏览器即可。

## 6. 完整更新命令

下面是一套常用完整流程：

```bash
cd /opt/cool_proxy
git pull

cd /opt/cool_proxy/backend
cargo build --release
pkill -f target/release/cool_proxy
nohup ./target/release/cool_proxy config.yaml > /tmp/cool_proxy.log 2>&1 &
curl http://127.0.0.1:8317/healthz

cd /opt/cool_proxy/frontend
npm run build

# 如果 nginx 没有直接指向 frontend/dist，按实际目录同步：
# rsync -a --delete /opt/cool_proxy/frontend/dist/ /var/www/cool_proxy/
# nginx -s reload
```

## 7. 额度查询排查

额度查询会使用账号导入时分配的代理请求上游。如果页面显示查询失败，先看接口返回里的 `error` 字段，再看后端日志：

```bash
tail -n 200 /tmp/cool_proxy.log
```

常见方向：

- `dns error` / `connection refused` / `timed out`：VPS 或账号代理无法访问 `chatgpt.com`。
- `proxy` 相关错误：账号绑定的代理不可用或格式不正确。
- `401` / `403`：账号 token 失效、账号权限异常，先尝试刷新 token。
- `quota response missing primary/secondary windows`：上游返回结构变化，需要更新解析逻辑。

单独检查某个账号的额度接口：

```bash
curl -X POST \
  -H "Authorization: Bearer <你的 admin_token>" \
  http://127.0.0.1:8317/api/accounts/<账号ID>/quota
```

批量刷新当前页额度由前端自动触发；也可以手动调用：

```bash
curl -X POST \
  -H "Authorization: Bearer <你的 admin_token>" \
  -H "Content-Type: application/json" \
  -d '{"ids":["<账号ID1>","<账号ID2>"]}' \
  http://127.0.0.1:8317/api/accounts/quota/refresh
```

