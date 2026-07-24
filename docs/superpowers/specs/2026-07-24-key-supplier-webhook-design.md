# Key 供应与 Webhook 自动导入设计

## 目标

在现有 Kiro RS 管理端中接入第三方 Key 供应 API。系统既能在收到
`new_keys_available` webhook 后自动购买并导入 `ksk_` 凭据，也能由管理员手动购买；
所有 webhook、订单和导入结果均可在管理后台查询。域名、DNS 和服务器反向代理属于
最后部署阶段，不阻塞本地功能开发。

## 已知供应商协议

- 所有出站请求使用 `X-API-Key`；JSON 请求使用 `Content-Type: application/json`。
- `GET /api/my/profile` 返回账号、额度和当前 webhook URL。
- `GET /api/my/stock` 返回当前最大可提取数量。
- `GET /api/status` 返回供应系统状态。
- `POST /api/my/purchase` 使用 32 位十六进制 `client_order_id` 保证幂等。
- `PUT /api/my/webhook` 保存回调 URL，`POST /api/my/webhook/test` 发送测试事件。
- `new_keys_available` 携带稳定的 `event_id`、`purchase_order_id` 和 `new_keys`。
- `all_keys_dead` 携带稳定的 `event_id` 和 `dead`。

## 方案选择

采用 RS 进程内的独立 `key_supplier` 模块。模块持有供应 API 客户端、SQLite 事件存储和
现有 `MultiTokenManager`，但不把供应状态混入凭据或 trace 数据。相比直接写入 Admin
handler，该边界便于独立测试、去重和重试；相比外置 worker，它不增加部署单元。

## 配置

以下字段写入现有 `config.json`，敏感字段通过 Admin API 只返回“是否已配置”，不回显：

- `keySupplierBaseUrl`：供应 API 根地址。
- `keySupplierApiKey`：供应商用户密钥，敏感。
- `keySupplierPublicBaseUrl`：公开 HTTPS 根地址，部署域名后填写。
- `keySupplierWebhookToken`：首次启用时生成的高强度随机路径令牌，敏感。
- `keySupplierAutoPurchase`：是否处理 `new_keys_available` 自动购买。
- `keySupplierMinPurchase` / `keySupplierMaxPurchase`：单次购买边界。
- `keySupplierApiRegion`：导入 Kiro API Key 使用的 API Region。
- `keySupplierRpmLimit`、`keySupplierPriority`、`keySupplierGroups`、
  `keySupplierSourceChannel`、`keySupplierNicknamePrefix`：导入凭据模板。

默认关闭自动购买，最小和最大均为 1，API Region 为 `us-east-1`，RPM 为 10，来源渠道为
`Webhook 自动采购`。管理员保存有效的供应地址、密钥和购买边界后再显式开启自动购买。

## Webhook 入口与安全

公共入口为：

`POST /api/admin/key-supplier/webhook/{token}`

供应协议没有签名或自定义认证头，因此使用至少 32 字节随机令牌作为不可猜测路径。
入口不经过 Admin Key 中间件，但必须满足以下条件：

1. 路径令牌使用常量时间比较。
2. `Content-Type` 必须为 JSON，请求体限制为 64 KiB。
3. 仅接受两个已知事件；ID 必须是 32 位十六进制，数量必须为正整数。
4. `event_id` 在 SQLite 中有唯一索引；重复请求返回成功，但不重复排队。
5. 不记录请求头、供应商密钥或采购返回的 `ksk_` 明文。

公开 URL 由 `keySupplierPublicBaseUrl`、固定路径和令牌组成。没有域名时可开发和本地测试，
但“同步 webhook”操作返回明确的未配置错误。

## 数据与状态

新增 `<cache_dir>/key_supplier.db`，启用 WAL。事件表保存：内部 ID、`event_id`、事件类型、
供应订单号、消息、数量、接收时间、处理状态、尝试次数、最后错误、购买数、导入数、
重复数和已读时间。绝不保存采购响应中的 Key。

事件状态：

- `received`：已验证并落库。
- `processing`：已原子领取。
- `succeeded`：购买与导入完成，或通知事件已处理。
- `skipped`：自动购买关闭、库存低于最小值等无需立即执行的情况。
- `failed`：可重试失败。

进程启动和每 30 秒扫描 `received` 事件，并把长时间停留在 `processing` 的事件恢复为
`received`。同一事件只能被一个执行器原子领取。管理员可对 `failed`/`skipped` 事件重试。

## 自动购买与导入

