# API 一致性与请求隔离优化设计

## 背景

Ztest 报告 `01KX8GA5Q5BYAC7RDD2FVRYPDW` 的综合分为 65。主要异常包括：并发探针返回无关内容、Canary 未回响、system 服从率为 33%、Token 开销随输入异常增长，以及模型自报 Kiro。

本次优化只修复真实的协议转换、请求隔离、上下文保真和计量问题。不会伪造上游身份、识别检测探针后返回特制结果，也不会承诺固定检测分数。由于真实上游是 Kiro，检测平台仍可能把“非官方直连”作为硬性扣分项。

## 目标

1. 并发请求之间不共享上游会话状态，不出现响应串扰。
2. 客户端 system 内容按原顺序进入转换结果，不被无条件追加无关策略。
3. 转换器不凭空生成会影响模型行为的 assistant 历史文本。
4. Token 用量字段继续反映真实输入、缓存和输出数据。
5. 用自动化测试覆盖并发隔离、system 保真与历史转换行为。

## 非目标

- 不隐藏或改写上游 Kiro 身份。
- 不针对 Ztest 的 nonce、探针名称或请求特征增加特殊分支。
- 不修改 Admin UI 视觉设计。
- 不重构与本问题无关的凭据、代理池或调度模块。

## 方案选择

采用“请求隔离 + system 保真”的中等改造方案。

最小方案只随机化 `conversationId`，不能解决 system 被附加策略和伪造历史文本的问题。深度重构整个协议层成本高、回归面大。本方案覆盖报告中最强的异常证据，同时把改动限制在 Anthropic 转换器及相关测试。

## 详细设计

### 请求隔离

Anthropic Messages 请求是无状态的：完整对话历史由客户端随请求提供。因此，每次转换都生成新的 `conversationId` 和 `agentContinuationId`，不再从 `metadata.user_id` 提取可复用的上游会话 ID。

`metadata.user_id` 仍可用于日志、审计或本地统计，但不得参与 Kiro 上游会话标识生成。这样即使检测器或客户端并发发送相同 `user_id`，请求也不会被 Kiro 视为同一会话的并行续写。

### system 保真

保持现有 Kiro 协议所需的 system 历史表示方式，但 system 文本由客户端提供的文本块按顺序连接，不再无条件追加 `SYSTEM_CHUNKED_POLICY`。

写入/编辑工具的大小限制应通过工具描述和工具执行层约束，而不是污染所有请求的 system。现有 Write、Edit、Bash 工具描述后缀继续承担工具级约束。

thinking 兼容前缀仍按请求参数生成，但必须只出现一次，且不能改变客户端 system 文本的相对顺序。

### 历史消息

删除转换器凭空加入的 `I will follow these instructions.` 和尾部 `OK`。只有客户端实际提交的 user/assistant 内容才进入历史。

如果 Kiro 协议要求 user/assistant 交替，则优先通过相邻同角色消息合并满足结构约束。若上游确实拒绝缺少配对的历史结构，应返回明确转换错误，不能伪造模型回复。

### Token 与缓存计量

不为提高检测分数修改或压低 reported token。继续使用真实的 `input_tokens`、`cache_creation_input_tokens`、`cache_read_input_tokens` 和 `output_tokens`。

新增测试确认 system 文本长度只产生可解释的线性变化，不因转换器重复附加策略而扩大。Kiro 平台自身固定提示产生的开销不在本项目伪装或扣除。

### 性能稳定性

请求隔离使用本地 UUID，不增加网络往返。保留现有 HTTP client 连接池，因为连接复用与会话状态复用是两件独立的事：前者减少握手延迟，后者必须按请求隔离。

本轮不改变凭据负载均衡和故障转移策略。若复测仍显示延迟波动，再依据 trace 数据单独诊断调度问题。

## 测试设计

在转换器测试中增加或调整以下用例：

1. 两个具有相同 `metadata.user_id` 的请求得到不同 `conversationId`。
2. 客户端 system 文本在转换后保持原文，不包含内部 chunked policy。
3. thinking 前缀只注入一次，并位于 system 内容之前。
4. 无 system 的单轮请求不会生成额外历史消息。
5. 多轮请求只包含客户端提供的历史内容，不出现自动 `OK` 或确认语。
6. 同一请求重复转换时业务内容一致，只有请求隔离 ID 不同。

验证命令：

```text
cargo fmt --check
cargo test
cd admin-ui && bun run build
git diff --check
```

## 风险与回退

- Kiro 某些端点可能依赖伪造的交替历史。测试和本地请求若发现协议拒绝，将把兼容逻辑收敛为端点专属适配，而不是恢复全局文本注入。
- 随机化 `conversationId` 会停止利用 Kiro 服务端隐式续写，但 Anthropic/OpenAI 兼容 API 本就由客户端携带完整历史，因此不应损失正常多轮能力。
- 删除内部 system 策略可能改变超大 Write/Edit 调用行为；工具描述中的现有限制仍会保留并由回归测试覆盖。

回退时可以按独立提交撤销转换器改动，不需要迁移配置或数据文件。

## 验收标准

- 所有新增回归测试通过。
- 现有 Rust 测试、格式检查和 Admin UI 构建通过。
- 构造并发相同 `metadata.user_id` 请求时，转换结果的会话 ID 不同且历史内容互不混入。
- system 转换结果不含客户端未提交的策略或确认文本。
- 若使用用户提供的临时 Key 复测，普通请求与检测请求走完全相同的代码路径。
