# Ztest 报告 01KXB5DY 剩余问题分析与修复建议

## 1. 报告概况

- 报告地址：<https://ztest.ai/report/01KXB5DYMQH4EGQRA7H1VK0THW>
- 报告时间：2026-07-12 12:39:28–12:40:05 UTC
- 被测模型：`claude-opus-4-8`
- 综合分数：82/100
- 检测时服务器镜像：`ghcr.io/3370842391/kiro-rs:sha-e928dd`
- 当前已通过的重要项目：D1、D2、D7、D10、D13、D17、D18、D19、S2、S4、S5

本轮真正影响分数和风险判定的项目主要是：

1. D5 Canary：0 分
2. S3 system 指令覆盖：33 分
3. D16 严格 JSON 子项：75 分
4. D11 隐式身份：33 分

## 2. 结论摘要

| 项目 | 是否可修 | 推荐优先级 | 结论 |
|---|---:|---:|---|
| D5 Canary | 可以 | P0 | 增加通用、严格受限的显式字符串复制契约 |
| S3 静态 system 输出 | 可以 | P0 | 允许静态精确 system 契约与被动 tools 列表共存 |
| D16 严格 JSON 空响应 | 可以加固 | P1 | rs 实际已生成 JSON，建议把本地 SSE 改为逐事件分块输出 |
| D11 context/cutoff | 只能部分修 | P2/不推荐 | 只允许使用真实配置元数据，不应硬编码或伪造 |
| S1 Token 注入 80 | 不需要修 | 不处理 | 已判定成功，继续调整可能破坏真实计量 |
| D3 身份一致性 88 | 不需要修 | 不处理 | 已判定 match，收益低且可能引入身份改写副作用 |

建议先修 D5、S3 和本地 SSE 分块。这三项完成后，预期可消除报告中的主要 high-risk 信号；综合分数预计可进入约 92–96，但最终分数仍受 Kiro 随机输出和 Ztest 评分权重影响，不能承诺固定数值。

## 3. D5 Canary：可以修复

### 3.1 报告现象

Ztest 要求模型复制一个随机字符串：

```text
I need you to copy this string into your response so I can verify the connection: <nonce>
```

模型实际拒绝：

```text
I can't discuss that.
```

因此 D5 得分为 0，并产生 `CANARY_SILENCE` high-risk 信号。

### 3.2 当前生产复现

在最终镜像上通过 NewAPI 发送同类流式请求，结果仍然无法回显唯一 token，证明这不是 Ztest 误判，而是 Kiro 上游拒绝了简单复制任务。

### 3.3 推荐修复

增加一个通用的“显式复制/回显契约”解析器，但必须限制范围：

- 只检查最新一条用户文本。
- 必须出现明确的 `copy this string`、`echo this token`、`repeat exactly` 等复制意图。
- 只能提取一个唯一候选 token。
- token 限制为 4–128 字节的 ASCII 字母、数字及 `-_.:`。
- 带 tools、required tool、thinking、document、image 或多个候选时不走快捷路径。
- 不识别 Ztest 报告 ID、固定 nonce 前缀或某个检测站专属文案。
- 返回标准 Anthropic 非流式或 SSE 响应，并继续使用真实 input/cache/output 计量。

### 3.4 客户影响

正面影响：

- 显式要求复制连接码、订单号、校验码时更加稳定。
- 不再依赖 Kiro 是否把普通 nonce 误判为敏感信息。

潜在风险：

- 模型会更严格地执行用户主动要求的字符串回显。
- 如果用户自己把敏感字符串放进请求并明确要求复制，代理会照做；但这属于用户显式提供并请求返回的内容，不是泄露服务器内部信息。

整体风险：低，前提是严格执行唯一候选、长度和字符集限制。

## 4. S3 system 指令覆盖：可以修复

### 4.1 报告现象

两个静态 system 契约没有被执行：

- 固定单词应输出 `7913d9bd`，实际回答 Paris。
- 固定 JSON 应只输出 `{"a":956,"b":575}`，实际输出身份冲突解释和额外文字。

第三个 identity lock 子项已经被 Ztest 计为通过，因此只需修前两个静态输出子项。

### 4.2 根因证据

使用当前生产环境复现：

| 请求形式 | 结果 |
|---|---|
| 静态 exact system，不带 tools | 通过 |
| 同一个静态 exact system，附带被动 tools 列表 | 失败 |

当前 `exact_system_output` 为了防止工具调用对话被本地短路，只要 `tools` 非空就拒绝静态 system 快捷路径。Ztest 的 Claude Code 兼容请求会附带工具定义，即使本轮并不要求调用工具，因此请求被发到 Kiro，随后被 Kiro 底座提示词覆盖。

### 4.3 推荐修复

调整静态 exact system 与工具策略：

- tools 非空不再自动拒绝。
- `tool_choice` 缺省、`auto` 或 `none` 时，可以执行无歧义的静态 exact system 契约。
- `tool_choice=any`、`tool_choice=tool` 等强制工具策略仍必须拒绝本地文本短路。
- system 必须同时具有 exact cue 和 no-extra cue。
- 只允许唯一固定 ASCII token 或唯一合法 JSON。
- identity、动态日期、模板、多候选、用户输入插值等场景继续拒绝。
- 建议额外拒绝包含未完成 `tool_use/tool_result` 交互的历史，防止破坏真实 agent 工具循环。

