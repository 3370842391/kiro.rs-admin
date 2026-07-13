# 工具 JSON 中断与图片总预算修复设计

## 1. 目标

修复生产中反复出现的两类请求失败：

```text
Upstream ended before completing tool_use ... JSON input
CONTENT_LENGTH_EXCEEDS_THRESHOLD
```

设计目标：

- 上游已经输出完整合法 JSON、但漏发 `stop=true` 时，安全恢复工具调用。
- 真正半截或非法 JSON 永不补全、永不转发执行。
- 在尚未向客户端交付可执行工具调用时，提供一次受控重试。
- 对多轮历史图片实行总预算，而不只处理单张图片。
- 自动压缩历史图片，但不自动删除任何图片。
- 当前轮图片保持原质量；仍超限时在发送上游前返回可操作错误。
- 不造成重复工具执行、重复文本或错误 usage。

## 2. 已确认根因

### 2.1 tool_use

当前 `ToolJsonAccumulator::finish()` 的行为是：

- 空缓冲按 `{}` 打捞无参工具。
- 任何非空缓冲只要没收到 `stop=true`，一律记为 `IncompleteJson`。
- 不尝试判断缓冲区本身是否已经是一个完整合法 JSON 值。

因此“完整 JSON + 上游漏结束标记”会被误判。生产日志中的 `fs_write` 连续多次只缓冲约 54–60 字节，也可能包含真正半截 JSON；没有证据时不能把所有错误都当作漏标记。

### 2.2 图片总量

当前图片处理主要限制单张图片。handler 会统计图片数和 base64 总字节，但超过警告阈值只写 warning，不改变请求。

生产请求曾出现约 12 张图片、约 1993 KiB base64，随后上游返回 `CONTENT_LENGTH_EXCEEDS_THRESHOLD`。这说明单图缩放成功不等于整个多轮请求体满足 Kiro 总大小限制。

## 3. 工具 JSON 完成状态

将流结束时的残留分为四类：

```text
Empty       空白输入，等价于 {}
Complete    整个缓冲可严格解析为一个 JSON 值
Incomplete  serde_json 错误类别为 EOF，明显在字符串、数组、对象或值中途结束
Invalid     已经结束但语法非法，或存在一个 JSON 值之外的额外非空内容
```

处理规则：

- `Empty`：沿用当前无参工具打捞。
- `Complete`：把流结束视为隐式 stop，经统一工具名和参数还原入口转发。
- `Incomplete`：不转发，进入受控重试或协议错误。
- `Invalid`：不转发，返回 `upstream_tool_json_error`。

只允许 `serde_json` 对完整缓冲严格解析。禁止补右括号、补引号、截断尾部、猜测字段或根据工具 schema 自动生成缺失参数。

流结束时必须先对本批全部残留完成分类，再原子提交：

- 全部为 `Empty` 或 `Complete` 时，才一次性生成工具事件。
- 任意一项为 `Incomplete` 或 `Invalid` 时，本批尚未交付的完整工具也不先发送。
- 缓冲路径丢弃整次 attempt 后重试；实时路径按已输出状态决定报错。

这样可以避免客户端先执行一部分工具，随后才发现同一轮另一个工具参数残缺。

## 4. 工具调用重试边界

透明重试必须同时满足：

1. 错误是 `IncompleteJson`，不是工具 schema 业务错误。
2. 尚未向客户端转发任何 `tool_use`。
3. 尚未发生客户端工具执行。
4. 当前请求在可缓冲路径中，或者尚未发出任何不可撤销的内容 delta。
5. 本请求此前未因同类错误重试。

### 4.1 非流式与缓冲流

非流式和 Claude Code 缓冲流在完整验证前不向客户端交付事件，可以使用原始请求重试一次。重试复用正常凭据调度，优先换一个健康凭据；不追加隐藏提示词，不改变工具 schema。

第二次仍然半截时返回明确错误，不继续循环。

### 4.2 实时流

如果实时流已经发出文本、thinking 或工具事件，则不能透明重放，否则会造成重复内容或重复工具执行。此时：

- 发送一个 Anthropic SSE `error` 事件。
- 不发送成功的 `message_delta` 和 `message_stop`。
- 日志标记 `retry_skipped=client_visible_output_started`。

如果实时路径尚未发出任何客户端可见内容，可以在内部重试一次；实现若无法可靠证明该条件，则保守地不重试。

