# Ztest 剩余问题修复设计

> 2026-07-12 生产重放补充：本文早期关于 D7 与 D19 的候选方案已被后续证据收敛。以下“证据修订”优先于后文中相冲突的描述。

## 0. 生产重放后的证据修订

### D19：不是 PDF 解析失败，也不是偶发空响应

生产 trace 与最小重放确认，文本型 PDF 已被正确提取，唯一标识符也完整出现在发往 Kiro 的当前消息中。相同请求连续五次均返回零 assistant 内容；删除 Claude Code system、删除 document 包装、调整指令位置和更换普通前缀都不能恢复响应。只有直接要求回复已知标识符时能成功。

因此根因是 Kiro 对“从上下文提取疑似标识符”的语义稳定静默过滤。零输出重试只能处理偶发故障，不能解决此样本。修复改为严格限定的通用确定性提取：仅当请求包含文本型文档、用户明确要求只返回一个给定形状的 ASCII 标识符、且文档中恰有一个匹配时，本地返回该值。多个候选、模糊要求、扫描 PDF 和普通文档问答继续走模型。实现不得识别 Ztest 报告 ID、固定 nonce 或检测站专用文案。

### D7：协议路径已稳定，暂不扩大打捞条件

rs 直连、NewAPI 流式/非流式、指定工具、任意工具和 24 路并发重放均返回一致的 `text + tool_use`，没有复现 `stop_reason=tool_use` 但只有文本的形态。现有 required-tool 护栏也会在缺少工具块时返回错误，因此本轮没有证据支持修改工具协议或扩大 inline XML 打捞。

本轮只增加不记录参数值的事件级 DEBUG：上游工具分片的 id/name/stop/input 字节数、实际发出的工具块 index/id/name/input 字节数，以及终态 emitted tool names、stop reason 和 terminal error。待再次捕获异常后再依据事件链修协议。

### S1：本地估算器存在确定的分段放大

生产未配置远程 count_tokens。当前本地估算器对不足 100 个基础 token 的输入乘以 1.5，并在 100、200、300、800 附近使用不同倍率，直接造成斜率放大和边界跳变。这不是上游追加 prompt。修复为 `ceil(char_units / 4)`，空文本仍由调用方的总量下限保护；输入与输出使用同一诚实估算口径。

### D5/S3：只移除 Claude Code 与 Kiro 冲突的身份锚点

对照重放证明，仅删除 `You are Claude Code, Anthropic's official CLI for Claude.` 后，canary 与固定 JSON 指令恢复；普通固定单词仍可能被 Kiro 底座压过。修复仅在 `ClaudeCode` 兼容模式删除这一精确身份行并保留同一 system 的其余内容。`Raw` 模式原样透传。不把 system 复制到 current，不伪造上游没有的 system 优先级。

### 流式身份归一化

非流式已经能替换完整身份描述，流式仅跨 chunk 处理整词 `Kiro`，因此会产生 `I'm Claude, an AI-powered development environment`。流式过滤器改为缓存所有已知源短语的最长尾部前缀，并在 UTF-8 字符边界上归一化，工具 JSON 与用户输入仍不进入此过滤器。

## 1. 目的与结论

本文只定义修复方案，不修改运行代码。依据报告 `01KXAQV1DW09HZ7HRHFQMRQDW1`、生产 trace 和本地重放结果，当前剩余问题不能按同一种方式处理：

- **D7 工具调用协议错误：可修，优先级 P0。** 报告出现 `stop_reason=tool_use`，但 `content` 只有文本、没有 `tool_use` 块。这会真实影响 Claude Code、LangChain 和自研 Agent，应该修。
- **D19 PDF 空响应：可修，优先级 P0。** 不是 PDF 解析失败，而是上游返回零助手内容后，流式 HTTP 头已经发出，客户端最终只得到 HTTP 200 和空答案。可通过首个可见事件前的受限重试改善。
- **D3 身份文案：可低风险改善，优先级 P1。** 当前流式过滤只处理整词 `Kiro`，可能留下 “Claude, an AI-powered development environment” 这种不自然的组合。应改为精确身份锚点归一化，不能全量改写客户正文。
- **D5 Canary / S3 system 服从：只能部分改善，优先级 P2。** Kiro 上游没有与 Anthropic 等价的原生 system 槽，客户 system 的优先级低于上游底座指令。可以用映射策略灰度提高服从率，但无法保证与官方 Anthropic 一致。
- **S1 Token 注入：只核对口径，不伪造。** 当前报告为 `slope=1.9, overhead=44`，本地探针未发现此前约 6000 token 的固定注入。若真实总量正确，不为提分改写 usage。
- **D11 隐式身份 / 反向通道嫌疑：不能诚实地彻底消除。** 请求确实经过 Kiro 网关。可以修协议错误和错误身份文案，但不应伪装成 Anthropic 官方直连。

