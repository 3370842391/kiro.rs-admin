# 8991 HTTPS 测试入口

公开测试实例通过以下入口访问：

- API Base URL：`https://rs-test.43-225-196-10.sslip.io`
- 管理端：`https://rs-test.43-225-196-10.sslip.io/admin`
- 后端：`http://127.0.0.1:8991`

该入口只代理隔离的 `kiro-rs-test` 容器，不修改生产 `kiro-rs-admin`（8990）。

目标服务器当前使用 Nginx 1.30.3；最终配置中的 `http2 on;` 要求 Nginx 1.25.1 或更高版本。部署前必须先执行：

```bash
/www/server/nginx/sbin/nginx -v
```

## 首次部署

服务器使用宝塔安装的 Nginx：

```bash
/www/server/nginx/sbin/nginx -c /www/server/nginx/conf/nginx.conf
```

1. 确认域名解析：

```bash
getent ahostsv4 rs-test.43-225-196-10.sslip.io
```

2. 创建 ACME webroot，并先部署仅包含 80 端口和 challenge location 的临时 vhost。bootstrap 配置对普通 HTTP 请求返回 404，不允许在证书签发窗口通过明文入口调用 API。

3. 使用 Debian Certbot 的 webroot 模式签发证书：

```bash
certbot certonly --webroot \
  --webroot-path /www/wwwroot/rs-test.43-225-196-10.sslip.io \
  --domain rs-test.43-225-196-10.sslip.io \
  --non-interactive --agree-tos --register-unsafely-without-email
```

4. 把 `rs-test.43-225-196-10.sslip.io.conf` 复制到：

```text
/www/server/panel/vhost/nginx/rs-test.43-225-196-10.sslip.io.conf
```

5. 校验并热加载：

```bash
/www/server/nginx/sbin/nginx -t -c /www/server/nginx/conf/nginx.conf
/www/server/nginx/sbin/nginx -s reload -c /www/server/nginx/conf/nginx.conf
```

6. 安装续期后的宝塔 Nginx 热加载钩子，并确认定时器已启用：

```bash
install -m 0755 reload-bt-nginx.sh \
  /etc/letsencrypt/renewal-hooks/deploy/reload-bt-nginx.sh
systemctl enable --now certbot.timer
systemctl is-enabled certbot.timer
systemctl is-active certbot.timer
certbot renew --dry-run
/etc/letsencrypt/renewal-hooks/deploy/reload-bt-nginx.sh
```

当前服务器的 Certbot 2.1 在 dry-run 时不会执行 deploy hook，所以续期模拟后需要单独执行钩子。钩子会先执行宝塔 Nginx 的配置校验，成功后再热加载，避免证书 symlink 更新后 worker 仍继续提供旧证书。该测试域名使用无邮箱 ACME 账户，因此还需监控 `certbot.timer` 和 `/var/log/letsencrypt/letsencrypt.log`，不能依赖到期邮件提醒。

## 验证

```bash
curl -fsS https://rs-test.43-225-196-10.sslip.io/admin >/dev/null
curl -sS -o /dev/null -w '%{http_code}\n' \
  https://rs-test.43-225-196-10.sslip.io/v1/models
```

预期管理端为 200，未认证模型列表为 401。认证测试使用服务器测试配置中的系统 Key，但不得在命令输出或日志中打印该 Key。

使用临时环境变量验证 SSE 事件完整性；命令完成后立即清理变量：

```bash
curl -fsS -N \
  -H "x-api-key: ${TEST_API_KEY}" \
  -H 'anthropic-version: 2023-06-01' \
  -H 'content-type: application/json' \
  --data '{"model":"auto","max_tokens":32,"stream":true,"messages":[{"role":"user","content":"Reply with exactly STREAM_OK"}]}' \
  https://rs-test.43-225-196-10.sslip.io/v1/messages
unset TEST_API_KEY
```

响应必须依次包含 `message_start`、文本 `content_block_delta`、`message_delta` 和 `message_stop`，且首个事件不应被代理缓冲到请求结束后才一次性返回。

代理使用标准 combined access log，只记录请求元数据，不记录 Authorization、`x-api-key` 或请求 body。SSE 路径关闭响应和请求缓冲，读写超时为 300 秒。

## 回滚

部署前必须为原 vhost（如果存在）创建不会被 `*.conf` include 加载、且带 UTC 时间戳的备份：

```bash
vhost=/www/server/panel/vhost/nginx/rs-test.43-225-196-10.sslip.io.conf
cp -a "${vhost}" "${vhost}.bak.$(date -u +%Y%m%dT%H%M%SZ)"
```

回滚时恢复指定 `.bak.<UTC>` 文件；若此前不存在配置，则移除新 vhost。随后必须执行 Nginx `-t`、热加载，并重新检查测试域名与生产 8990 的健康状态。撤销入口时，证书、私钥和 renewal 配置应按服务器密钥保留策略处理，不能把私钥当作普通审计材料长期散落保存。
