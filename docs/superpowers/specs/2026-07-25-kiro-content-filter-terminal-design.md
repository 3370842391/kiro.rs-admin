# Kiro 内容过滤终态设计

## 背景与根因

Kiro 可以用 HTTP 200 返回 AWS EventStream，但只包含
`metadataEvent.stopReason=CONTENT_FILTERED`、`contextUsageEvent` 和
`meteringEvent`，不包含正文、thinking 或完整工具调用。当前 RS 未注册
`metadataEvent`，该帧被当作 `Unknown` 丢弃，最终被误分类为可重试的
`EmptyResponse`，第二次失败后向客户返回 502。

公开参考实现：

- `youxuanxue/sub2api#1407` 已合并同类终态识别。
- `Quorinex/Kiro-Go#141` 在 2026-07-25 增加 `metadataEvent.stopReason` 解析。
- `Quorinex/Kiro-Go#143` 只允许在尚未输出时重试截断流；RS 已有等价门控。

## 目标行为

1. 将 `metadataEvent` 解析为结构化事件，保留 `stopReason`。
2. 当 `stopReason` 为 `CONTENT_FILTERED`，并且没有正文、thinking、redacted
   thinking 或完整工具调用时，将请求分类为不可重试的内容过滤终态。
3. Messages API 返回 HTTP 400、`invalid_request_error` 和稳定文案
   `Request was blocked by upstream content filtering`。
4. 流式请求在尚未提交 `message_start` 时返回普通 HTTP 400 JSON；不得先返回
   SSE 200 再伪造成功结束。
5. 严格 JSON 路径在首轮遇到内容过滤时立即结束，不追加 JSON 修复提示，也不重试。
6. 普通未知空流继续进行一次受控重试，第二次仍为空时继续返回现有 502。
7. 若上游在 `CONTENT_FILTERED` 前已经产生可交付正文、thinking 或工具调用，保留
   已有输出，不将其改写成 400。

## 数据流

`metadataEvent` 由 Kiro EventStream 解析器转换为 `Event::Metadata`。
`AttemptObservation` 记录是否出现 `CONTENT_FILTERED`，并在 attempt 收尾时结合
语义输出状态生成 `AttemptFailure::ContentFiltered`。现有流式、非流式和严格 JSON
路径继续共用 `AttemptFailure`，只在 HTTP 映射处把该类型映射为 400；其他失败类型
保持原状。

## 重试与账号边界

`ContentFiltered` 不加入 `ToolAttemptState::should_retry` 的允许列表，因此不会触发
空响应压缩重试、Schema 修复重试或第二次账号尝试。Kiro 已返回 HTTP 200，因此本次
变更也不新增凭据冷却、禁用或配额惩罚。

## 可观测性

解析器保留实际 `stopReason`。attempt 收尾日志输出稳定错误类型
`invalid_request_error`，错误快照继续记录原始上游事件体，以便区分内容过滤和真正
空流；客户响应不包含请求正文、凭据或上游内部数据。

## 客户影响

只有此前被误报为 502 的内容过滤请求会改为明确的 HTTP 400。正常对话、1 秒 SSE
Ping、首字门控、缓存计量、usage、工具 ID 和普通空流重试均不改变。NewAPI 会把
该请求视为客户端请求不可重试，而不会错误切换同类 Kiro 账号。

## 验收

- `metadataEvent` 的 camelCase 与 snake_case stop reason 均可解析。
- 无输出 `CONTENT_FILTERED` 分类为 `ContentFiltered` 且不重试。
- 有输出 `CONTENT_FILTERED` 保持成功。
- 流式、非流式、严格 JSON 均返回 HTTP 400，而普通空流仍返回 502。
- 相关 Rust 聚焦测试、完整测试、格式检查通过。
