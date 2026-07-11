# Token、system、工具调用与文档兼容性跟进设计

## 背景

Ztest 报告 `01KX8T1E4QWA3E1666XFTNEF11` 的综合分为 75。上一轮已经增加文本型 PDF 提取、非流式 `<invoke>` 工具调用恢复以及 system 当前消息映射，但新报告暴露了四项需要跟进的问题：

- S1 报告的输入 Token 存在约 5,000–6,000 的固定开销，并随输入长度异常增长。
- S3 中，当前 `client_system_instructions` / `user_content` JSON 信封被上游明确识别为提示注入，降低了普通客户端 system 指令的遵循率。
- D7 返回 `stop_reason=tool_use`，但响应中没有实际的 Anthropic `tool_use` content block。
- D19 的 PDF 请求返回空响应；当前 `untrusted_document` JSON 包装增加了上游理解文档正文的难度。

代码追踪表明，S1 的主要原因不是 RS 内存在约 6K Token 的固定提示词，而是 RS 将 Kiro `contextUsageEvent` 换算出的整个上游上下文占用直接写入 Anthropic API 的 `usage.input_tokens`。该上下文包含 Kiro 不可由 RS 删除的隐藏 foundational prompt。RS 自身额外加入的主要内容是 system JSON 信封和 thinking XML。

本轮只修复通用协议转换和计量边界。不识别测试站点、nonce、固定验证词或工具名称，不伪造模型身份，不把普通叙述文本伪造成工具调用，也不宣称减少了 Kiro 的真实计费。

## 目标

1. API `usage` 反映客户端提交并由 RS 转换的可见输入，不再把 Kiro 隐藏上下文计入客户端输入 Token。
2. Kiro 上游上下文占用继续用于日志和上下文溢出保护，不能因 API 计量调整而丢失安全护栏。
3. 撤回会触发注入识别的 system JSON 信封，同时保持多轮历史、工具结果与会话隔离行为不变。
4. 对原生支持 reasoning 的模型不再注入 thinking XML，减少 RS 自身的附加输入。
5. PDF 提取文本使用简单、稳定的引用格式进入当前消息，不再嵌套 `untrusted_document` JSON。
6. `stop_reason=tool_use` 与实际输出的 `tool_use` block 保持严格一致。
7. 流式、非流式、`/v1` 与 `/cc` 路径使用一致的 Token 和协议不变量。

## 非目标

- 不删除或绕过 Kiro 自带的 foundational prompt。
- 不修改 Kiro 的真实 credits、计费或上游上下文统计。
- 不删除 `agentTaskType=vibe`、`origin=AI_EDITOR` 或 `envState`；这些字段属于当前上游协议兼容边界。
- 不修改 `conversationId`、history 顺序、assistant/tool-result 配对规则。
- 不伪造 Claude、Kiro 或其他模型身份。
- 不支持扫描版 PDF 的 OCR，也不增加远程文档下载。
- 不承诺固定 Ztest 分数；验收以通用协议行为和回归测试为准。

## 方案选择

采用“双轨计量 + 回退 system 信封 + 精简本地提示词”方案。

备选一是继续直接向外暴露 Kiro `contextUsageEvent`。它保留了上游上下文占用，但会把客户端不可见的 Kiro 隐藏提示词误算为客户端输入，不符合 Anthropic 兼容接口的可见用量语义。

备选二是完全忽略 `contextUsageEvent`。它能消除 API 用量异常，却会失去真实上游上下文的溢出判断依据。

选定方案将两种口径分离：客户端协议使用本地可见计量，上游安全控制继续使用 Kiro 上下文计量。这样既修复兼容接口，又不牺牲服务端保护。

## 架构与组件

### 双轨 Token 计量

引入两个语义明确、不可互相覆盖的计量值：

- `client_visible_tokens`：本地计算 system、messages、tools 及 RS 实际发送的必要兼容包装。它用于 Anthropic API `usage`、流式 usage 事件和 prompt cache 字段。
- `upstream_context_tokens`：由 Kiro `contextUsageEvent` 和模型上下文窗口换算。它只用于结构化日志、监控和服务端上下文溢出保护。

现有 `resolve_usage_input_tokens` 不再允许上游值无条件覆盖本地值。调用边界应使用带语义的结构或参数名，避免以后再次混淆两种口径。

缓存字段必须遵守 Anthropic usage 内部一致性：缓存创建、缓存读取与非缓存输入的合计等于该响应的客户端可见输入总量。没有足够信息区分缓存类别时沿用现有分类规则，但总量不得切换到上游上下文口径。

