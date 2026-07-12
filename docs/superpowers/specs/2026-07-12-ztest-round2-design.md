# Ztest 第二轮剩余问题修复设计

## 1. 目标与非目标

本轮针对报告 `01KXAZ23KM7YQ05RC13BADHXMB` 中仍有证据支持修复的三项问题：

- D7：强制工具调用时让 `tool_use` 成为第一个 content block。
- S3：执行明确要求固定字面量或固定 JSON、且禁止额外文本的 system 指令。
- D16：严格 JSON 输出出现前导解释或截断时，在客户端看到任何内容前做一次受限恢复。

以下项目不修改：

- D11 的知识截止时间不硬编码；没有可信模型元数据时不伪造答案。
- S1 已无固定注入且判定成功，不再人为整形 token/cache。
- `tool_choice=auto`、普通 system、普通 JSON/代码生成、thinking、工具参数和 PDF 路径保持现状。

## 2. 已确认的证据

### 2.1 D7

两次 Ztest 工具请求在 rs DEBUG 中均完整收到 `get_weather` 参数分片，并发出了：

1. `content_block_start(type=tool_use)`；
2. 完整 `input_json_delta`；
3. `content_block_stop`；
4. `message_delta(stop_reason=tool_use)`。

通过 NewAPI 原始 SSE 重放后，实际顺序为：

```text
content_block_start(text, index=0)
text_delta...
content_block_stop(index=0)
content_block_start(tool_use, index=1)
input_json_delta
content_block_stop(index=1)
message_delta(stop_reason=tool_use)
```

因此 rs 与 NewAPI 都没有丢工具块。Ztest 只保留首个 text block，报告为 `raw_content_types=["text"]`。修复点是 required tool 的首块顺序，不是扩大 XML 打捞或伪造工具调用。

### 2.2 S3

Kiro 没有 Anthropic 等价的原生 system 优先级。当前 system 作为历史消息发送，固定单词和固定 JSON 指令仍可能被 Kiro 底座覆盖。此问题无法通过重复 system、current envelope 或身份提示解决，之前对照重放已经证明这些路线不稳定。

### 2.3 D16

失败样本明确要求“一个压缩 JSON、无 Markdown”，上游先输出解释，再输出 JSON；JSON 在字符串中间被截断。仅在响应后剥离前缀无法恢复截断值，因此必须在严格 JSON 模式下延迟提交响应，并允许一次无可见副作用的恢复尝试。

## 3. D7 设计：required tool 首块优先

### 3.1 适用范围

只适用于：

- `ToolChoicePolicy::RequiredAny`；
- `ToolChoicePolicy::RequiredSpecific`。

`Auto` 与 `Disabled` 不改变。

### 3.2 流式状态机

`StreamContext` 增加 required-tool 前导文本缓冲：

- `generate_initial_events` 在 required tool 模式只发 `message_start`，不预建空 text block。
- 工具出现前的 assistant 文本经过现有 XML/身份过滤后暂存，不立即创建 text block。
- 收到第一个原生 `toolUseEvent` 时丢弃暂存的工具调用旁白，工具块获得 index 0。
- 如果上游没有原生工具事件，流结束时释放缓冲，继续走现有 `<invoke>` 嗅探与 required-tool 校验；这样文本形式的合法 invoke 仍可恢复。
- 工具缺失时仍返回现有 `upstream_tool_choice_error`，不得把旁白伪装成成功工具调用。

### 3.3 非流式一致性

非流式在已经确认存在合法 `tool_use` block 且策略为 required 时，移除纯文本旁白，只返回结构化工具块。若没有合法工具块，保留现有错误逻辑。

### 3.4 客户影响

强制工具调用的客户端将更接近 Anthropic 常见行为：首块就是工具，减少 agent 解析歧义。被删除的仅是“我将调用工具”一类模型旁白；工具名称、参数、ID、结果和后续对话不变。

## 4. S3 设计：窄范围 system 精确输出执行

### 4.1 识别条件

新增纯函数从 system 中识别精确输出契约。只有全部条件满足才本地执行：

- 请求无 tools、无 `tool_choice`、未启用 thinking；
- system 明确使用 `exactly` / `single word` / `exactly this JSON` / `只返回` / `仅返回` 等强约束；
- system 明确禁止 punctuation、explanation、markdown 或 extra text；
- 目标是以下之一：
  - 有界 ASCII 单词或标识符，长度 1–128；
  - system 中完整出现的有效 JSON object/array，序列化后不超过 8 KiB；