## 5. 工具调用状态与追踪

请求级状态增加：

- 是否已向客户端发送内容。
- 是否已转发任何工具调用。
- 本次是否为 tool JSON 重试。
- 错误分类：`complete_without_stop`、`incomplete_eof`、`invalid_json`。

日志只记录工具调用 ID、工具名、缓冲字节数、错误类别和重试结果，不记录 `fs_write` 的路径或文件正文。

成功打捞完整 JSON 时记录聚合指标，方便区分“修复命中”与“真正上游截断”。

## 6. 图片总预算策略

新增结构化 `ImageBudgetPolicy`，默认开启：

```text
enabled=true
totalBase64BudgetBytes=819200
historyMaxDimension=1280
historyJpegQuality=72
retryHistoryMaxDimension=960
retryHistoryJpegQuality=60
```

第一版按以上固定默认值发布。后续如需调整，只通过有类型、带范围校验的管理端配置完成，不能暴露任意环境变量编辑器。

预算只统计将发送给 Kiro 的 base64 字节；日志另行记录图片数量、当前轮字节、历史字节和压缩前后总量。

## 7. 图片处理顺序

预算器在最终 `KiroRequest` 出站结构序列化前执行，而不是只扫描原始 Anthropic 顶层 content。它必须递归覆盖：

- 普通 user content 中的图片。
- `tool_result.content[]` 中被转换或提升的图片。
- `/v1/messages` 与 `/cc/v1/messages` 两条路径。

现有 converter 的逐图环境变量压缩和历史 SHA256 去重必须由统一预算器接管：converter 保留原始图片及全部图片 block，预算器从原始数据最多重编码一次。这样不会先按单图规则压一次、再按总预算二次 JPEG 压缩，也不会因旧去重逻辑违反“不删除图片”。

请求发送前执行：

1. 扫描全部图片并区分当前最后一条 user 消息与历史消息。
2. 当前轮图片不修改。
3. 如果总量超过预算，从最旧历史图片开始重新编码和降采样。
4. 每处理一张后重新计算总量，达到预算即停止。
5. 不删除图片、不替换为文字占位符、不合并语义重复图片。
6. 如果所有历史图片都达到最低允许质量后仍超预算，则不调用上游，直接返回明确 HTTP 400。

预算处理只修改最终出站副本，不修改客户端原始 `MessagesRequest`，也不改变文本、tool ID、tool_result status、消息顺序或工具配对。

拒绝信息包含：

- 图片总数。
- 当前轮和历史图片数量。
- 压缩后的 base64 总 KiB。
- 配置预算 KiB。
- 建议减少当前轮图片或开启新会话。

不得返回任何图片内容、hash、文件路径或用户正文。

## 8. 上游 400 的一次降级重试

如果预检通过但 Kiro 仍返回 `CONTENT_LENGTH_EXCEEDS_THRESHOLD`，说明限制还包含文本、工具结果或其他序列化开销。

仅当存在尚可继续压缩的历史图片时：

1. 使用 retry 级别的历史图片尺寸和质量重新生成请求体。
2. 重试一次，优先换健康凭据但保持相同模型。
3. 第二次仍失败则返回明确的 Anthropic `invalid_request_error`。

如果没有历史图片、只有当前轮图片，或历史图片已达到 retry 下限，则不进行无效重试。

该错误发生在模型生成前，不存在重复输出或重复工具执行风险。

## 9. Admin API 与界面

在治理设置中新增“图片总预算”区块：

- 启用图片预算治理。
- 总 base64 预算 KiB。
- 历史图片最大边长。
- 历史图片 JPEG 质量。
- 上游拒绝后的重试最大边长和质量。

配置持久化到现有 `config.json`，运行时立即生效。校验范围：

```text
totalBase64BudgetBytes: 256 KiB–8 MiB
historyMaxDimension: 640–4096
historyJpegQuality: 40–95
retryHistoryMaxDimension: 480–historyMaxDimension
retryHistoryJpegQuality: 30–historyJpegQuality
```

界面固定提示：

> 只自动压缩历史图片，不会删除图片，也不会修改当前轮图片。预算过低可能使长对话更早要求开启新会话。

## 10. 协议与 usage