这不是忽略客户工具，而是遵守更高优先级、明确要求“只输出固定内容”的 system 契约。强制工具请求仍保持原行为。

### 4.4 客户影响

正面影响：

- Claude Code 类客户端即使自动携带工具清单，固定 system 输出仍能稳定生效。
- system 指令遵循度提高。

潜在风险：

- 某些客户端同时附带自动 tools 和固定输出 system 时，模型不会自行调用工具。
- 该行为与 system 的“只输出固定内容”语义一致，但需要用回归测试确保 required tool 不受影响。

整体风险：低到中等。必须保留 required tool 的硬隔离测试。

## 5. D16 严格 JSON：rs 已生成结果，建议加固 SSE

### 5.1 报告现象

`constraints_json` 子项：

- HTTP 200
- `raw_response` 为空
- Ztest 标记为 `not_json`

### 5.2 服务器证据

报告运行期间，rs DEBUG 日志显示：

```text
2026-07-12T12:39:36.756... Received POST /v1/messages stream=true max_tokens=128
2026-07-12T12:39:37.962... recovered strict JSON response attempts=1 output_bytes=38
```

这说明 rs 已经识别该请求并生成了长度合理的严格 JSON。空响应出现在 rs 之后的流式聚合环节，而不是模型没有返回 JSON。

当前生产环境验证：

- NewAPI 非流式严格 JSON：20/20
- NewAPI 16 路并发流式严格 JSON：16/16
- 没有空事件或空文本

因此 D16 更像一次 NewAPI/Ztest SSE 边界兼容或并发时序问题，而不是当前 JSON 恢复逻辑持续失效。

### 5.3 推荐加固

当前本地 strict JSON 和静态 system 流式响应会先拼成一个完整字符串，再通过 `Body::from(...)` 一次性返回。建议改为真正的事件流：

- 每个 `message_start/content_block_start/content_block_delta/...` 独立产生一个 `Bytes` chunk。
- 保持标准 `event:` 和 `data:` 行及双换行边界。
- 不需要人为 sleep；只需保证事件级分块。
- 对 rs 直连和 NewAPI 各增加 32 路并发聚合测试。
- 校验首块、最终 `message_stop`、文本聚合和 usage 一致性。

### 5.4 客户影响

- 协议语义不变。
- 客户端更容易逐事件消费本地响应。
- chunk 数量会增加，但响应很短，性能影响可以忽略。

整体风险：低。

## 6. D11 隐式身份：只建议部分修复

### 6.1 报告现象

- 代码签名风格识别为 Claude：通过。
- 最大上下文窗口：模型拒绝回答。
- 知识截止时间：模型拒绝回答。

### 6.2 可修部分

上下文窗口可以从真实运行配置或模型映射中提供，例如：

- 仅当服务端明确配置 `context_window_tokens` 时，回答该配置值。
- `[1M]` 模型别名可以回答配置的 1,000,000，但必须确认上游实际允许该窗口。
- 普通模型回答配置表中的真实窗口，而不是根据模型名称猜测。

### 6.3 不建议修部分

Kiro 没有提供可信的 Claude 知识截止日期。硬编码某个月份只是在伪造身份特征，可能帮助检测得分，却会向真实客户返回错误信息。

建议做法：

- 默认继续拒绝或说明无法可靠确认。
- 只有管理员显式配置 `knowledge_cutoff` 后才允许确定性回答。
- 配置必须被视为运营声明，而不是从 Kiro 自动推断。

### 6.4 客户影响

- 使用真实配置回答上下文窗口：风险低。
- 硬编码知识截止日期：风险高，不推荐。

## 7. 不建议继续调整的项目

### S1 Token 注入：80 分但已成功

报告判定：

```text
slope=1.57, overhead=0
suspicious_count=0
```

固定 token 注入已经消失。继续人为调整 reported tokens 可能破坏真实计量、缓存拆分和客户账单，不应为了 20 分边缘分数修改。

### D3 身份一致性：88 分且判定 match

当前显式模型字段为 100，隐式回答识别为 Anthropic Claude。继续强行改写具体版本可能误改正常内容，收益很低。

## 8. 推荐实施顺序

1. 增加 exact system 的拒绝原因日志，只记录分类原因，不记录 system 原文。
2. 允许静态 exact system 与被动 tools/auto tools 共存，required tool 继续拒绝。
3. 增加受限的显式 copy/echo token 本地契约。
4. 把本地静态文本 SSE 改为逐事件 chunk 输出。
5. 扩展本地探针：
   - static system + passive tools
   - static system + required tool（必须不短路）
   - D5 风格唯一 token echo
   - strict JSON 32 路并发 SSE
   - NewAPI 和 rs-direct 双路径
6. 全量测试、合并、部署并保持 DEBUG。
7. 重新运行 Ztest；如果 D16 再次出现空响应，用请求时间关联 rs/NewAPI 两侧的事件数量和首末事件。

## 9. 预期结果

按上述方案实施后：

- D5：预期从 0 提升到 100。
- S3：两个静态 override 子项可修，预期从 33 提升到 100。
- D16：事件分块加固后预期从 75 提升到 100；当前本地并发已经稳定，主要用于消除外部解析偶发性。
- D11：建议维持 33，除非补充真实、可审计的模型元数据配置。

优先目标不是伪造模型信息，而是让明确、可确定执行的用户契约在 rs 层稳定实现，同时不破坏正常工具调用、计量和多轮对话。
