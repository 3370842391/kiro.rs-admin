# 默认最好模式设计

## 目标

新增一个名为“默认最好模式”的 RS 路由预设，首选 `Kiro Runtime`，并按以下顺序安全降级：

1. Kiro Runtime（`runtime`）
2. Legacy Kiro IDE（`ide`）
3. Legacy CodeWhisperer（`codewhisperer`）
4. Legacy Amazon Q（`amazonq`）

同时把会话级粘性、上游首字节延迟和实时 in-flight 负载纳入账号调度。旧的手动端点与 CLI 路由继续兼容。

## 非目标与安全边界

- 不对四个端点做无条件并发竞速；一次对话请求只允许一个上游执行，避免重复工具调用、重复扣费和重复文本。
- 不改变已有 400 客户请求错误、工具 Schema 错误、524 网关超时的快失败规则。
- 不改变客户端 SSE 事件顺序、首字节心跳、token 计费和缓存字段。
- 不改变凭据级 401/403/429 冷却规则；最佳模式只改变默认端点和账号选择顺序。

## 配置模型

在 `Config` 增加 `endpoint_mode`，序列化字段为 `endpointMode`：

- `best`（默认）：使用 `runtime` 作为未显式指定 endpoint 的凭据首选，并使用固定四端点降级链。
- `manual`：沿用当前 `defaultEndpoint` 和 `endpointChains` 配置。

显式设置在凭据上的 `endpoint` 优先于全局模式；CLI 凭据仍使用 `cli`，不被 IDE 协议链改写。最佳模式的固定链只适用于 IDE 协议端点。

管理端新增 `GET/PUT /api/admin/config/endpoint-mode`，返回机器值、中文标签、首选端点和降级链。保存后运行时立即生效并持久化到 `config.json`。

## 调度模型

### 会话粘性

从 Kiro 请求体提取 `conversationState.conversationId`，与客户端分组组成进程内 affinity key。最佳模式下，最近 5 分钟内仍健康且未超载的会话优先复用上次成功账号；账号达到粘性让出阈值或进入冷却时，回退到全局调度。状态只保存在内存，不写凭据文件。

### 实时并发调度

在凭据运行态维护 `in_flight` 和 `first_byte_ewma_ms`：

- 每次请求交出凭据时原子式增加 in-flight，RAII guard 结束时减少。
- 流式响应首次收到上游 body chunk 时记录首字节延迟，并以 alpha=0.3 更新该凭据 EWMA。
- 最佳模式的候选排序优先考虑 in-flight；当多个候选负载接近时，首字节 EWMA 较慢的账号降权。没有样本的账号不因延迟项受罚。
- 调度快照向 Admin UI 暴露 `firstByteEwmaMs`，便于观察实时效果。

### 端点重试

最佳模式首选 `runtime`，在未向客户端提交语义内容且收到可重试的 408/429/5xx 时按固定四端点顺序串行尝试。现有 `maxBucketAttemptsPerRequest` 仍是硬上限；所有端点失败后回到现有账号级重试与冷却分支。

## 错误处理

- 已开始输出文本、thinking 或工具调用后不切换端点。
- 端点重试失败记录完整 endpoint attempt，最终错误仍沿用当前 HTTP 状态映射。
- 配置文件缺少 `endpointMode` 时反序列化为 `best`；非法值在管理端返回 400，不修改当前配置。
- 旧的 `endpointChains` 继续可读；切换到 `manual` 后恢复其覆盖行为。

## 验证

- Rust 配置测试：默认值、旧 JSON 兼容、非法模式拒绝。
- Rust 调度测试：会话粘性、过载让出、首字节 EWMA 排序、无样本不降权。
- Rust 端点测试：最佳模式固定四端点顺序，手动模式仍使用原链。
- Admin API 测试：GET/PUT 返回标签和链，持久化后重新加载保持模式。
- 前端契约测试：模式选择显示中文标签和四端点顺序，手动模式仍可编辑链。
- 回归：`cargo test`、`cargo fmt --all -- --check`、`bun test`、`bun run build`。
