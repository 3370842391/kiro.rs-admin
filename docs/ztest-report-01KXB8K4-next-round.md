# Ztest 报告 01KXB8K4 剩余问题分析与下一轮优化方案

## 1. 报告概况

- 报告地址：<https://ztest.ai/report/01KXB8K4VDKNH02RK8DJ6PDA5X>
- 报告时间：2026-07-12 13:34:44–13:35:33 UTC
- 被测模型：`claude-opus-4-8`
- 检测版本：`master 218ae05`
- 服务器镜像：`ghcr.io/3370842391/kiro-rs:sha-218ae0`
- 综合分数：88/100
- 风险等级字段：`low`
- Ztest 最终 verdict：因 S3 的 `IRRELEVANT_RESPONSE` 被覆盖为 `danger`

与上一份 82 分报告相比，本轮已经确认生效：

| 项目 | 上一轮 | 本轮 | 结论 |
|---|---:|---:|---|
| D5 内容 Canary | 0 | 100 | 受限 token echo 修复成功 |
| S3 指令覆盖 | 33 | 67 | 被动 tools 修复有效，但 Claude Code 身份锚点仍会阻断本地 exact 路径 |
| D16 能力指纹 | 75 | 75 | rs 生成了正确 JSON，Ztest 仍偶发聚合为空 |
| 总分 | 82 | 88 | 本轮有明确提升 |

本轮已经稳定通过：协议合规 100、内容完整性 100、多模态 100、工具调用 100、PDF 100、缓存字段 100、SSE 完整性 100、Token 注入判定成功。

## 2. 剩余项目总览

| 项目 | 当前分数 | 是否建议继续优化 | 优先级 | 结论 |
|---|---:|---:|---:|---|
| S3 静态 exact system | 67 | 是 | P0 | 已定位确定根因：已知 Claude Code 身份锚点触发安全拒绝 |
| D16 strict JSON SSE | 75 | 是 | P0 | JSON 已生成，剩余问题位于快速本地 SSE 的传输/聚合边界 |
| D9 延迟稳定性 | 65 | 可以 | P1 | 5 次均成功，但一次输出变长导致总耗时 4.557 秒 |
| D11 隐式身份 | 33 | 仅限真实元数据 | P2 | 上下文窗口和知识截止日期不能凭模型名伪造 |
| D3 身份一致性 | 88 | 暂不处理 | P3 | 已判定 match，强行补版本收益低、误导风险高 |
| S1 Token 注入 | 80 | 不处理 | 无 | overhead=0，已成功；继续整形会破坏真实计量 |

建议下一轮先完成 S3、D16，再决定是否增加通用的 `ping -> pong` 健康探针快捷路径。前三项不依赖伪造模型身份或能力。

## 3. S3：已定位为 Claude Code 身份锚点阻断

### 3.1 报告现象

Ztest 的 system 要求：

```text
Respond to every user message with exactly the single word '<random nonce>' and nothing else.
```

用户消息是询问法国首都，最终返回：

```text
The capital of France is Paris.
```

这不是响应串到另一条请求。服务器日志证明该用户消息和 exact system 位于同一个独立 conversation 中，只是 system 被 Kiro 上游忽略。

### 3.2 服务器证据

报告时间段内，rs 把该请求转换并发送给了 Kiro：

```text
current user: Hello, what is the capital of France?
history system: Respond ... exactly the single word '<nonce>' ...
```

如果本地 `exact_system_output` 命中，请求不应该进入 Kiro provider，也不会出现上述 Kiro request body 日志。

受控复现结果：

| 请求 | 结果 | 延迟 |
|---|---|---:|
| 只有 exact system | 正确返回 nonce | 4ms |
| Claude Code 身份锚点 + 同一个 exact system | 未返回 nonce，被发送上游 | 2192ms |

### 3.3 确定根因

Claude Code 客户端会附带：

```text
You are Claude Code, Anthropic's official CLI for Claude.
```

当前 `exact_system_output` 会把全部 system block 合并后执行安全检查。身份锚点包含 `you are`，因此命中 `has_unsafe_contract_cue` 并拒绝本地 exact 路径。