- 完整工具 JSON 打捞后必须沿用现有 `CompletedToolUse::from_kiro()`，保证短工具名和参数映射一致。
- 流式事件顺序保持 `content_block_start -> input_json_delta -> content_block_stop -> message_delta -> message_stop`。
- 失败路径不能同时发送 SSE `error` 与成功终止事件。
- 透明重试只计最终客户端可见一次响应；trace 保存每次上游 attempt。
- 上游已经消耗的失败尝试可以记录内部 credits，但不能伪造成客户端输出 token。
- 图片压缩改变实际发送给上游的字节，不改变客户端原始请求和 traces 中的请求摘要。

## 11. 测试设计

### 11.1 ToolJsonAccumulator RED 测试

- 合法对象 JSON、无 stop：`finish()` 必须打捞。
- 合法数组、字符串、数字 JSON、无 stop：严格按 JSON 值处理。
- 真半截对象、数组、字符串：仍为 `IncompleteJson`。
- 完整但非法 JSON：为 `InvalidJson`。
- 一个完整值后带额外非空内容：不得打捞。
- 空输入仍按 `{}` 打捞。
- 多工具残留中完整、空和半截分类稳定且顺序确定。
- 多工具残留中存在任意半截或非法项时，本批完整项也不得先交付。
- 工具名与参数反向映射不回归。

### 11.2 重试测试

- 非流式第一次半截、第二次完整：只返回第二次完整工具调用。
- 缓冲流第一次半截、第二次完整：客户端只收到一套标准事件。
- 第二次仍半截：返回一次明确错误。
- 已有客户端可见内容时不重试。
- 已转发工具调用时不重试。
- 任何路径最多一次同类重试。

### 11.3 图片预算测试

- 未超预算时请求体字节不变。
- 顶层图片和 `tool_result.content[]` 图片都计入同一总预算。
- `/v1/messages` 与 `/cc/v1/messages` 使用相同的递归统计和治理结果。
- 超预算时只压缩历史图片，当前轮 base64 完全不变。
- 从最旧历史图片开始处理。
- 不删除图片，处理前后图片 block 数量一致。
- 达到预算后停止额外重编码。
- 无法达到预算时在调用 provider 前返回 400。
- 上游首次阈值错误、二次压缩成功时只返回最终响应。
- 没有历史图片时不做无效重试。
- PNG 透明图、JPEG、GIF 和不支持格式有确定性降级行为。
- 损坏 base64 返回清晰错误，不发生 panic。

### 11.4 回归与集成

- tools、required specific tool、thinking、PDF、websearch 的现有测试通过。
- Claude Code 真实 `fs_write` 连续调用不会因完整 JSON 漏 stop 中断。
- 真半截 `fs_write` 不会到达客户端执行器。
- 12 张历史截图场景经过治理后可调用，或在上游前返回可操作错误。
- UTF-8 工具参数和图片旁中文文本不发生字节切片错误。

## 12. 发布验证

先在 8991 测试容器开启 DEBUG 聚合字段，执行：

1. 流式与非流式 `fs_write`。
2. 多轮连续工具调用。
3. 人工构造完整 JSON 漏 stop。
4. 人工构造真正半截 JSON。
5. 逐步增加历史截图直到触发预算。
6. 本地 Ztest 类探针和真实 Claude Code 会话。

验收后再部署 8990。观察：

- `complete_without_stop` 打捞次数。
- `incomplete_eof` 重试成功率。
- 工具调用中断率。
- 图片预压缩次数、上游阈值错误次数和预检拒绝次数。

修复稳定后关闭详细 DEBUG，仅保留不含正文的聚合指标。

## 13. 客户影响

正面影响：

- 完整工具调用不再因漏结束标记被丢弃。
- 非流式和缓冲流中的真正上游截断有一次恢复机会。
- 多图长对话在调用上游前得到治理或明确提示。

可能影响：

- 历史截图可能降低分辨率和 JPEG 质量。
- 真正半截工具参数仍会失败，这是防止错误文件写入的必要保护。
- 只有当前轮大量图片时不会自动压缩，客户需要减少图片或开启新会话。
- 一次受控重试会增加失败场景的延迟，但不会造成重复工具执行。

## 14. 非目标

- 不补全或猜测半截 tool_use JSON。
- 不静默删除任何当前或历史图片。
- 不修改当前轮图片。
- 不把工具参数或图片正文写入日志。
- 不对所有 400 盲目重试。
- 不为单个 Ztest 报告 ID 编写硬编码分支。
