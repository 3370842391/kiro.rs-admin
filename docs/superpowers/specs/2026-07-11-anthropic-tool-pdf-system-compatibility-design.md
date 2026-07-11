# Anthropic 工具调用、PDF 与 system 映射兼容性设计

## 背景

第二轮 Ztest 报告 `01KX8N2CCEDDGSX1ZTWXD20X82` 的综合分为 76。第一轮修复已消除并发串扰并改善内容完整性和性能稳定性，但仍存在三个真实的协议兼容问题：

- D7 中响应的 `stop_reason` 为 `tool_use`，`content` 却没有 Anthropic `tool_use` block。
- D19 中 Anthropic `document` block 未被识别，导致文本型 PDF 对上游不可见。
- system 内容被降级到较早的 user history，指令时序弱于当前用户消息。

本轮只修复通用协议转换问题。不识别 Ztest 探针，不针对 nonce 或工具名增加特殊分支，不伪造上游模型身份，也不修改真实 Token 计量来提高检测分数。

## 目标

1. 流式和非流式响应都能输出结构正确的 Anthropic 工具调用。
2. `stop_reason=tool_use` 时必须至少存在一个 `tool_use` content block。
3. 支持 base64 编码的文本型 PDF，并将提取文本按原始消息顺序送入上游上下文。
4. system 指令在每次请求中保持原始顺序，并与当前用户消息处于同一请求轮次。
5. 所有解析失败和资源超限都返回明确的 Anthropic 风格错误，不能静默丢失内容。
6. 不破坏现有多轮对话、工具结果、图片输入、缓存计量和请求隔离行为。

## 非目标

- 不支持 OCR 或扫描版 PDF。
- 不支持远程 URL 文档下载。
- 不增加针对测试站点、固定验证词或特定工具名的分支。
- 不伪造 Claude 身份或改写 Kiro 的真实自我描述。
- 不通过压低 reported tokens 改善评分。
- 不在本轮统一重写整个流式/非流式响应状态机。

## 方案选择

采用中等范围的“共享兼容层增强”方案。

最小补丁会分别在流式和非流式处理器中复制工具恢复逻辑，并把 PDF 解析直接塞进大型转换函数，后续容易再次产生行为差异。完全统一响应状态机虽然长期更整洁，但回归范围过大。本方案只抽取本轮需要共享的归一化边界：文档输入标准化、current message 封装和工具调用输出标准化。

## 架构与组件

### 文档输入模块

扩展 Anthropic `ContentBlock`，识别 `type: "document"`。本轮只接受以下 source：

```json
{
  "type": "base64",
  "media_type": "application/pdf",
  "data": "..."
}
```

独立文档模块负责 base64 解码、PDF 资源检查、文本提取和错误归类。模块输出纯文本及页数等必要元数据，不直接构造 Kiro 请求，因此 PDF 实现可以替换而不影响协议转换器。

默认资源限制：

- base64 解码后的 PDF 最大 10 MiB。
- 最大 100 页。
- 单份 PDF 提取文本最多 200,000 个 Unicode 字符，超过限制即拒绝请求，不做静默截断。
- 空文本、只有图片的扫描版、加密文件和损坏文件均视为不可解析。

PDF 解析属于 CPU/阻塞工作，必须通过阻塞任务执行，不能占用异步请求执行线程。请求在所有文档成功解析后才允许发送给 Kiro，避免部分文档丢失时仍产生残缺请求。

### 当前消息封装

请求归一化器按以下顺序构造 Kiro `currentMessage`：

1. 客户端 system 指令区。
2. 当前轮文档内容区。
3. 当前用户消息区。

多个 system text block 按请求中的原始顺序连接。system 不再作为更早的普通 user history 发送，也不生成伪造的 assistant 确认语。

每个区块包含明确的类型、长度和边界。动态内容进行转义，使用户文本或 PDF 文本中出现同名边界时不能改变封装结构。文档区明确标记为“不可信引用内容”，其中的命令性文字不是客户端 system 指令。

如果一轮包含多个 text、image、document 或 tool result block，归一化器保持可表示内容的原始相对顺序。图片和工具结果继续使用现有 Kiro 协议结构；只有不能被 Kiro 原生表示的 system 和 PDF 文本进入带边界的文本封装。

### 工具调用输出归一化

建立共享的工具调用聚合函数，供流式和非流式路径调用。它接收两类候选：

- 上游结构化 `Event::ToolUse`。
- 文本内容中可由现有 `extract_invoke_content_blocks` 规则恢复的 `<invoke>` 调用。

结构化事件优先。文本恢复用于兼容上游只输出调用文本但结束原因为工具调用的情况。候选按调用 ID 去重；没有稳定 ID 时，按工具名和规范化后的 JSON 参数去重。恢复成功的 `<invoke>` 标记从对用户可见的普通文本中移除，避免同一调用同时以文本和结构化 block 暴露。