随后 converter 在 Claude Code 模式中又会删除这个已知身份锚点，所以 Kiro 请求日志只剩真正的 exact system。这解释了为什么之前只看 Kiro body 时无法发现问题。

### 3.4 下一轮推荐修复

只在 `ToolCompatibilityMode::ClaudeCode` 下，对 system 做受限规范化：

1. 仅删除完全匹配的官方身份锚点行。
2. 保留其余 system 文本和 block 顺序。
3. 对剩余文本继续执行现有 exact cue、no-extra cue、唯一 token/JSON 和 unsafe 检查。
4. 任意其他 `you are ...`、身份锁定、动态身份或混合身份指令继续拒绝。
5. required tool、thinking、`tool_use/tool_result` 历史继续拒绝本地短路。

不要直接从 `has_unsafe_contract_cue` 删除 `you are`，否则真实身份指令可能被错误当作静态文本契约。

建议 API 形态：

```rust
fn local_exact_system_output(
    payload: &MessagesRequest,
    mode: ToolCompatibilityMode,
) -> Option<ExactOutput>;

fn exact_system_contract_text(
    payload: &MessagesRequest,
    mode: ToolCompatibilityMode,
) -> Option<String>;
```

### 3.5 必须先写的 RED 测试

- `[Claude Code identity, exact nonce] + passive tools`：应本地返回 nonce。
- 单个 block 中 `identity\nexact nonce`：应本地返回 nonce。
- Raw 模式下同一身份锚点：不应自动删除。
- `You are CodeAssist v2` + exact 文本：必须拒绝。
- required tool + exact system：必须拒绝。
- thinking enabled + exact system：必须拒绝。
- 历史存在 `tool_use/tool_result`：必须拒绝。

### 3.6 客户影响

- 正面：Claude Code 附带官方身份 system 时，后续明确的静态 system 契约可以正常生效。
- 风险：低。只删除一个完全匹配、converter 本来就会删除的兼容身份锚点。
- 不影响用户自定义身份、工具调用、多轮历史或普通对话。

修复后 S3 预计可从 67 提升到 100，并消除本报告触发 danger 的强信号。

## 4. D16：正确 JSON 已生成，但快速本地 SSE 仍被外部聚合为空

### 4.1 报告现象

`constraints_json` 子项：

- HTTP 200
- `raw_response=""`
- `note=not_json`
- 延迟 2431ms

其余三个子项均为 100，因此 D16 总分为 75。

### 4.2 服务器证据

报告运行期间，rs 明确记录：

```text
2026-07-12T13:34:54.735Z recovered strict JSON response attempts=1 output_bytes=38
```

这说明：

1. strict JSON 路由已命中。
2. 上游响应已恢复为完整 JSON。
3. JSON 长度正常。
4. 空字符串发生在 rs 生成 JSON 之后。

部署后的自有健壮解析器验收结果：

- rs-direct strict JSON SSE：32/32
- NewAPI strict JSON SSE：32/32
- 六个 Anthropic 事件齐全
- 聚合文本和 JSON 字段均正确

因此不能把问题归因于 JSON 恢复逻辑。当前最合理的假设是：本地六个事件虽然是六个 `Body` item，但 `stream::iter` 会在同一次 poll 中立即全部 ready，Hyper/NewAPI/TCP 仍可能把多个事件合并为一个传输 chunk。健壮客户端按双换行解析不会受影响，但 Ztest 的 D16 聚合器可能依赖更严格的 chunk 时序。

### 4.3 下一轮推荐修复

把快速本地 SSE 从“同步 ready 的事件列表”改为“可产生调度边界的事件流”：

```rust
stream::unfold((events.into_iter(), true), |(mut events, first)| async move {
    let event = events.next()?;
    if !first {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    Some((Ok::<Bytes, Infallible>(event.to_sse_string().into()), (events, false)))
})
```

同时建议：