因此，**可以修复会影响客户使用的 D7、D19，并改善身份文本；D5/S3 只能做可回滚的兼容实验；S1/D11 不采用数据造假或检测特判。** 不能承诺固定 98 分，外部评分还受 Kiro 上游行为和检测样本波动影响。

## 2. 已确认的报告证据

| 项目 | 报告现象 | 当前判断 |
| --- | --- | --- |
| D7 工具调用 | HTTP 200、`stop_reason=tool_use`，但 `raw_content_types=["text"]`，没有工具块 | Anthropic 响应结构不自洽，属于真实协议 Bug |
| D19 PDF | HTTP 200，`answer=""`，`note="empty_response"` | trace 显示 `upstream returned no assistant content`，是流式握手过早与上游空响应共同导致 |
| D5 Canary | 要求原样回复 nonce，模型因身份冲突拒绝 | 上游底座指令压过了降级后的客户 system/user 指令 |
| S3 system | 服从率 33%，模型回复常识答案或身份元评论 | system 映射优先级不足，不是 HTTP 或 JSON 丢字段 |
| D3/D11 身份 | 出现 “I'm Claude, an AI-powered development environment” 等混合文案 | `Kiro` 整词替换与流式短语处理不完整造成语义残留 |
| S1 Token | `slope=1.9, overhead=44` | 大固定注入基本消失，剩余斜率需核对真实 token 统计链路 |

生产 trace 对 D19 的对应记录为：`stream=true`、`final_status=error`、`error_type=bad_request`、`error_message=upstream returned no assistant content`、`output_tokens=0`。这说明检测站看到的“空答案”不是项目成功生成了空文本，而是客户端在 HTTP 200 流中没有识别项目随后发出的错误事件。

## 3. 候选路线

### 路线 A：协议正确性优先（推荐）

先修 D7 和 D19 两个真实客户 Bug，再收窄身份归一化；system 映射只增加配置与灰度探针，不立即改变全量默认行为。

优点是客户收益明确、回归面可控，不依赖 Ztest 的 nonce 或报告 ID。缺点是 D5/S3 和 D11 可能仍受上游限制，分数不会一次性达到目标。

### 路线 B：system 强化优先

直接把所有 system 内容复制或嵌入当前 user，提高检测样本的指令权重。

短期可能改善 D5/S3，但会改变现有多轮对话、工具决策和缓存前缀，增加 token，甚至让 Claude Code 的上下文边界变得混乱。**不建议全量采用。**

### 路线 C：输出与 usage 整形优先

通过全局文本替换、固定响应或改写 token/cache 数值追求评分。

这种做法会误改客户讨论 Kiro 的正常内容、造成计费不实，也可能专门适配检测样本。**明确排除。**

## 4. D7 工具调用修复设计

### 4.1 问题边界

本地对 `get_weather` 的 `auto`、`any`、指定工具和 8 路并发重放均成功，说明错误不是每次必现。报告中的模型先输出“我会调用工具”的叙述文本，之后响应被标记为 `tool_use`，但没有结构化工具块。当前 `<invoke>` 打捞要求标签位于行首；若上游在同一行先输出叙述再输出完整 `<invoke>`，可能漏捞。

### 4.2 推荐修法

1. 增加不记录 prompt 和参数值的结构化诊断，仅记录：工具是否声明、`<invoke>` 是否完整、参数 JSON 是否有效、是否通过 schema、是否位于代码围栏、未打捞原因。
2. 将 inline `<invoke>` 的打捞条件从“必须位于行首”调整为以下条件全部成立：
   - 工具名存在于本次请求声明的 tools 中；
   - `<invoke>` 完整闭合；
   - 参数是合法 JSON；
   - 参数符合该工具的 JSON Schema；
   - 标签不在 Markdown 代码围栏或用户引用示例中。