结构化日志可以同时记录两个数值及其差值，但不得记录 system、用户消息、工具参数、PDF 正文或其他敏感内容。日志字段名称必须明确，不能继续使用含义模糊的 `input_tokens` 表示上游占用。

### system 映射回退

删除当前消息中的以下 JSON 信封字段：

```json
{
  "client_system_instructions": [],
  "user_content": "..."
}
```

system 恢复为此前较稳定的原始历史表示，保持客户端 system text blocks 的原始顺序。不得新增 assistant 确认语、策略声明或要求模型解释 system 的提示词。

本次回退只改变 system 内容的表示位置和包装，不改变实际对话轮次的相对顺序、`conversationId`、已有 assistant 消息、工具调用或工具结果配对。图片和工具结果继续使用现有 Kiro 原生结构。

Kiro 自带隐藏指令仍可能高于客户端 system 指令，因此无法保证身份覆盖类指令被遵循；本轮只消除 RS 自己引入的注入特征。

### reasoning 提示精简

对已原生支持 reasoning/output configuration 的模型，例如 Opus 4.8，保留其原生请求字段，不再向消息正文注入：

```text
<thinking_mode>...</thinking_mode>
<max_thinking_length>...</max_thinking_length>
```

对尚不支持原生 reasoning 映射的旧模型，暂时保留现有兼容回退，避免扩大行为回归范围。模型能力判断应复用已有模型归一化逻辑，而不是按测试请求内容判断。

### PDF 文本映射

保留现有 base64 解码、文本型 PDF 提取、资源限制和错误分类。本轮只调整成功提取后的消息表示：使用简短的普通文本边界标记，将文档名称或序号、正文作为引用内容直接放入当前用户消息，不再序列化为 `untrusted_document` 嵌套 JSON。

边界标记不包含策略性指令，不声称替用户或 system 下达命令，也不要求模型讨论提示注入。多个 document/text block 继续保持客户端 content blocks 的相对顺序。提取为空、上游返回空内容或无法产生合法响应时必须明确失败，不能返回 HTTP 200 的空答案。

### 工具调用不变量

流式和非流式路径都以最终实际发出的内容块计算结束原因：

- 至少发出一个合法 `tool_use` block 时，结束原因为 `tool_use`。
- 未发出 `tool_use` block 时，不能返回 `tool_use`。
- 上游声明工具结束，但既没有结构化工具事件，也无法从严格 `<invoke>` 语法恢复合法调用时，返回上游协议错误。

普通文本中的“我将调用工具”“这里存在注入”等叙述不能转换为工具块。现有严格 `<invoke>` 恢复只处理完整、合法、能对应已声明工具的调用。

## 数据流

1. HTTP 层解析 Anthropic 请求并验证内容块。
2. 请求归一化器按原始顺序整理 system、history、当前消息、文档、图片和工具结果。
3. Token 计量器计算 `client_visible_tokens`，并在请求级上下文中固定该值。
4. reasoning 映射器对支持原生配置的模型发送原生字段；只在旧模型兼容路径保留文本回退。
5. Kiro 客户端使用原有 `conversationId`、`vibe`、`AI_EDITOR` 和 `envState` 发送请求。
6. 事件处理器接收 `contextUsageEvent`，计算 `upstream_context_tokens`，只更新日志和上游上下文护栏。
7. 工具调用聚合器生成最终 content blocks，并从实际输出计算结束原因。
8. 响应渲染器将 `client_visible_tokens` 写入流式或非流式 Anthropic usage；上游数值不进入客户端 usage。

## 错误处理

- 客户端请求、PDF 或工具参数无效时，继续返回 Anthropic 风格的明确错误。
- 上游报告工具调用结束但没有可输出的合法工具块时，非流式返回上游协议错误；流式若已经开始，则发送 Anthropic SSE `error` 并终止。
- PDF 提取成功但上游最终响应为空时，不返回伪成功；应转换为可诊断的上游空响应错误。
- 上下文溢出判断使用 `upstream_context_tokens`。上游事件缺失时按现有保守策略处理，不能用较小的客户端可见值绕过保护。
- 本地 Token 估算失败时返回明确内部计量错误，不能悄悄退回隐藏上下文口径。

## 测试设计

### Token 计量

