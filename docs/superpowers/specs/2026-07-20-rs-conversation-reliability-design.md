# RS 客户对话工具可靠性修复设计

## 目标

修复生产错误日志中已经确认、由 RS 自身可安全处理且会中断客户对话的两类问题：工具参数使用等价别名导致 Schema 校验失败，以及同一条助手消息内出现完全相同的重复 `tool_use`。修复必须保持正常请求语义，不猜测业务参数，不改变首字、SSE、缓存、计费、身份映射或上游重试策略。

## 生产证据与根因

对 `43.225.196.10:18792` 上的生产容器、Nginx 日志、RS trace 和 `error_snapshots.db` 检查后，最近 24 小时内确认：

- `duplicate tool_use id` 约 381 次。抽样快照显示，客户端历史中的同一条 assistant message 连续携带两个 `id + name + input` 完全相同的工具块，下一条消息只有一个对应 `tool_result`。RS 当前把所有重复 ID 都判为无效历史，导致续轮请求在到达 Kiro 前直接 400。
- 工具 Schema 参数名不兼容约 157 次，包括 `name_path` 对 `name_path_pattern`、`content` 对 `contents`、`pattern` 对 `glob_pattern`、`query` 对 `pattern`。RS 已经有严格 Schema 验证和 `file_path` 对 `path` 的安全搬移，但其他已观测等价别名没有进入这套通用规则。
- 无法安全补全的错误仍然存在，例如 `Edit` 缺少 `file_path`、`read_file` 缺少路径。这些请求没有可搬移的原值，RS 不得猜测。

下列错误不属于本设计的代码修复范围：

- 封号、失效凭据造成的 403 鉴权失败。
- 正常限流造成的 429。
- RS 已换不同凭据重试后，Kiro 仍返回空内容或连续 120 秒无数据。
- 已持续输出正文后在约 300 秒由下游 Go 客户端主动取消、Nginx 记录 499 的连接。

## 方案

### 1. Schema 感知的等价别名搬移

在 `src/anthropic/tool_schema.rs` 的对象校验边界内，把现有 `file_path -> path` 特例收敛为一组受限别名规则：

| 来源字段 | 目标字段 | 已观测场景 |
| --- | --- | --- |
| `file_path` | `path` | Claude Code 文件工具兼容 |
| `name_path` | `name_path_pattern` | Serena `find_symbol` |
| `content` | `contents` | `Write`/文件写入工具 |
| `pattern` | `glob_pattern` | `Glob` |
| `query` | `pattern` | `Grep` |

只有同时满足以下条件才搬移现有 JSON 值：

1. 目标字段属于 Schema 的 `required`。
2. 输入中不存在目标字段。
3. Schema 声明目标字段，但没有声明来源字段。
4. 输入中存在来源字段。
5. 来源值的 JSON 类型符合目标字段声明的类型。

搬移后仍使用现有完整 Schema 校验检查 `enum`、`const`、长度、正则、嵌套属性和 `additionalProperties`。`validate_and_repair` 已使用副本事务化校验；后续约束失败时，原始输入不会被部分修改。

目标字段已经存在、Schema 同时声明两个字段、类型不匹配或目标不是必填时，一律不搬移，继续按原来的严格规则返回错误。规则不按工具名硬编码，因此只由客户声明的 Schema 决定是否成立。

### 2. 同消息内完全重复的 `tool_use` 去重

在 `src/anthropic/tool_history.rs` 的 ID 规范化和配对验证前，对每一条 assistant history message 单独检查工具块：

- 第一次出现的 ID 保留。
- 只有后续块的 `id`、`name` 和完整 JSON `input` 与第一次完全相同时，删除后续副本。
- 同一 ID 但工具名或输入不同，继续返回 `DuplicateToolUseId`，防止不明确地选择一个可能有副作用的调用。
- 同一 ID 在不同 assistant messages 中复用，继续报错。
- `tool_result` 仍只能出现一次，并继续执行现有孤立、重复和时序校验。

这一处理不会让工具执行两次，也不会合成任何工具结果；它只把已经重复序列化的同一个历史块恢复成一份。

### 3. 可观察性

现有 Schema 修复日志已经只记录工具名和修复路径，不记录客户参数值，继续沿用该行为。历史完全重复块被去重时新增一条结构化 warning，只记录去重数量，不写 `input` 正文。

仍然无法安全修复的 Schema 错误继续进入现有 `upstream_tool_schema_error` 快照；冲突重复 ID、孤立结果和重复结果继续进入请求转换错误，保留定位能力。

## 数据流

1. 客户请求转换为 RS 的 Kiro conversation history。
2. 历史 ID 规范化入口先删除同消息内完全相同的重复工具块，再执行全局 ID 唯一性、非法字符规范化和 `tool_result` 配对验证。
3. Kiro 返回工具调用后，流式与非流式路径继续共用 `validate_and_repair`。
4. Schema 对象校验在满足全部安全条件时搬移别名值，然后执行完整约束验证。
5. 校验成功才向客户交付工具调用；无法确定正确性的输入保持失败关闭。

## 客户影响

正面影响：

- 已观测的等价参数名不再让工具调用在交付前中断。
- 客户历史中由客户端重复序列化的同一个工具块不再让下一轮直接 400。
- 现有非法工具 ID 规范化、工具结果配对和 Schema 严格校验保持有效。

明确不变：

- 不改对话正文、system、身份回复或模型映射。
- 不改 SSE 事件顺序、提前握手、ping 或首字延迟。
- 不改缓存创建、缓存读取模拟或 token 计费拆分。
- 不增加 Kiro 重试次数，不延长最坏等待时间。
- 不自动补路径、正文、用户选择等缺失业务值。

## 测试设计

测试遵循先失败后实现：

### `tool_schema.rs`

- 为四个新增别名分别写成功搬移测试。
- 验证现有 `file_path -> path` 仍然工作。
- 验证目标已存在时不覆盖。
- 验证 Schema 声明来源字段时不搬移。
- 验证来源类型与目标类型不匹配时不搬移。
- 验证目标不是 required 时不搬移。
- 验证搬移后违反目标约束时整体失败且原始输入保持不变。

### `tool_history.rs`

- 同一 assistant message 内完全相同的重复块保留一份并与单个结果成功配对。
- 同一 ID 但 name 不同继续拒绝。
- 同一 ID 但 input 不同继续拒绝。
- 不同 assistant messages 复用同一 ID 继续拒绝。
- 重复 `tool_result`、孤立结果、非法 ID 规范化的现有测试继续通过。

### 回归

- `cargo fmt --check`
- `cargo test anthropic::tool_schema`
- `cargo test anthropic::tool_history`
- `cargo test`
- 构建 Linux x64 测试镜像并部署到隔离公网 `8991`。
- 用包含历史重复块和四类参数别名的请求验证不再中断；冲突重复块和缺失不可推断字段仍返回明确错误。

## 验收标准

1. 生产快照中已观测的四种字段别名在满足相同 Schema 条件时可正常交付工具调用。
2. 同消息内完全相同的重复工具块不再导致续轮 400。
3. 任何冲突或缺失业务值都不会被 RS 猜测或静默覆盖。
4. 完整 Rust 测试通过，既有工具配对与 Schema 约束测试无回归。
5. `8991` 验证期间首字、SSE、缓存和正常文本对话行为不发生变化。