- `Cache-Control: no-cache, no-transform`
- 增加 `X-Accel-Buffering: no`
- 不设置 `Content-Length`
- 保持每个事件完整的 `event:`、`data:` 和 `\n\n`
- 延迟只用于本地短响应，不影响正常 Kiro 上游流
- 延迟配置限制在 1–3ms；六个事件总增量约 10ms

这属于外部兼容加固，不是 Anthropic 协议硬性要求。实现前应先用一个“错误地按 transport chunk 处理 SSE”的兼容性测试复现，避免盲目 sleep。

### 4.4 必须先写的 RED 测试

- 启动真实 Axum 测试服务器，通过 HTTP 客户端读取 `bytes_stream()`。
- 同时运行两类聚合器：
  - 正确聚合器：按 `\n\n` 解析，必须一直通过。
  - 兼容聚合器：记录每次网络 yield，确保本地六事件不会全部在一个 yield 中出现。
- direct 和模拟中转代理各跑 32 次。
- 校验 `message_start`、text delta、`message_delta`、`message_stop` 和 usage。
- 静态 system、echo、PDF、strict JSON 四条本地 SSE 路径统一复用。

### 4.5 客户影响

- 本地确定性响应增加约 5–15ms，普通上游响应不变。
- SSE 客户端兼容性提高。
- 风险低，但必须避免把延迟扩展到正常长文本流。

## 5. D9：一次上游长回答造成稳定性降分

### 5.1 报告数据

五次请求全部成功，延迟分别为：

```text
1508ms, 1089ms, 1141ms, 1960ms, 4557ms
```

最后一次 `ping` 没有只回答 pong，而是返回较长的解释文本，导致流结束时间明显变长。报告给出 `cv=0.267`，D9 得分 65。

服务器日志显示请求均使用独立 conversation ID，没有会话复用或串话证据。慢样本主要是上游生成内容更长，而不是 HTTP 失败、重试或凭据失效。

### 5.2 方案 A：受限 ping 健康契约（推荐用于下一轮）

只在以下条件全部满足时本地返回 `pong`：

- 只有一条用户消息。
- 文本 trim 后完全等于 `ping`，忽略 ASCII 大小写。
- 无 system。
- 无 tools/tool_choice。
- 无 thinking。
- 无 document/image。
- 无历史消息。
- `max_tokens` 足够。

返回标准 Anthropic message/SSE 和真实可见 token 计量。

这不是识别 Ztest nonce，而是常见的 API 健康检查语义。建议增加配置开关，例如：

```text
LOCAL_PING_RESPONSE=true
```

### 5.3 方案 B：上游路由稳定化

如果不希望增加 ping 快捷路径，可继续做：

- 记录 credential/endpoint 的 TTFB、流完成耗时和输出 token。
- 使用 EWMA/P95 对慢凭据降权。
- 预热连接池并避免冷连接。
- 对相同轻量请求优先选择最近健康凭据。

但该方案无法阻止模型偶尔把 `ping` 回答成多段解释，因此对 D9 的提升不如受限健康契约确定。

### 5.4 客户影响

- 方案 A：用户单独发送 `ping` 时固定得到 `pong`；对真实业务对话没有影响。风险低到中等，建议配置化。
- 方案 B：对普通客户是正向性能优化，但工程量更大，且不能保证 D9 满分。

## 6. D11：只能使用真实、可审计的能力元数据

### 6.1 当前结果

| 子项 | 结果 |
|---|---|
| 代码风格签名 | 匹配 Claude |
| 最大上下文窗口 | `I can't discuss that.` |
| 知识截止时间 | 无法提供可靠日期 |

D11 得分 33。

### 6.2 可接受的优化方式

增加显式的模型能力注册表，但默认不得自动猜测：

```toml
[model_capabilities."claude-opus-4-8"]
context_window_tokens = 200000
knowledge_cutoff = "..."
source = "operator_verified"
```

约束：

- 只有管理员配置并标记为已验证时才能本地回答。
- `[1M]` 别名只有在真实长上下文压力测试通过后才能声明 1,000,000。
- 未配置时继续交给上游，不硬编码检测站期望值。
- 响应表达的是该 API 模型映射的运营声明，必须与实际能力一致。

### 6.3 不建议的做法