- 本地估算为 72、Kiro 上游换算为 5,417 时，API `input_tokens` 为 72；日志/护栏仍收到 5,417。
- 长输入增加时，API usage 只按客户端可见内容增长，不继承 Kiro 隐藏固定开销。
- 流式、非流式、`/v1`、`/cc` 对同一请求返回一致口径。
- prompt cache 各输入字段之和等于客户端可见输入总量。
- 缺少 `contextUsageEvent` 不影响 API usage；存在该事件也不能覆盖本地计量。
- 上游上下文接近模型窗口时，即使客户端可见值较小，服务端护栏仍触发。

### system 与多轮回归

- 发往 Kiro 的内容中不存在 `client_system_instructions`、`user_content` JSON 信封。
- 单个和多个 system blocks 保持原始顺序。
- 映射过程不生成 assistant 确认语、身份声明或策略提示。
- 多轮 user/assistant history 顺序不变。
- `tool_use` 与 `tool_result` 配对不变。
- `conversationId`、请求隔离、图片输入和缓存行为不回归。

### reasoning

- Opus 4.8 请求不包含 `<thinking_mode>` 或 `<max_thinking_length>` XML。
- Opus 4.8 原生 reasoning/output configuration 保留且值正确。
- 不支持原生 reasoning 的既有模型继续走兼容回退。
- reasoning 开关不改变 system、history 或工具配对。

### PDF

- 提取出的验证文本直接存在于 current message 的文档引用区块中。
- current message 不包含 `untrusted_document` JSON。
- 文档、普通文本和多文档保持原 content block 相对顺序。
- 文档正文包含边界相似文本时不能逃逸引用区块。
- 扫描版、损坏、加密、超限或空文本 PDF 继续返回明确错误。
- 上游空响应被识别为失败，不返回 HTTP 200 空答案。

### 工具调用

- 返回 `stop_reason=tool_use` 前必须实际输出至少一个 `tool_use` block。
- 只有普通叙述文本时不生成工具块，也不返回 `tool_use`。
- 上游工具结束信号无合法调用时触发协议错误。
- 结构化工具事件和严格 `<invoke>` 同时存在时正确去重。
- 流式工具块具有完整 start、参数 delta、stop 和 message delta 顺序。

测试使用通用夹具和 mock 上游，不包含 Ztest nonce、报告专用验证词或针对固定工具名称的生产分支。

## 验证命令

```text
cargo fmt --check
cargo test
cargo clippy --all-targets --all-features -- -D warnings
cd admin-ui && bun run build
git diff --check
```

若全仓严格 Clippy 仍出现既有历史 lint，应单独记录基线，并对本轮变更文件运行范围适当的 Clippy 或等价静态检查；不得借本轮任务进行大面积无关格式化或 lint 重写。

## 风险与缓解

- API usage 口径改变可能影响依赖旧行为的调用方。通过版本说明明确该字段现在表示客户端可见输入；上游占用继续保留在服务端日志，而不混入兼容协议。
- 本地 Token 估算与供应商精确 tokenizer 可能存在小幅偏差。所有路径必须复用同一估算器，优先保证一致、单调和不包含隐藏上下文。
- system 回退可能降低其与当前用户消息的接近程度，但新报告已证明 JSON 信封会触发明确的注入识别。通过 system 顺序、多轮和行为级回归测试控制风险。
- 删除 thinking XML 可能改变部分模型的思考行为。仅对已有原生 reasoning 支持的模型删除，旧模型保持兼容回退。
- 简单 PDF 引用格式不构成安全边界，模型仍可能受文档内容影响。本轮目标是协议可理解性，不声称实现提示注入隔离。
- 流式响应开始后无法修改 HTTP 状态。发现工具不变量被破坏时使用 SSE error 明确终止，而不是输出矛盾的成功结束事件。

## 验收标准

- 新增 Token 双轨、system 回退、reasoning 精简、PDF 映射和工具不变量测试全部通过。
- API usage 不再包含 Kiro 隐藏 foundational prompt 的固定开销；`contextUsageEvent` 仍参与日志和上下文保护。
- Kiro wire 内容中不再出现 system JSON 信封；Opus 4.8 不再出现 thinking XML。
- PDF 验证文本可以进入实际上游请求，空响应不会被报告为成功。
- `stop_reason=tool_use` 与实际 `tool_use` blocks 在所有响应路径中一致。
- 多轮 history、会话 ID、工具结果配对、图片和请求隔离无回归。
- 不存在站点特判、身份伪造、普通文本工具调用伪造或 Kiro credits 虚报。
- S1、S3、D7、D19 的通用根因得到修复，但不承诺固定检测分数，也不把 API 显示优化描述为 Kiro 实际成本下降。