最终 `stop_reason` 根据归一化后的内容计算：存在工具调用时为 `tool_use`；不存在工具调用时不能返回 `tool_use`。若上游明确声称工具调用结束，但既没有结构化事件也无法恢复合法调用，则视为上游协议错误，而不是返回矛盾的成功响应。

## 数据流

1. HTTP 层完成 Anthropic 请求反序列化。
2. 请求归一化器收集 system 和 message content blocks。
3. 文档模块并行于本地内容转换执行 PDF 文本提取，但所有文档结果必须在发送上游前汇合。
4. 归一化器构造 history、currentMessage、图片和工具结果结构。
5. 现有 Kiro 客户端发送请求并接收事件流。
6. 工具调用聚合器同时消费结构化工具事件和可恢复文本。
7. Anthropic 流式或非流式渲染器输出一致的 content blocks 和结束原因。

## 错误处理

请求侧错误使用 Anthropic 风格 `invalid_request_error`，HTTP 状态为 400，并指出错误 content block 的位置。包括：

- 无效 base64。
- 非 `application/pdf` 媒体类型。
- 文件大小、页数或文本长度超限。
- PDF 损坏、加密、没有可提取文本或属于扫描版。

内部 PDF 任务异常或无法归类的解析器失败返回明确的服务错误，不把文档当作空字符串继续执行。

非流式响应在输出前发现 `tool_use` 不变量被破坏时，返回上游协议错误。流式响应若已经开始发送，则发送 Anthropic SSE `error` 事件并终止；若尚未发送响应头，则直接返回普通错误响应。

日志只记录错误类别、content block 索引、字节数和页数，不记录 PDF 正文、system 内容或 base64 数据。

## 测试设计

### 工具调用

- 非流式原生 `Event::ToolUse` 生成合法 `tool_use` block。
- 非流式文本 `<invoke>` 恢复为结构化调用。
- 两种候选同时存在时只输出一个调用。
- 流式调用具有完整的 `content_block_start`、参数 delta 和 `content_block_stop`。
- 嵌套 JSON、分片 JSON 和转义字符能够正确聚合。
- 普通文本中的相似标签不被误判。
- `stop_reason` 与最终 content blocks 保持一致。
- 使用通用 `get_weather` 示例做协议回归，但实现代码不识别该名称。

### PDF

- 单页和多页文本型 PDF 能提取测试夹具中的验证文本。
- base64 document 与普通文本混合时保持原始内容顺序。
- 多份文档按 content block 顺序注入。
- 损坏、加密、空文本、扫描版、错误媒体类型和超限文件返回明确错误。
- PDF 文本中的伪 system 边界经过转义，只作为不可信文档内容出现。
- 任一 PDF 失败时不会调用 Kiro 上游。

测试夹具应为仓库内最小化、无敏感信息的静态 PDF。扫描版夹具只需包含一张小图片，用于验证“无文本”错误，不引入 OCR 依赖。

### system 映射与回归

- 单个和多个 system block 保持原始顺序。
- system、document 和 user 区块边界明确。
- 动态内容包含边界字符串时不能逃逸封装。
- 固定 JSON 输出、指令覆盖和 identity lock 使用通用输入做行为级回归，不包含 Ztest nonce。
- 多轮消息、tool result、图片输入、缓存计量和每请求会话隔离保持现有行为。

## 验证命令

```text
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cd admin-ui && bun run build
git diff --check
```

另外使用本地 mock 上游分别执行 Anthropic 非流式工具调用、流式工具调用和文本型 PDF 请求，检查完整 HTTP/SSE 响应结构。

## 风险与缓解

- PDF crate 可能增加编译体积或触发复杂 PDF 的高资源消耗。实现计划需优先选择纯 Rust、依赖较轻的文本提取方案，并通过文件、页数和文本长度限制控制资源。
- system 与当前用户内容同轮封装能改善时序，但 Kiro 自带隐藏 system 仍具有更高优先级，因此不能保证模型服从所有客户端指令。
- `<invoke>` 文本恢复存在误判风险。只解析满足现有严格语法并具有合法工具名和 JSON 参数的完整调用；普通文本保持原样。
- 流式响应开始后无法更改 HTTP 状态。此时使用 Anthropic `error` 事件明确失败，而不是发送不一致的 `message_stop`。

## 验收标准

- 新增的 D7、D19 和 system 映射回归测试全部通过。
- 完整 Rust 测试、格式检查、Clippy 和 Admin UI 构建通过。
- 流式与非流式工具响应都满足 Anthropic content block 和结束原因不变量。
- 文本型 PDF 的验证文本能够进入实际发给 Kiro 的请求；扫描版和解析失败不会被静默忽略。
- system 保持客户端顺序，并在每次请求的 currentMessage 中位于用户内容之前。
- 不存在测试站点特判、身份伪造或 Token 虚报。
- 本轮目标是修复 D7、D19 并改善 system 指令遵循率；预计综合得分有机会进入 85–90，但不承诺任何固定检测分数。