`new_keys_available` 的处理顺序：

1. 自动购买关闭时标记 `skipped`，保留通知。
2. 调用 `/api/my/stock` 获取服务端当前上限。
3. 计算 `count = min(new_keys, stock.max, configured_max)`；低于配置最小值时标记 `skipped`。
4. 调用 `/api/my/purchase`，将 webhook 的 `purchase_order_id` 原样作为
   `client_order_id`。网络超时重试时始终复用该 ID。
5. 校验响应订单号、数量和每个 `ksk_` 格式。
6. 逐个调用现有 `MultiTokenManager::add_credential`，使用配置的区域、RPM、优先级、分组、
   来源和昵称前缀。现有重复 Key 校验继续生效；重复项计数但不视为整个订单失败。
7. 只保存购买数、导入数、重复数及脱敏错误摘要。

手动购买由管理员提交数量，服务端生成 32 位十六进制订单号并执行同一流程。响应只返回
订单号和汇总，不向浏览器返回 Key 明文。`all_keys_dead` 只生成告警，不自动禁用 RS 凭据。

## Admin API

除公共 webhook 外，以下端点均沿用 Admin Key 认证：

- `GET/PUT /api/admin/config/key-supplier`：读取和更新脱敏配置。
- `GET /api/admin/key-supplier/overview`：合并 profile、stock 和 status。
- `POST /api/admin/key-supplier/purchase`：手动购买并导入。
- `POST /api/admin/key-supplier/webhook/register`：向供应商同步公开 URL。
- `POST /api/admin/key-supplier/webhook/test`：让供应商发送测试事件。
- `GET /api/admin/key-supplier/events`：分页事件列表和未读数。
- `POST /api/admin/key-supplier/events/read`：批量标记已读。
- `POST /api/admin/key-supplier/events/{id}/retry`：重新排队。

所有供应商错误统一保留 HTTP 状态和最多 300 字符的脱敏说明。`ksk_`、`X-API-Key` 和
路径令牌在错误、Debug、事件列表和日志中均需脱敏。

## 管理后台

新增“Key 供应”页及顶栏未读徽标。页面包含：

- 连接配置：供应地址、密钥、公开地址、密钥已配置状态。
- 自动化配置：自动开关、最小/最大购买量、API Region、RPM、优先级、分组、来源与昵称。
- 供应概览：账号、剩余额度、已用额度、最大可取数、库存和系统状态。
- 操作区：刷新、同步 webhook、发送测试、手动购买。
- 通知列表：事件类型、消息、数量、状态、时间、结果和失败重试。

页面使用 React Query 轮询事件；发现新的未读事件时显示 Sonner 通知。轮询失败不清空旧数据，
连续错误只提示一次，避免通知风暴。移动端保持单列，表格信息改为可换行列表。

## 错误处理

- webhook 落库成功即返回 `202`，供应 API 调用和导入在后台执行。
- 重复 `event_id` 返回 `200`，响应标记 `duplicate: true`。
- 供应商 400/403/404/409 不盲目更换订单号；事件记录原订单号并进入可诊断状态。
- 供应商超时只使用相同订单号做有限重试，避免重复扣费。
- 单个 Key 导入失败不回滚已成功导入的 Key；汇总保留失败数量。
- SQLite 暂时不可用时 webhook 返回 `503`，促使上游重试。

## 测试

- Rust 单元测试：事件解析、路径令牌、数量计算、配置校验、密钥脱敏。
- SQLite 测试：`event_id` 去重、原子领取、状态迁移、未读统计和恢复。
- HTTP 客户端测试：请求头、订单幂等、错误映射、响应校验；使用本地 Axum 假服务。
- 路由测试：公共 webhook 不需要 Admin Key，其余端点必须认证；无效 body 和令牌被拒绝。
- 导入集成测试：购买结果复用现有 API Key 去重，且任何响应和记录不含 `ksk_` 明文。
- 前端测试：配置密钥不回显、手动购买参数、事件状态渲染、未读变化触发通知。
- 完整验证：`cargo test`、`cargo fmt --check`、Admin UI 测试和构建。

## 部署阶段

代码完成后再处理以下外部条件：

1. 为 `webhook.apiv3.52codeflow.top` 添加 DNS 记录并确认 Cloudflare 代理策略。
2. 在服务器 Nginx 为该主机配置 HTTPS，反向代理到 RS 监听端口。
3. 在管理页填写公开地址并同步 webhook，随后发送测试事件。
4. 验证事件入库后才开启自动购买。

