# Rust SSE Early Stream Handshake Design

## Goal

让 `/v1/messages` 流式请求在等待 Kiro 上游响应时立即向客户端发送标准 SSE 注释，消除数秒无响应的等待感，同时保持错误可观测、请求可取消，并提供无需重新编译的回滚开关。

## Scope

- 仅修改 Anthropic `/v1/messages` 的流式路径。
- 非流式请求保持现有行为。
- 本地鉴权、请求 JSON、模型映射和参数校验继续在提交响应前完成，仍返回真实 HTTP 4xx。
- Kiro 上游请求开始后的错误在提前握手模式下通过标准 SSE `error` 事件返回。
- OpenAI 兼容层必须忽略立即握手注释，并安全忽略协议级 `ping`，不把心跳转换成正文事件。

不在本次范围内：账号选择策略、代理配置、TLS 实现、重试策略以及缓存计费逻辑。

## Configuration

新增布尔配置 `earlyStreamHandshake`：

- `false`：保持现有路径，等待 Kiro 返回响应头后再构造 SSE 响应；上游错误可继续映射为真实 HTTP 状态。
- `true`：立即构造 SSE 响应，首个 body 项为 `: connected\n\n`；上游调用在响应流内部继续执行。
- 缺省值为 `false`，避免升级后静默改变既有客户端的错误处理语义。

管理面板是否暴露该开关不属于本次首版范围；可以先通过配置文件启用和回滚。

## Architecture

### Existing path

当前 `handle_stream_request` 先等待 `provider.call_api_stream(...).await`。只有拿到 `reqwest::Response` 后才创建 `StreamContext`、初始事件和 Axum `Response`，所以上游响应头前客户端收不到任何字节。

### Early-handshake path

启用开关时，handler 完成本地校验和请求转换后立即返回 `200 text/event-stream`。响应 body 使用一个延迟上游流，按以下顺序工作：

1. 立即产出 `: connected\n\n`。
2. 在 body stream 内等待 `provider.call_api_stream(...)`。
3. 等待期间每秒产出合法 Anthropic 心跳 `event: ping\ndata: {"type":"ping"}\n\n`，直到上游调用成功或失败。第一个心跳约在一秒时产生，使仅识别 `data:` 的 New API 能记录并转发连接活性。
4. 上游成功后才创建并产出既有 `message_start`、`content_block_start` 等初始事件。
5. 后续复用现有 Kiro EventStream 解码和 Anthropic SSE 转换逻辑。
6. 上游失败时产出一个标准 Anthropic `event: error`，然后结束流；不得产出 `message_start` 或伪造正常收尾。

旧路径与新路径共享成功后的 `create_sse_stream` 处理，避免复制解码、工具调用、thinking、计量和 trace 收尾逻辑。

## Error Semantics

响应尚未提交前发生的错误保持原 HTTP 语义，包括：

- API key 鉴权失败；
- JSON/字段校验失败；
- 不支持的模型或请求形状；
- 本地转换阶段失败。

提前握手已经提交后发生的错误使用：

```text
event: error
data: {"type":"error","error":{"type":"api_error","message":"...","upstream_status":429,"retry_after_ms":1000}}

```

规则：

- `upstream_status` 仅在能从 provider 错误中可靠提取时出现。
- `retry_after_ms` 仅在上游明确提供时出现。
- 面向客户端的 `message` 使用现有脱敏错误文本，不泄露 token、代理认证或完整上游响应体。
- trace、usage hook 和失败计数继续记录为 error；HTTP 200 不得被统计成业务成功。
- 旧路径的 `map_provider_error` 保持不变。

## Cancellation and Resource Lifetime

延迟上游调用必须由响应 body stream 持有。客户端断开导致 body 被丢弃时：

- 正在等待的 provider future 被丢弃并取消；
- 已建立的 reqwest 请求随 future/drop 传播取消；
- 凭据和代理的 in-flight RAII guard 正常释放；
- 不再发送心跳或继续重试。

不得为每个请求启动脱离 body 生命周期的后台任务，避免客户端断开后继续消耗账号额度。

## Metrics

保留现有字段以兼容管理面板，同时修正其语义：

- `first_token_ms`：首次产出客户端可见的 `content_block_delta` 或结构化 `tool_use` 内容时记录；SSE 注释、初始事件、metadata 和空 delta 不计入。
- 新增 `upstream_first_byte_ms`：首次收到 Kiro 原始 body chunk 时记录。
- `duration_ms`、attempt duration 和最终状态保持现有定义。

心跳不能计为 Rust 自身的 token，也不能影响 output byte/token 统计。New API 的 `FirstResponseTime` 会把第一个合法心跳记作首响应，这是其“首 Token”列的既有口径，不改变 Rust 管理面板的真实内容首字口径。

## Compatibility

- 立即握手仍使用冒号开头的 SSE 注释，符合 SSE 标准，Anthropic 客户端应忽略。
- 一秒心跳使用 Anthropic 协议已定义的 `ping` 事件；New API 会识别其 `data:` 行、记录首响应并在 Claude relay 下转发。
- Rust OpenAI SSE parser 必须显式验证注释和 Anthropic `ping` 都不会转换成正文或错误事件。
- `Cache-Control: no-cache` 和 `Connection: keep-alive` 保持不变。

## Testing

按测试驱动方式覆盖：

1. 开关关闭时，pending 上游 future 不应提前产出 body 项。
2. 开关开启时，pending 上游 future 的首个 body 项立即为 `: connected\n\n`。
3. 上游持续 pending 时按一秒间隔产生 `event: ping` 与 `data: {"type":"ping"}`，且不会产生消息事件或触发 Rust `first_token_ms`。
4. 上游成功时，注释之后的首个协议事件仍为 `message_start`，随后进入现有事件顺序。
5. 上游 401、429、5xx/网络错误时产生一个脱敏 `event: error`，trace/usage 状态为 error，不产生 `message_start`。
6. 丢弃 body stream 会丢弃 provider future，验证取消哨兵被触发。
7. OpenAI SSE parser 忽略 `: connected` 注释与 Anthropic `ping` 事件；Chat Completions 和 Responses 均不得产生客户端正文事件。
8. `first_token_ms` 不被原始 chunk、注释或初始事件触发，只被首个实际内容触发。
9. `upstream_first_byte_ms` 在首个原始 chunk 触发且只记录一次。
10. 用 New API 的扫描规则验证：注释不触发首响应，合法 `ping` 的 `data:` 行会在约一秒触发首响应且可被 Claude relay 转发。

完成单元测试后运行完整 `cargo test`，再用本地两个服务发送相同的小型流式请求，记录响应头、首个非空行和首个 `content_block_delta` 时间。

## Rollback

将 `earlyStreamHandshake` 设为 `false` 并重启服务即可恢复旧路径，不需要回退二进制或数据库。新指标字段必须允许为空，旧数据和旧管理页面应继续可读。