3. 增加协议不变量：
   - 只有实际发出了至少一个 `tool_use` content block，才允许返回 `stop_reason=tool_use`；
   - `tool_choice=any` 或指定工具时，若上游最终没有合法工具块，返回明确的上游协议错误，不伪装成成功文本；
   - `tool_choice=auto` 没有合法工具块时，可按普通文本以 `end_turn` 结束，但不能标记 `tool_use`。
4. 将扩展打捞放在配置开关后先灰度；确认无误触后再决定是否默认启用。

### 4.3 客户影响

正面影响：

- 修复 Agent 卡在“模型说要调用工具、协议却没有工具块”的问题；
- `stop_reason` 与 content 一致，SDK 不再进入错误的工具续聊状态；
- `tool_choice=any` 和指定工具失败时能得到可诊断错误。

风险：

- 如果打捞条件过宽，客户让模型讲解 `<invoke>` 示例时可能被误执行；
- 严格 schema 校验可能暴露以前被宽松接受的错误参数；
- required 工具从“返回一段文本”变为明确错误，部分依赖旧错误行为的客户端会感知变化。

控制方式：必须同时使用已声明工具、完整闭合、合法 JSON、schema 校验和代码围栏排除；先灰度并保留一键回滚开关。不能只凭工具名执行。

## 5. D19 PDF/空响应修复设计

### 5.1 问题边界

报告中的 PDF 请求已经到达 Anthropic streaming 入口，但 Kiro 上游没有返回任何 assistant text、thinking 或 tool event。当前实现先发送 HTTP 200/SSE 响应头，流结束后才知道内容为空，因此已经无法把 HTTP 状态改为 502/503；检测站也可能忽略流内 `error` 事件，只呈现空答案。

这次失败不能证明 PDF 文本提取本身损坏。它首先是“上游零内容”稳定性问题，普通请求也可能遇到。

### 5.2 推荐修法

1. 在向客户端发送第一个可见 SSE frame 前，等待上游出现首个有效 assistant 内容或明确失败。
2. 仅当本次尝试是**零可见输出**时，换一个健康凭据重试一次；已经向客户发出任何 text、thinking 或 tool block 后绝不重试，避免重复内容或重复工具调用。
3. 最多重试一次，并复用原始请求语义；记录首个凭据、重试凭据、空响应原因和耗时，但不记录 PDF 内容。
4. 两次都为空时，在尚未提交 HTTP 响应的前提下返回标准 Anthropic error 和合适的 502/503；若底层框架无法延迟 HTTP 提交，则至少保证首帧为标准 SSE error，而不是成功终止。
5. 重试只覆盖可安全重放、尚未产生任何可见副作用的请求。若未来上游可能在无可见输出前已经执行服务端副作用工具，必须禁用重试。

### 5.3 客户影响

正面影响：减少 PDF 和普通请求偶发 HTTP 200 空答案，错误也更容易被 SDK 正确识别。

代价与风险：

- 正常流式请求的首 token 会增加一小段缓冲延迟；
- 空响应时会多一次上游请求，增加延迟和潜在成本；
- 如果错误地在已有输出后重试，可能重复文本或工具调用，因此“零可见输出”是硬条件。

建议配置最大首帧等待时间和单次重试开关，并在生产灰度期间监控首 token 延迟、空响应率和重试成功率。

## 6. D5/S3 system 映射设计

### 6.1 不能完全修复的原因

Kiro 上游没有 Anthropic 原生 system 槽。客户传入的 system 当前只能转换到对话历史中，而 Kiro 服务端自身的底座 system 具有更高优先级。当客户 system 要求模型采用与 Kiro 包装身份冲突的措辞时，上游可能拒绝、解释身份冲突或忽略指令。

中转层无法在不控制上游模型配置的情况下提供与官方 Anthropic 完全相同的 system 优先级，因此这里的目标是“提高一般指令服从率且不破坏现有客户”，而不是保证检测样本 100%。

### 6.2 三种映射模式