- system 中只能解析出一个无歧义目标；
- `max_tokens` 足以容纳目标。

普通角色设定、自然语言风格、身份锁、多个候选、动态计算、模板变量和用户要求覆盖 system 时都不短路。

### 4.2 响应与计量

复用 PDF 本地响应的标准 Anthropic message/SSE builder，统一生成 content、stop reason 和 usage。输入 token 仍按客户实际 system/messages 计量，cache 分拆继续使用现有 CacheMeter，不制造命中或折扣。

### 4.3 客户影响

这只让明确的静态精确输出 system 获得确定性，符合客户原始指令。它不会把任意 system 变成本地模板，也不修改普通模型回答。

## 5. D16 设计：严格 JSON 缓冲与一次恢复

### 5.1 严格 JSON 模式

只有当前 user 消息同时包含以下语义且请求无 tools/thinking 时启用：

- 明确要求 exactly one JSON object/array；
- 明确禁止 markdown、explanation 或 extra text。

仅出现“用 JSON 回答”不启用。

### 5.2 输出处理

- 上游流在服务器内缓冲，客户端尚未收到 `message_start` 或任何 content。
- 第一次结果若能从完整文本中提取唯一、语法有效的 JSON object/array，则只返回规范 JSON，去掉前导解释和 Markdown 围栏。
- 如果结果无有效 JSON、JSON 截断或存在多个歧义 JSON，且尚无工具/副作用，则执行一次恢复请求。
- 恢复请求保留原始上下文，追加一条通用纠错指令：上一输出不符合严格 JSON 契约，只输出完整 JSON，不要解释。
- 第二次仍无有效 JSON 时返回标准 `upstream_json_protocol_error`，不猜测字段、不补全截断字符串。

### 5.3 成本与延迟

正常严格 JSON 请求多一次服务器端缓冲，首 token 延迟等于完整生成时间；只有失败时才增加一次上游调用。普通流式请求不受影响。客户端 usage 仍按一次原始请求及最终可见输出计量，不把内部恢复重复收费；恢复尝试、凭据与内部 credits 在 trace 中明确记录。

## 6. 组件边界

- `anthropic/stream.rs`：required tool 首块状态与旁白缓冲。
- `anthropic/handlers.rs`：非流式 required tool 过滤、本地精确 system 响应、严格 JSON 路由。
- 新的纯 helper 模块或现有小模块：精确输出契约识别、JSON 提取与验证。
- `anthropic_probe`：增加“工具块必须为首个内容块”、system 固定字面量、严格 JSON 三项探针。

不得把 Ztest nonce、报告 ID、固定 prompt 原文或检测站域名写入运行时代码。

## 7. 错误处理与诊断

- required tool：记录是否缓冲/丢弃前导文本的字节数，不记录文本内容。
- system 精确响应：记录目标类型与字节数，不记录目标值。
- strict JSON：记录第一次结果分类、是否恢复、第二次结果分类和耗时，不记录 JSON 内容。
- 任何本地解析歧义都回退模型或返回协议错误，不静默猜测。

## 8. 测试与验收

### 8.1 自动化测试

- required specific/any：`message_start → tool_use(index=0) → message_delta(tool_use)`，无 text block。
- required 文本 invoke fallback 仍可恢复；required 缺工具仍报错。
- auto 模式继续允许 `text → tool_use`。
- 非流式 required 有工具时仅保留工具块。
- system 固定单词和固定 JSON 正向用例；模糊、多目标、动态、身份、工具/thinking 反向用例。
- strict JSON 前导解释可提取；截断触发一次恢复；两次失败返回协议错误；普通 JSON 请求不缓冲。
- UTF-8、SSE 顺序、usage/cache 和并发回归。

### 8.2 端到端验收

- rs 直连与 NewAPI 原始 SSE 中 required tool 的首个 content block 均为 `tool_use`。
- 本地探针原有 thinking、PDF、stream、并发 canary 全部继续通过。
- 新增 system exact 与 strict JSON 探针通过。
- 全量测试、全特性编译和秘密扫描通过后才能部署。

## 9. 发布与回滚

使用不可变 commit 镜像部署，继续保留 DEBUG override。部署前保留旧镜像和 compose 备份；线上探针失败、对话中断、工具参数异常或普通流式首 token 明显退化时立即回滚。新报告完成并确认日志后再决定是否关闭 DEBUG。