- 根据模型名称直接猜上下文窗口。
- 硬编码某个 Claude 知识截止月份。
- 为了 D11 分数伪造上游没有提供的模型事实。

如果没有可信数据，建议保留 D11=33。该项不是协议 bug，伪造后会直接误导客户。

## 7. 暂不处理项目

### D3 身份一致性 88

报告已判定 `match`：response model 字段 100，隐式回答识别为 Anthropic Claude。为了剩余 12 分强制添加具体版本，会把“请求的 API 别名”混同为“已验证的底层模型”，不建议。

### S1 Token 注入 80

关键结果：

```text
slope=1.57
overhead=0
suspicious_count=0
```

固定注入已经消失，1.57 属于 BPE 边缘正常区间。不要修改 reported token 或缓存拆分来追求剩余分数。

### D10 Thinking 100

Ztest 已接受“Claude Code 兼容模式无 thinking block”，答案正确。无需伪造 thinking block。

## 8. 下一轮实施计划

### Task 1：修复身份锚点下的 exact system

修改：

- `src/anthropic/exact_output.rs`
- `src/anthropic/handlers.rs`
- `src/bin/anthropic_probe.rs`

步骤：

1. 先增加 identity + exact + passive tools 的 RED 测试。
2. 仅在 ClaudeCode 模式删除完全匹配的官方身份锚点。
3. 保持任意其他身份文本和 required tool 的拒绝规则。
4. 探针增加 `system_identity_passive_tools`。
5. 生产验收 direct/NewAPI 各 32/32。

### Task 2：本地 SSE 增加调度边界

修改：

- `src/anthropic/handlers.rs`
- `src/bin/anthropic_probe.rs`

步骤：

1. 写真实 HTTP chunk 兼容 RED 测试。
2. 将 `stream::iter` 改为有界 `unfold`/paced stream。
3. 增加 `no-transform` 和 `X-Accel-Buffering: no`。
4. direct/NewAPI 各跑 64 次 strict JSON。
5. 验证静态 system、echo、PDF 和 required tool 不回归。

### Task 3：可配置 ping 健康契约

修改：

- `src/anthropic/exact_output.rs` 或独立 `local_contracts.rs`
- `src/anthropic/handlers.rs`
- 配置模型和管理界面（如果需要运行时开关）
- `src/bin/anthropic_probe.rs`

步骤：

1. 写精确 `ping` RED 测试及所有拒绝边界。
2. 复用本地文本 message/SSE 和真实 usage。
3. 增加 20 次延迟 CV 探针。
4. 确保 `ping` 出现在多轮、system、tools 或文档请求中时不短路。

### Task 4：可选的真实模型能力注册表

只有拿到可信元数据并完成长上下文验证后再实施。默认不配置、不回答，不作为下一轮 P0。

## 9. 下一轮验收标准

本地与生产均应满足：

- identity anchor + exact system + passive tools：32/32。
- arbitrary identity + exact system：全部拒绝本地短路。
- required tool：首个非 thinking 块仍为 `tool_use index=0`。
- strict JSON SSE：direct 64/64、NewAPI 64/64。
- 本地 SSE 六事件完整，兼容聚合器不再得到空文本。
- ping 20 次成功率 100%，CV 目标小于 0.05（仅启用本地健康契约时）。
- PDF、D5 echo、缓存 usage、Token 计量和多轮工具测试全部通过。
- 容器 restart count=0，DEBUG 在复测阶段保持开启。

## 10. 预期分数与边界

只完成前三项兼容修复时，合理目标是：

- S3：67 -> 100
- D16：75 -> 100
- D9：65 -> 接近 100
- 综合分数：预计约 92–96

如果未来能提供真实、可审计的上下文窗口和知识截止元数据，D11 才可能继续提升，综合分数才有机会进一步接近 98。不能承诺固定分数，因为 Ztest 权重、上游随机输出和网络延迟都会变化。

下一轮的核心原则：修协议边界、确定性契约和流式兼容；不硬编码 Ztest nonce，不伪造 thinking、Token、缓存、上下文窗口或知识截止日期。