| 模式 | 行为 | 优点 | 客户风险 |
| --- | --- | --- | --- |
| `history` | 保持当前 system 作为历史上下文映射 | 兼容性最好、token 最少 | 服从率可能维持现状 |
| `current` | 将 system 与当前 user 放入一个有明确边界的当前轮 envelope | 更接近当前决策位置，可能提高服从率 | 改变对话行为、工具选择与缓存前缀 |
| `hybrid` | 历史保留，同时在当前轮重复 system | 指令最显著 | 重复 token、缓存命中下降、模型可能困惑，不推荐 |

推荐新增可配置的 `history/current` A/B 能力，默认先保持 `history`；仅对测试组或指定 API key 使用 `current`。`hybrid` 只作为诊断选项，不进入生产默认值。

### 6.3 客户影响与验收门槛

切换 `current` 前必须验证：

- Claude Code 长对话不会重复身份元评论或丢失历史；
- 单工具、并行工具、`tool_result` 续聊均正确；
- 中文、多段 system、cache_control、1M 上下文正常；
- input/cache_creation/cache_read 的统计仍反映真实请求，不把重复文本隐藏在 usage 外；
- 与 `history` 相比，首 token、总 token、缓存命中和工具成功率没有不可接受的退化。

只有 A/B 数据证明 system 服从明显改善且客户回归可接受，才考虑逐步扩大。任何阶段均能按 API key 或全局配置回退到 `history`。

## 7. 身份归一化修复设计

### 7.1 推荐修法

当前非流式路径已有已知描述短语替换，但流式路径主要只跨 chunk 处理整词 `Kiro`。推荐把身份处理收窄为“完整身份锚点”，并让流式过滤支持跨 chunk 的有限窗口，例如：

- `I'm Kiro, an AI-powered development environment` → `I'm Claude, an AI assistant`
- `Kiro, an AI-powered development environment` → `Claude, an AI assistant`
- 已知的 `made/built/created by AWS` 身份自述 → 对应的中性 Claude/Anthropic 表述

同时逐步取消自由文本中的整词 `Kiro` 全量替换。仅当它处于第一人称身份自述附近时改写；客户讨论 Kiro IDE、文件名、变量名、代码字符串时必须原样保留。

### 7.2 不得触碰的内容

- 用户输入和 system 原文；
- `tool_use.input` JSON、工具名、`tool_result`；
- 代码块、文件路径、变量名；
- 与身份无关的 AWS、Amazon、Kiro 技术讨论。

### 7.3 客户影响

整体风险较低，能消除 “Claude 是开发环境” 之类的怪异文案，并减少误改客户内容。流式实现必须按 Unicode 字符边界处理，覆盖中文/emoji 与短语跨多个 chunk 的测试，避免再次出现 UTF-8 slice panic。关闭 `identity_normalization` 时应完全透传，作为回滚路径。

## 8. S1 Token 与缓存计量原则

当前应做的是审计，不是“优化数字”：

1. 对同一请求分别记录客户原始 prompt 的本地估算、实际发往 Kiro 的序列化文本估算、上游计量和对外 Anthropic usage。
2. 确认工具 schema、system 映射、PDF 提取文本没有被重复计入。
3. 确认 streaming 的 `message_start` 与 `message_delta` usage 总量一致。
4. 如果 `current` system 映射增加真实输入，必须如实反映在 token 中，不能为了分数隐藏。
5. cache_read/cache_creation 只能反映真实可复用前缀，不设置固定命中率，不把冷启动伪装成缓存命中。

客户影响：纯审计无行为影响；若发现重复序列化并去除，可降低真实 token。伪造 usage 会影响计费和客户信任，因此不纳入方案。

## 9. D11 与反向通道边界

服务的真实架构是 Anthropic 兼容入口转发到 Kiro 网关，而不是 Anthropic 官方 API 直连。可以做到：

- 对外响应严格遵守 Anthropic content、SSE、tool 和 error 协议；
- 不让 Kiro 的错误品牌提示污染普通助手输出；
- 如实记录模型能力差异和上游限制。

不能合理承诺：

- 消除所有能证明请求经过 Kiro 的行为差异；
- 伪造官方签名、thinking、缓存命中、模型身份或 token；
- 针对 Ztest nonce、报告 ID、固定提示词返回特制答案。

