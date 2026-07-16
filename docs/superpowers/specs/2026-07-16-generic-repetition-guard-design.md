# 通用流式复读熔断设计

## 背景

Kiro 上游的 Opus 模型在超长上下文中可能退化为重复输出同一行或同一 reasoning 分片，例如连续输出 `}`。当前 RS 只对普通文本中的独占行 `call`、`count`、`card` 做 32 次阈值检测；原生 `reasoningContentEvent` 会直接转换成 `thinking_delta`，且 handler 不会在熔断后停止读取上游。因此客户可能看到 Thinking 无限增长、客户端卡顿、Token 持续消耗，并把复读垃圾带入下一轮历史。

正式服务器事故发生时仍运行 `kiro-rs 0.9.8`。本设计只修改 `kiro.rs-admin`，不修改参考项目 `D:/kiro2api/kiro-rs2/kiro.rs`。

## 目标

1. 普通文本和 Thinking 都能识别任意完全相同的连续短行或分片复读。
2. 检测到退化后立即停止读取上游，避免继续消耗输出额度。
3. 向客户端发送完整 Anthropic SSE 收尾，以 `stop_reason=max_tokens` 正常结束当前响应。
4. 不产生 502、连接重置或不完整 content block；下一轮对话可以继续。
5. 正常代码、不同缩进的闭合括号、工具调用和首字节路径不受影响。

## 非目标

- 不自动换账号或重试已经向客户端提交语义输出的请求。
- 不尝试修正模型已经生成的语义内容。
- 不新增管理端配置项；阈值先作为保守的内部常量。
- 不修改 Kiro 原版项目。

## 检测规则

每个 `StreamContext` 保存一个跨 chunk 的复读状态，并区分 `text` 与 `thinking` 通道。

- 候选单元为去掉行尾换行、空格、制表符后的非空内容。
- 保留行首缩进参与比较，避免把正常嵌套代码中不同缩进的 `}` 视为同一行。
- 只检测不超过 512 字节的候选，避免复制或比较超大正文。
- 同一通道内完全相同的候选连续出现 16 次时跳闸。
- 空行被忽略且不改变连续计数，以覆盖 `}\n\n}\n\n` 形态；普通不同内容、超长行或通道切换会重置计数。
- 跳闸前已经发送的正常内容保留；触发行及其后的文本/Thinking 不再发送，也不进入客户端历史。

现有 `call/count/card` 行属于通用规则的子集，不再需要硬编码才能得到流式保护。非流式/web_search 的既有块级保护暂不扩展，本轮事故和客户截图均来自流式 Thinking。

## 流式收尾

当文本或 Thinking 过滤器跳闸时：

1. `StreamContext` 将 stop reason 设置为 `max_tokens`。
2. 实时 `/v1/messages` handler 发送当前 chunk 中仍被允许的事件后，立即跳出上游 `bytes_stream` 循环。
3. Claude Code 缓冲流 handler 同样停止继续收集上游事件。
4. 复用现有 `generate_final_events_for(Eof)`，依次关闭开放的 thinking/text block，发送 `message_delta(stop_reason=max_tokens)` 和 `message_stop`。
5. 上游 response/stream 随任务结束被丢弃，底层 HTTP 请求取消。

客户端得到的是一次协议完整但被截断的正常响应，不是断流。若退化发生在工具调用生成之前，该轮不会凭空制造工具调用；客户可继续发送下一条消息。

## 日志与可观测性

跳闸时只记录：

- 通道（text/thinking）；
- 连续次数；
- 候选字节数；
- handler attempt 和已接收上游字节数。

不记录重复正文、请求头、凭据或工具参数。handler 同时写入 `upstream_repetition_guard` 协议诊断，便于错误快照检索，但最终客户端响应保持正常完成。

## 测试

新增回归测试覆盖：

1. 普通 text 中连续 `}` 在阈值处熔断，最终 stop reason 为 `max_tokens`。
2. 原生 `ReasoningContentEvent` 中连续 `}` 在 Thinking 通道熔断。
3. 15 次相同行不触发，验证阈值边界。
4. 不同前导缩进的 `}` 不触发，避免误伤正常嵌套代码。
5. 跳闸后后续 text/thinking 均不再产生可见 delta。
6. realtime 与 buffered handler 均检查 `repetition_guard_tripped()` 并结束上游读取。
7. 现有工具、Thinking、SSE 顺序及全量 Rust 测试继续通过。

## 客户影响

正常对话、工具调用、Token 口径和首字节速度不变。唯一行为变化是：若客户故意要求连续输出 16 个完全相同且缩进相同的短行，该轮会被视为模型退化并以 `max_tokens` 提前结束；会话本身仍可继续。