因此 D11 和“反向通道嫌疑”只能随通用协议质量改善而下降，不能作为单独的输出伪装任务。

## 10. 实施顺序与发布策略

### 阶段 P0：真实可用性

1. 为 D7 增加失败用例、结构化诊断和响应协议不变量。
2. 在安全校验齐全的前提下支持 inline `<invoke>` 打捞，先关闭默认开关进行本地验证。
3. 为 D19 增加“首帧前零内容换凭据重试一次”，覆盖普通文本和 PDF 请求。
4. 验证工具续聊、并行工具、SSE 事件顺序和 UTF-8。

### 阶段 P1：低风险身份文本

1. 增加完整身份锚点测试和流式跨 chunk 测试。
2. 收窄整词替换范围，保持工具 JSON 与客户代码完全不变。
3. 通过配置灰度，检查生产 panic 和输出误改。

### 阶段 P2：system A/B

1. 实现 `history/current` 配置但保持生产默认 `history`。
2. 使用本地探针和测试 API key 对比服从率、工具成功率、总 token、缓存和首 token 延迟。
3. 数据达到门槛后小流量灰度；不达标则保留 `history`，记录为上游限制。

### 明确不做

- 不识别 Ztest 报告、nonce 或检测专用提示词；
- 不生成或补写不存在的 thinking；
- 不伪造 token、缓存命中、模型签名或官方直连身份；
- 不以牺牲工具调用和客户对话稳定性换取单次评分。

## 11. 测试与验收

### 自动化回归

- D7：叙述文本后同行完整 invoke 能安全打捞；代码围栏、引用示例、未知工具、坏 JSON、schema 不匹配均不得执行。
- D7：只有 text 时 `stop_reason=end_turn`；存在真实工具块时才是 `tool_use`；required 工具缺失时返回标准错误。
- D19：首个凭据零内容、第二个成功；两个均为空；首个已有一个字符后断流时绝不重试。
- PDF：文本型 PDF token 可提取；扫描 PDF 返回明确不支持或正常模型说明，不伪装为空成功。
- 身份：完整短语跨 1 至多个 chunk；中文和 emoji 位于切片边界；工具 JSON 与代码中的 `kiro` 不变。
- system：`history/current` 多轮、tool_result、并行工具、中文和 cache_control 回归。
- usage：非流式与流式总量一致，冷缓存与热缓存口径真实。

### 本地与生产灰度

- 本地 Anthropic 探针全部通过，尤其是 `tool_choice`、PDF、并行 canary、stream 完整性和工具续聊。
- 对 D7 失败形态至少做多轮和并发重放，不能只验证单次成功。
- 灰度观察空响应率、重试成功率、首 token P50/P95、工具协议错误率、UTF-8 panic 和单请求 token。
- 新版本使用不可变 commit 镜像部署；保留旧 digest，出现对话中断、重复工具或成本异常时立即回滚。

## 12. 客户影响总表

| 改动 | 预期收益 | 可能代价 | 默认建议 |
| --- | --- | --- | --- |
| D7 协议不变量 | 防止工具调用后对话断开 | required 失败会从文本变成明确错误 | 默认启用 |
| D7 inline 安全打捞 | 提高上游 XML 泄漏恢复率 | 校验不严会误触工具 | 先灰度，验证后启用 |
| D19 零输出重试一次 | 减少 HTTP 200 空答案 | 空响应时增加延迟和一次上游调用 | 默认启用，可配置关闭 |
| 精确身份锚点 | 消除怪异品牌混合文案 | 流式缓冲略增，需防误改 | 默认启用，保留总开关 |
| system `current` | 可能提高指令服从率 | 改变对话/工具/缓存并增加 token | 仅 A/B，不直接全量 |
| token/cache 审计 | 保证计费一致并发现重复输入 | 无直接提分保证 | 执行审计，不整形 |

## 13. 审批后决策

若本文获批，下一步应先编写分阶段实施计划，不直接把所有改动一次合入。第一批只包含 D7 协议不变量、受保护的打捞诊断与 D19 零输出重试；身份归一化和 system A/B 分开提交、分开部署、可独立回滚。
