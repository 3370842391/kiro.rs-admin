# 断流续写与 P1 / P2 优化思路

> 日期：2026-08-05 · 状态：**待评审，未动代码**
> 证据窗口：`traces.db` 2026-08-02 → 08-05（`traceRetentionDays=3`）、
> `error_snapshots.db` 07-27 → 08-05（64772 条）、生产容器 `kiro-rs-admin`
> （`key-supplier-presets-f49db38`，8990）4.3 小时 docker 日志。
> 按要求**跳过账号封禁 / 号池枯竭**（那批 ~28000 条 `unknown` 不在本文范围）。

---

## 0. 结论速览

每项的完整代价分析见 **§4**。「主要代价」列只列最需要提前知道的那一条。

| # | 问题 | 建议动作 | 主要代价（详见 §4） | 客户可感知 | 风险 |
|---|---|---|---|---|---|
| A | **`stream_tail` 100% 丢失**，续写/排障拿不到断点内容 | 二进制尾部按原样存（已是 BLOB + zstd） | **对话正文落盘**（隐私面，非磁盘） | 否 | 低，改 10 行 |
| B | 续写两个开关**生产全关** | 先修 C 再开 | 见 C | 是 | 中 |
| C | 续写门槛与生产断流形态**完全错配** | 按场景放宽（见 §1.5） | **重复 `tool_use` 会被真的执行两次**；额度翻倍；该轮缓存全丢 | 是 | 高，需分场景 |
| D | `upstream_empty_response` 重试后仍空 | 先归因埋点，再考虑换凭据重试 | 换号 = Kiro 侧缓存冷启动；号池只剩 3 个 | 延迟 | 中 |
| E | 请求体过大被上游 400 | 预检压缩，别等上游拒 | **有损且客户不知情**；裁剪改前缀 → 缓存失效 | 是 | 中 |
| F | 工具入参命名变体被整轮拒 | 别名归一化接进现有确定性修复 | 在改写模型输出，映射错 = 工具执行错误操作 | 正向 | 低 |
| G | 同一违规落进两个 error_type 桶 | 统一分类口径 | 历史数据断层，报表/告警要重新校准 | 否 | 低 |
| H | thinking WARN 淹没日志 | 降级为采样/计数 | 降到 debug 后生产默认看不到 | 否 | 低 |
| I | 快照库 9.14 GB，`client_disconnected` 独占 3.2 GB | 该类只存元数据 | 客户投诉断流时无法复现 | 否 | 低 |
| **J** | **自动压缩永不触发** | **降压缩信号阈值 100%→85%**（不动 usage） | 零计费影响；但依赖客户端认这个 stop_reason，须验证 | 压缩变频繁 | 低 |
| ~~J'~~ | ~~把 usage 报成真值~~ | **暂缓** | **已确认客户账单翻倍、长会话 3.5×**（§2.4.5） | 账单 | 高 |

---

## 1. 主线：断流续写

### 1.1 生产断流的真实形态

近 4 天，`final_status='interrupted'` 与断流类错误：

| error_type | 次数 | 有已下发内容 | 平均已下发 | 最大 | 平均耗时 | 平均 output tokens |
|---|---|---|---|---|---|---|
| `client_disconnected` | 8426 | 8426 | 154 KB | 1.38 MB | 75 s | 2161 |
| `upstream_empty_response` | 1817 | **0** | – | – | – | 0 |
| `stream_idle_timeout` | 811 | 732 | 37 KB | 1.69 MB | 181 s | 662 |
| `stream_read_error` | 101 | 101 | **835 KB** | 1.72 MB | **694 s** | **14187** |
| `stream_interrupted` | 13 | 13 | 899 KB | 1.34 MB | 1407 s | 0 |

**可续写的目标集** = `stream_idle_timeout`(732) + `stream_read_error`(101) + `stream_interrupted`(13)
≈ **846 条 / 4 天 ≈ 210 条/天**。

其中 `stream_read_error` 是**性价比最高**的一类：平均已经生成了 14187 个 output token、
跑了 11.5 分钟、下发了 835 KB，然后断掉。这一轮的算力和额度全部作废，用户还得重来。
`stream_interrupted` 更极端，平均 23 分钟。

`upstream_empty_response`（1817 条）**不属于续写范畴** —— 一个字节都没产出，没有"续"的起点，
归到 §3.1 处理。`client_disconnected`（8426 条）是客户端主动挂断，不该续写，但它在存储上有问题（§4.4）。

### 1.2 请求头：已完整记录 ✅

回答你的问题——**记录了，而且够用**。

`src/anthropic/error_snapshot.rs:707` 把 `headers` 与 `request` 一起写进 `client_request` payload，
`sanitize_headers()`（`:842`）保留全部头、只对 secret 字段打码。从生产库实取一条
（`snap_bfb8379f...`，`stream_idle_timeout`，claude-opus-5）解出来是：

```json
{
  "anthropic-beta": "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,
                     thinking-token-count-2026-05-13,context-management-2025-06-27,
                     prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,
                     advisor-tool-2026-03-01,effort-2025-11-24,fallback-credit-2026-06-01,afk-mode-2026-01-31",
  "anthropic-version": "2023-06-01",
  "user-agent": "claude-cli/2.1.222 (external, cli)",
  "x-stainless-timeout": "600",
  "x-stainless-package-version": "0.94.0",
  "content-length": "831040",
  "x-api-key": "[REDACTED]",
  "x-newapi-user": "3474046521",
  "x-oneapi-request-id": "33e1eddd-e11a-49cc-9906-794e55802f97"
}
```

能拿到客户端身份（`claude-cli/2.1.222`）、协商的 beta 能力集、客户端超时（600 s）、
NewAPI 侧的路由 ID。做续写的**请求侧**信息不缺。

> 附带一条：`x-newapi-token` 是非 UTF-8，被记成 `[NON_UTF8 length=6 sha256=...]`。不影响续写。

### 1.3 阻断项 A：断点内容 100% 丢失（**必须先修**）

请求侧齐了，**响应侧断点是空的**。

`src/anthropic/error_snapshot.rs:408` `StreamTail::snapshot_bytes()`：

```rust
fn snapshot_bytes(&self) -> Vec<u8> {
    if std::str::from_utf8(&self.bytes).is_ok() {
        return self.bytes.clone();       // ← 只有合法 UTF-8 才存原文
    }
    serde_json::to_vec(&serde_json::json!({   // ← 否则只留摘要
        "invalid_utf8": true,
        "original_bytes": self.bytes.len(),
        "sha256": hex::encode(Sha256::digest(&self.bytes)),
    })).unwrap_or_default()
}
```

缓冲的是 Kiro 的 **AWS event-stream 二进制帧**（4 字节长度前缀 + CRC32 + 头部），
**永远不可能**通过 `from_utf8`。实测：

```
stream_tail 原始大小分布：  <200B(疑似仅摘要)  34307 条  平均 120 字节
```

**34307 条，一条不漏，全是摘要。** 上面那条样本丢掉的是 92516 字节的真实断点数据。

同样的问题在 `push()`（`:391`）也有：它试图把缓冲裁到 UTF-8 边界，对二进制帧同样无效。

**改法**：`data` 列本来就是 BLOB、`content_type` 本来就写的 `application/octet-stream`、
外面本来就套了 zstd —— 直接存原始字节即可，摘要只作为"确实存不下"时的兜底。
`sanitize_payload_data()` 对 `application/octet-stream` 需要放行（它现在按 JSON 路径清洗）。

按 34307 条 × 92 KB 估，全量存约 3 GB/7 天未压缩；zstd 后预计 300–600 MB。
建议同时把 `STREAM_TAIL_MAX_BYTES` 调小（续写只需要末尾几十 KB），并只对
`stream_idle_timeout` / `stream_read_error` / `stream_interrupted` 三类存尾部。

> 没有这一步，续写做出来也无法验证对错、线上出问题也无从复盘。**这是 P0 前置。**

### 1.4 阻断项 B：现有续写机制的门槛对不上

`should_auto_continue_round()`（`src/anthropic/handlers.rs:3534`）有六道门槛：

```rust
if !setup.auto_continue_enabled                                   // ① 生产 = false
    || continuation_count >= setup.auto_continue_max              // ② 3 轮
    || !matches!(termination, AttemptTermination::Eof)            // ③ 只认 Eof
    || ctx.accumulated_text().trim().is_empty()
    || !auto_continue_is_plain_text_mode(setup.thinking_enabled, ctx)  // ④ 排除 thinking/tool
    || ctx.repetition_guard_tripped()
    || ctx.has_terminal_error()
{ return false; }

let stop_reason = ctx.current_stop_reason();
if !auto_continue_stop_reason_allows(&stop_reason, setup.partial_stream_recovery_enabled) {
    return false;                                                 // ⑤ 只认 max_tokens
}
```

对照生产实际：

| 门槛 | 生产断流的情况 | 结果 |
|---|---|---|
| ③ 只认 `AttemptTermination::Eof` | 断流是 `IdleTimeout`(811) / `ReadError`(101) | **全部挡掉** |
| ④ `!thinking && !saw_tool_use && !saw_reasoning_output` | 客户端是 Claude Code，thinking + tools 常驻 | **全部挡掉** |
| ⑤ `stop_reason == "max_tokens"` | 断流时压根没有 `message_delta`，拿不到 stop_reason | **全部挡掉** |

**即使把 `autoContinueEnabled` 打开，上面 846 条里一条都续不了。**
现有机制解决的是"纯文本写到 max_tokens 截断"，不是"流断了"。这是两件事。

好消息是**管道是通的**：`:3980` 已经会把 `message_delta`/`message_stop` 从待发事件里摘掉，
`ctx.prepare_for_continuation()` / `begin_continuation()`（`:4001`）负责跨轮上下文，
`prepare_auto_continue_request_body()`（`:2654`）已经会把已生成文本作为
`assistantResponseMessage` 塞进 `history` 再用 `AUTO_CONTINUE_PROMPT` 续。
**要改的是判定，不是管道。**

### 1.5 分场景续写策略（建议）

断流点的内容形态决定能不能续，必须分开处理，不能一刀切：

| 断点位置 | 能否续写 | 处理 |
|---|---|---|
| **text block 中间** | ✅ 能 | 走现有 `prepare_auto_continue_request_body`，把已下发文本作为 assistant 历史 |
| **tool_use 的 input JSON 中间** | ❌ 不能 | JSON 半截无法接续。丢弃该 block 整轮重试（`ToolJsonAccumulator` 已能识别 `IncompleteToolJson`） |
| **thinking block 中间** | ⚠️ 危险 | thinking 有 signature，续写产生的新 block 签名对不上，客户端会校验失败。**建议先不做**，或续写时降级为不带 thinking 的纯文本补完 |
| **block 之间的边界** | ✅ 最安全 | 已完成的 block 全部保留，从下一个 block 续 |

**建议的门槛改法**（按风险从低到高，可分批上）：

1. **放开终止态**：③ 改为接受 `Eof | IdleTimeout | ReadError`，
   排除 `ClientClosed`（客户端已经走了，续写纯属浪费额度）。
2. **放开 stop_reason**：⑤ 在断流场景下 stop_reason 本来就不存在，
   应改成"断流且已有已提交内容"即可，不再要求 `max_tokens`。
3. **按 block 边界而非全局标志放开 ④**：把
   `!saw_tool_use && !saw_reasoning_output` 这种"整轮见过就禁"的判定，
   换成"**断点当前所在的 block 类型**"。见过 tool_use 但断在 text 里，是可以续的。
4. **thinking 单独开关**，默认关。先把 text/tool 边界这两类跑稳。

配套：
- 续写轮次要计入 `traces.db`（新增 `continuation_rounds` 列），否则线上无法评估收益。
- 续写要有**额度护栏**：`stream_read_error` 那类平均已经烧了 14187 token，
  续写失败会翻倍。建议单请求续写总 token 设上限，超了就如实报错。
- `AUTO_CONTINUE_PROMPT` 目前是为"接着写"设计的，断流续写的语义不同
  （不是"继续"而是"从这里接上"），提示词要分开。

### 1.6 落地顺序

```
1. 修 stream_tail 存储（§1.3）           ← 无依赖，先上，为后续验证提供证据
2. 加 continuation 观测列 + 断点 block 类型埋点   ← 上线后先只观测，不改行为
3. 放开终止态与 stop_reason（1 + 2）      ← 灰度开 autoContinueEnabled
4. 按 block 边界放开 ④（3）
5. thinking 续写（4）                     ← 单独评估，可能不做
```

---

## 2. P1

### 2.1 `upstream_empty_response`：重试后仍空（1817 条 / 4 天，且在涨）

08-02 → 08-05：205 → 467 → 541 → 604，**趋势向上**。

现状链路（4.3 小时日志）：
- `实时首轮未提交语义输出，丢弃整轮并重试一次 attempt=1 termination=Eof` 716 条
- 同样的 WARN，`termination=IdleTimeout` 140 条
- `prepared empty-response recovery body after the first upstream attempt failed` 207 条
  （`retry_body_variant="empty_response_recovery"`，会截断 tool_results、缩图）
- 最终 `ERROR 上游未产生可交付的助手内容 error_type=upstream_empty_response
  Upstream returned no assistant content after one retry` 672 条

即：**重试机制在跑，但重试一次仍然空，然后就放弃了。**

建议：
1. 重试时**换凭据**。现在的 recovery 只换了请求体（截断 tool_results / 缩图），
   没换 credential。如果是账号侧的问题，同一个号重试必然还是空。
2. 重试次数从固定 1 次改为可配，并按 `termination` 区分：
   `Eof` 立刻空 vs `IdleTimeout` 等了 120 秒，两者该用不同策略。
3. 先做归因埋点：把空响应按 `(credential_id, model, endpoint, 请求体特征)` 分组统计。
   现在无法判断是账号问题、模型问题还是请求体问题 —— **这个要先做**。

### 2.2 请求体过大 / 上下文超限

| 现象 | 次数 |
|---|---|
| `Upstream context window was exceeded` | 663 |
| `400 {"message":"Input is too long.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}` | 482 |
| WARN `流式请求在发送响应前失败 ... upstream request body is too large` | 413（4.3 h） |
| `auto_compact_diagnostics diagnosis="payload_limit_preempted"` | 407（4.3 h） |

典型样本：`request_body_bytes=2933116` vs `upstream_request_max_bytes=2799833` —— **只超 4.8%**。

也就是说很大一部分是"差一点点"。现在的做法是发出去被上游 400 再说，
但 `payload_limit_preempted` 说明我们**发之前就知道会超**。

建议：
1. 预检超限时直接走已有的缩图 / 截断 tool_results 路径，而不是发出去等 400。
   `empty_response_recovery` 那套请求体裁剪逻辑可以复用。
2. `diagnosis="client_usage_signal_incomplete"` 这类（`upstream_context_percentage` 到 99.8%
   而 `client_reported_tokens` 只有 15 万）说明**客户端不知道该压缩了**。
   考虑在 usage 里把真实上游占比回传，让 Claude Code 自己触发 auto-compact。
   这条能从源头减少 663 条 context exceeded。
3. 图片：`image_count=46 image_total_b64_kb=8886` 这种请求存在。
   现有 `imageRetryHistoryMaxDimension=960 / JpegQuality=60` 只在**重试时**生效，
   考虑首轮就按预算缩。

### 2.3 上游 400 `REQUEST_BODY_INVALID`

- `Invalid tool use format.` **568**
- `Improperly formed request.` 136

这是**我们构造的请求体**被上游拒了，不是客户端的锅。568 条集中在 08-05。
需要拿 `kiro_request` payload（快照里有，53035 条）做样本比对，定位是哪一类工具组合触发的。
**这条目前信息不足以定方案，先取样。**

---

## 2.4 自动压缩触发不了（根因已定位）

已有一份 `docs/ISSUE-auto-compact-not-triggering.md`（2026-07-28），结论停在
「token 到 99%、`model_context_window_exceeded` 已下发，客户端仍不压缩」的矛盾上。
本次用 `traces.db` 的诊断列把它定死了。

### 2.4.1 生产诊断分布（近 4 天）

| `compaction_diagnosis` | 条数 | 上游占比 | 客户端看到 | 上游真实 | 比值 |
|---|---|---|---|---|---|
| `normal` | 544428 | 9.7% | 38073 | 93743 | 0.41 |
| `client_usage_signal_incomplete` | 1634 | 89.0% | 120541 | 651344 | **0.19** |
| `suspected_client_compaction_not_triggered` | 1238 | 91.0% | 186619 | 854932 | **0.22** |
| `upstream_context_unknown` | 642 | – | 92623 | – | – |
| `payload_limit_preempted` | 528 | 99.6% | 135643 | 256120 | 0.53 |
| `context_signal_enqueued` | 233 | 96.7% | 222166 | 196660 | 1.13 |
| `proxy_context_signal_not_exposed` | 184 | 99.9% | – | 494698 | – |

**上游用掉 90% 窗口时，我们只告诉客户端 28%。** Claude Code 按自己收到的 usage
占窗口比例决定压缩（阈值 ~83.5%–92%），看到 15% 自然永不触发，一路撞到字节墙 400。

### 2.4.2 根因：`message_start` 早于 `ContextUsage`，客户端只认前者

三段证据锁死：

**(1) 协议侧** —— Anthropic 流式契约里 `message_delta.usage` 只携带 `output_tokens`：

```
event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":12}}
```

客户端 SDK 组装最终 usage 时，**`input_tokens` 取自 `message_start`**，
`message_delta` 只用于更新 `output_tokens`。

**(2) 代码侧** —— `usage.rs:57` 的注释自陈了时序：

```rust
// 估算值仍保留：message_start 早于 contextUsageEvent，那时只有它可用。
```

`split_api()`（`usage.rs:36`）的「上游真值优先」修复（commit `5f785cf`）是对的，
但 `message_start` 发出时 `upstream_context_tokens` 还是 `None`，只能退回本地估算。
上游 `ContextUsage` 要等到流中段才到（`stream.rs:2371`）。
收尾的 `message_delta` 确实带了修正值（`stream.rs:3914` → `resolved_usage()`），
**但那是客户端不读的位置**。

**(3) 数据侧** —— 按流式/非流式拆分，结论无可辩驳：

| | 请求数 | 平均比值 | 比值 ≥0.95（准确） |
|---|---|---|---|
| 非流式（收尾算 usage） | 95886 | 0.661 | **44795（47%）** |
| 流式（`message_start` 抢跑） | 404076 | 0.500 | **2358（0.58%）** |

非流式在响应末尾才算 usage，那时上游真值已到，所以近半数准确；
流式因 `message_start` 抢在前面，**准确率 0.58%**。同一套 `split_api()`，
差别只有「算的时候上游真值到没到」。

生产 `earlyStreamHandshake = true` 会让 `message_start` 发得更早（`handlers.rs:2770`），
进一步扩大这个窗口。

> 顺带澄清：诊断字段 `client_reported_tokens` 只统计 `message_start`
> （`compaction_diagnostics.rs:254-275`）。这不是埋点缺陷 —— 它量的正是客户端实际采信的那个值，
> 口径是对的。

### 2.4.3 为什么不能「等 `ContextUsage` 到了再发 `message_start`」

流式 40 万条里只有 0.58% 出现过上游真值早于 `message_start` 的情况，
说明 `ContextUsage` 基本总是晚到。等它 = 把 TTFB 拖到上游首个上下文事件，
与 `earlyStreamHandshake` 的目的直接冲突，也会放大 §1 里的空闲超时问题。**不建议。**

### 2.4.4 建议方案：用同会话上一轮的上游真值外推

会话是**单调增长**的 —— 上一轮的 `upstream_context_tokens` 是这一轮的可靠下界，
比本地估算准得多（本地估算平均只有真值的一半）。

可行性已验证：
- `session_hash` 来自请求的 `metadata.user_id`，**解析请求时即可得**
  （`compaction_diagnostics.rs:148`），早于任何上游调用。
- 覆盖率：高压力流式请求中 **9673/11404 = 85%** 带 `session_hash`，横跨 194 个会话。
- 历史深度：2971 个会话有 ≥3 条带上游真值的记录，够做外推。

实现要点：
1. 内存里维护 `session_hash → 上一轮 upstream_context_tokens`（带 TTL，会话结束即淘汰）。
2. `message_start` 的 `input_tokens` 取 `max(本地估算, 上一轮上游真值)`。
   取 max 而非直接替换，是因为本轮可能新增了大量内容。
3. 拿不到会话历史时（15% 无 `session_hash`、或会话首轮）退回现有本地估算，行为不变。
4. `message_delta` 的修正值继续保留 —— 不符合契约但无害，且对读它的客户端有用。

**这是估算，不是真值**，所以要配一条护栏：外推值不得超过窗口的某个上限
（例如 95%），避免误报把客户端逼进无谓的压缩循环。

### 2.4.5 ⚠️ 下游计费口径：已确认 NewAPI 就是按 `message_start` 记账

**这一条推翻了 §2.4.4 的原方案。**

查 NewAPI 源码（`Calcium-Ion/new-api`，`relay/channel/claude/relay-claude.go`
的 `HandleClaudeResponseData`）：

```go
if claudeResponse.Usage != nil {
    claudeInfo.Usage.PromptTokens = claudeResponse.Usage.InputTokens
    claudeInfo.Usage.CompletionTokens = claudeResponse.Usage.OutputTokens
    claudeInfo.Usage.PromptTokensDetails.CachedTokens =
        claudeResponse.Usage.CacheReadInputTokens
    claudeInfo.Usage.PromptTokensDetails.CachedCreationTokens =
        claudeResponse.Usage.CacheCreationInputTokens
```

这段在 **`message_start`** 分支里取 usage，落进 `claudeInfo.Usage`，
最终由 `relay/claude_handler.go` 的 `service.PostTextConsumeQuota(c, info, usage, nil)`
扣客户额度。`HandleStreamFinalResponse` 只在 `PromptTokens == 0` 时才用本地估算兜底。

**即：我们在 `message_start` 里报的那个数，就是 NewAPI 扣客户钱的依据。**
`message_delta` 里的修正值它根本不读 —— 和 §2.4.2 的协议分析完全一致。

#### 账单会涨多少：按比例线性放大

`split_api()` → `split_against_total()` → `split_raw()`（`cache_metering.rs:152`）
的拆分是**按无量纲比例**做的：

```rust
let ratio = (self.cache_covered_est / self.prompt_total_est).clamp(0.0, 1.0);
let cache_total = total * ratio;   // 三个桶全部随 total 等比放大
let read = cache_total * (cache_read / cache_covered_est);
let creation = cache_total - read;
let input = total - cache_total;
```

`total` 从本地估算换成上游真值后，**input / creation / read 三个桶按同一比例一起放大**，
没有「增量都落进廉价的 cache_read 桶」这种缓冲。
`shape_hit_rate` 只在 input↔read 之间挪、总量不变，也不改变放大倍数。

所以账单倍数 ≈ 1 / 当前比值：

| 流量 | 当前比值 | 账单倍数 |
|---|---|---|
| 全部流式请求 | 0.500 | **≈ 2.0×** |
| 高压力（上游 ≥80%） | 0.284 | **≈ 3.5×** |
| `client_usage_signal_incomplete` | 0.19 | ≈ 5.3× |
| `suspected_client_compaction_not_triggered` | 0.22 | ≈ 4.5× |

**把 usage 报准 = 客户平均账单翻倍，长会话涨 3.5 倍。**
这不是可以「顺手修一下」的改动。

#### 改后的建议方案：不动 usage，改压缩信号阈值

既然 usage 是计费依据，就别碰它。换一条**零计费影响**的路：

`stream.rs:2379` 现在是

```rust
if context_usage.context_usage_percentage >= 100.0 {
    self.state_manager.set_stop_reason("model_context_window_exceeded");
}
```

**把 100.0 降到 85.0。** 客户端收到 `model_context_window_exceeded` 就会压缩，
不需要它自己按 usage 算比例 —— 绕开了计费口径这条线。

为什么 85% 有效而现在的 100% 无效：原 `ISSUE-auto-compact-not-triggering.md` 记录了
100% 时信号**确实发了**（240 分钟内 5 次）会话却仍死在 400。原因是 100% 时压缩本身
已经没有余量 —— compact 请求同样要带全量历史，同样撞 400，形成死锁
（社区 issue #24976 / #48893 描述过同一现象）。85% 时压缩还有 15% 窗口可用，能成功。

代价对比：

| 方案 | 计费影响 | 修复完整度 |
|---|---|---|
| 报准 usage（原 §2.4.4） | **账单 2–3.5×** | 完整，但 15% 无 `session_hash` 的请求仍漏 |
| 降信号阈值到 85% | **零** | 依赖客户端认这个 stop_reason；不认就无效 |

**建议先上阈值方案**（改一个常量，可灰度、可回滚），
用 `context_signal_enqueued` 和 `suspected_client_compaction_not_triggered`
两个诊断计数验证客户端是否真的压缩了。
如果验证下来客户端确实不认这个信号，再回来讨论 usage 口径 —— 那时它就是个明确的商业决策，
不是技术选择。

§2.4.4 的会话外推方案**暂时搁置**，不废弃：如果将来要按真值报（比如下游换成不按 token 计费），
那套外推逻辑仍然是对的。

### 2.4.6 阈值取 85% 的依据

社区报告的 Claude Code auto-compact 触发点在 **83.5%–92%** 之间
（见原 ISSUE 文档的参考链接）。取 **85%** 的理由：

- 落在报告区间的下沿，早于客户端自身阈值触发，确保信号先到；
- 留 15% 窗口给 compact 请求本身用 —— 这是 100% 方案死锁的直接原因；
- 生产数据里 `upstream_context_percentage >= 80` 的高压力请求 4 天有 11404 条
  （其中 3059 条流式），85% 不会把正常流量卷进来（`normal` 类平均只有 9.7%）。

可调空间：如果 85% 触发得太频繁（观察 `context_signal_enqueued` 计数），
往上调到 88–90%；如果仍有请求撞 400，往下调到 80%。
**建议做成配置项而不是硬编码常量**，方便线上灰度。

---

## 3. P2

### 3.1 工具入参命名变体（低风险、见效快）

| 违规 | 次数 |
|---|---|
| `AskUserQuestion` missing `$.questions[0].multiSelect` | 61 |
| `AskUserQuestion` `$.questions` expected array | 60 |
| `Edit` missing `$.file_path` | 58 |
| `read_file` missing `$.indentation/$.limit/$.mode/$.offset` | 41 |
| `read_file` missing **`$.filePath`** | 13 |
| `StructuredOutput` `$.results` expected array | 10 |
| `read_file` missing `$.endLine/$.filePath` | 8 |
| `Edit` missing `$.old_string` | 5 |
| `grep_search` missing **`$.Query` / `$.SearchPath`** | 3 |
| `list_directory` missing `$.explanation` | 3 |

两类性质不同：

**(a) 纯命名变体** —— `filePath` vs `file_path`、`Query` vs `query`、`SearchPath` vs `path`。
上游产出的是同一个意思的字段，只是大小写/风格不同，却导致**整轮被拒**。
现有的「确定性修复上游工具固定字段」（4.3 小时跑了 184 次，
`tool=read_file paths=["$.path"]`、`tool=grep_search paths=["$.query","$.glob"]`）
已经是对的机制，**只是没覆盖别名**。建议加一张 snake_case ↔ camelCase ↔ PascalCase
的归一化表接进去。

**(b) 结构性缺失** —— `$.questions expected array`、`$.results expected array`、
`$.todos expected array`。期望数组拿到了非数组，更像是**工具 JSON 累积不完整**
（和 `upstream_tool_json_error` 346 条是同源问题），不能靠改名修。
建议先确认这些是不是都伴随 `IncompleteToolJson`。

`read_file` 缺 `indentation/limit/mode/offset` 这 4 个"必填"值得单独看一眼：
Claude Code 侧这些字段实际是可选的，我们的 schema 可能定得比客户端严。

### 3.2 错误分类口径不一致

同一条违规同时出现在两个桶：

- `bad_request`：`tool "AskUserQuestion" input violates schema: missing required $.questions[0].multiSelect` **61 条**
- `upstream_tool_schema_error`：同样的消息 **2 条**

同理 `read_file` 的违规在 `upstream_tool_schema_error` 里 41 条，
但 `bad_request` 桶里也有同源条目。这让**按 error_type 做的所有统计都不可信**，
也解释了为什么 `bad_request` 在 08-05 突然涨到 1375。

建议：schema 违规统一归到 `upstream_tool_schema_error`，
`bad_request` 只留真正的上游 400。这条要先做，否则修完 §3.1 无法验证效果。

### 3.3 thinking WARN 噪音

4.3 小时 **13233 条**，占全部 WARN 的 **70%**：

```
客户端请求了 thinking，但 Kiro 未返回 reasoning；流式保留有效正文或工具调用
  opus-5 5958 · sonnet-5 3907 · opus-4-8 1598 · opus-4-7 621
  opus-4-6 515 · sonnet-4-6 308 · opus-4.8 279 · haiku-4-5 147
```

**请求本身不失败**，属于正常降级。但它把真正的 WARN 淹了——
docker 日志轮转后只剩 4.3 小时可回溯，其中 70% 是这一条。

建议降为 `debug`，或按模型做计数聚合（每分钟一条汇总），保留 metrics 不保留逐条。

### 3.4 快照存储：`client_disconnected` 占 3.2 GB

`error_snapshots.db` 9.14 GB，保留期内 8.18 GB / 64772 条：

| error_type | 条数 | 占用 |
|---|---|---|
| `client_disconnected` | 19088 | **3222 MB** |
| `unknown`（号池，不在本文范围） | 9813 | 944 MB |
| `stream_idle_timeout` | 2092 | 564 MB |
| `upstream_empty_response` | 1373 | 275 MB |
| `upstream_tool_json_error` | 346 | 116 MB |

客户端主动断连**基本不是 bug**，却在 `errorSnapshotCaptureBodies=true` 下
把完整请求体（平均 169 KB）全存了。

建议：`client_disconnected` 只存元数据，不存 body。预计立省 3.2 GB，
腾出来的空间正好给 §1.3 要新增的 `stream_tail`（预计 300–600 MB）。

保留期机制本身是正常的（超期只剩 85 条 / 0.02 GB，另有 170 条 `retention_exempt`），
不用动。

> 另：`traces.db` 的 WAL 涨到 440 MB 没截断过。`main.rs` 里有退出前截断 WAL 的逻辑
> （v0.9.44 加的），但这个容器已经连续跑了 2 天没重启。可以考虑定期 checkpoint。

---

## 4. 每项优化的代价与风险

前面各节讲的是「怎么修」，这一节讲「修了会付出什么」。
**没有一项是纯收益的**，其中 §4.2（续写）和 §4.6（usage 口径）会直接改变
客户可感知的行为和账单，上线前必须单独评审。

### 4.0 三条横切风险（多个改动共同放大）

#### (a) prompt cache 命中率会下降

本项目开了 `cacheMeteringEnabled=true`，并且有 `cacheHitRateMinPct/MaxPct` 做命中率整形，
说明**命中率是对外可见的指标**。而 prompt cache 是**前缀匹配**——请求体前缀变一个字节，
后面全部失效。下面三个改动都会改前缀：

| 改动 | 为什么伤缓存 |
|---|---|
| 断流续写（§1.5） | 把已生成文本塞进 `history` 再重发，前缀变了 → 该轮缓存全丢 |
| 请求体预检裁剪（§2.2） | 裁掉最旧的历史 = 改前缀 → 缓存全丢，且每次裁剪点不同会导致**长期命不中** |
| 空响应换凭据重试（§2.1） | Kiro 侧缓存按账号隔离，换号 = 冷启动 |

**裁剪方向上有个绕不开的矛盾**：从**最旧**的历史裁，语义上最合理（旧的确实不重要），
但对缓存最致命（前缀变了）；从**最新**的裁，缓存能保住，但砍掉的恰恰是最相关的内容。
这个取舍必须明确选一边，不能含糊。

#### (b) token / credits 消耗会上升

费用走 `cost = credits × credit_price`（`admin/profit.rs:398`），
credits 来自上游 `metering.usage`（`stream.rs:2396`）。以下都在增加上游调用：

- 续写：失败即翻倍。`stream_read_error` 那类单轮已烧 14187 output token，续写再来一轮可能 3 万+
- 换凭据重试：多一次完整上游调用
- usage 报真值：客户端会**更频繁触发 auto-compact**，而 compact 本身是一次额外的 LLM 调用

叠加号池只剩 3 个凭据的现状，配额压力会更紧。

#### (c) 客户能直接感觉到的变化

- 续写产生的内容重复或语气断裂
- 预检裁剪是**有损的，且客户不知情**——模型看不到完整截图/工具结果，可能答错
- usage 报高后压缩变频繁，客户会觉得"上下文老是被压掉"

---

### 4.1 `stream_tail` 存原始字节（§1.3）

| | |
|---|---|
| **收益** | 续写可验证、断流可复盘。没有它后面三批都是盲改 |
| **代价** | 磁盘 +300–600 MB（zstd 后）；每条断流快照多几十 KB 写入 + 压缩 CPU |
| **⚠️ 主要风险** | **隐私**：现在存的是 sha256 摘要，什么内容都没有；改成存原文 = 把**客户对话正文和模型输出**落盘，保留 7 天，且 Admin UI 可查 |
| **客户影响** | 无（纯服务端观测） |

**这条的真正代价不是磁盘，是数据面。** 上游 event-stream 帧里就是模型正文。
落盘前必须想清楚三件事：谁能看、存多久、要不要脱敏。建议：
只对 `stream_idle_timeout` / `stream_read_error` / `stream_interrupted` 三类存；
调小 `STREAM_TAIL_MAX_BYTES`（续写只需末尾几十 KB）；
考虑对 `stream_tail` 单独设更短的保留期，并复用现有 `sanitize` 对可识别的敏感字段脱敏。

腾挪空间：§3.4 省下的 3.2 GB 正好覆盖这里的增量。

### 4.2 断流续写（§1.5）——**风险最高的一项**

| | |
|---|---|
| **收益** | 每天挽回 ~210 条断流；`stream_read_error` 那 101 条平均已跑 11.5 分钟、烧了 14187 output token |
| **代价** | 额度翻倍风险；延迟增加（客户端已等过 120 s 空闲超时，续写要再发一次上游请求）；该轮 prompt cache 全丢 |
| **⚠️ 最危险的失败模式** | **重复的 `tool_use` 导致副作用重复执行** |
| **客户影响** | 直接可感知 |

**为什么 tool_use 重复最危险**：内容重复只是难看，但如果续写产生了一个和断流前重复的
`tool_use`，Claude Code 会**真的再执行一次** —— 同一个 `Write` 写两遍、同一个 `Bash` 跑两遍。
这比"断流了让用户重来"糟糕得多。**这是 §1.5 表格里「断在 tool_use 中不能续」的真正理由**，
不只是 JSON 拼不回来。

**thinking 的风险同理**：续写轮产生的新 thinking block 签名对不上，
客户端校验失败会直接报错 —— 比不续写更糟。所以 §1.5 建议默认关闭。

必须配的护栏：
1. 单请求续写总 output token 上限，超了如实报错而不是继续烧
2. 只在**已完成的 block 边界**续写，断在 block 中间宁可不续
3. `ClientClosed` 一律不续（客户端都走了，纯浪费额度）
4. 续写轮次落进 `traces.db`，否则线上无法评估收益与损耗比

### 4.3 空响应换凭据重试（§2.1）

| | |
|---|---|
| **收益** | 可能降低 1817 条空响应中账号侧问题的那部分 |
| **代价** | 换号 = Kiro 侧 prompt cache 冷启动，该轮 `cache_read` 归零；多一次完整上游调用的延迟与额度 |
| **⚠️ 风险** | 号池只剩 3 个凭据，来回换号可能加速触发风控 |
| **客户影响** | 延迟增加 |

**如果空响应根因是请求体而非账号，换凭据纯属白费一次调用。**
所以 §2.1 里把「先做归因埋点」排在换凭据前面 —— 这个顺序不能颠倒。

### 4.4 请求体过大预检压缩（§2.2）

| | |
|---|---|
| **收益** | 减少 482 条 `Input is too long` + 663 条 context exceeded 里的一部分无效上游调用 |
| **代价** | 图片重编码消耗 CPU，抬高 TTFB；裁剪改前缀 → 缓存失效（见 §4.0(a) 的方向矛盾） |
| **⚠️ 风险** | **有损且客户不知情** |
| **客户影响** | 直接可感知，且难以察觉原因 |

模型看不到完整截图或被截断的 tool_result，可能给出错误答案，
而客户只会看到"AI 答错了"，不知道是我们裁掉了内容。
最低要求：**裁剪必须在响应里留下可观测痕迹**（trace 记录裁了什么、裁了多少），
出问题时能对上账。

另外 `payload_limit_preempted` 那 407 条典型样本是 `2933116 > 2799833`，**只超 4.8%**。
这种"差一点点"的场景，优先考虑更温和的手段（只缩最大的那张图），
而不是直接砍历史轮次。

### 4.5 上游 400 `REQUEST_BODY_INVALID`（§2.3）

信息不足，尚未定方案，**暂无代价可评估**。先取样 `kiro_request` payload 定位。

### 4.6 自动压缩：两个方案的代价对比（§2.4）

#### 方案一：降压缩信号阈值到 85%（**推荐**）

| | |
|---|---|
| **收益** | 客户端在还有 15% 窗口时收到信号，compact 能成功；解掉 663 条 context exceeded 里的死锁 |
| **代价** | 改一个常量。建议做成配置项 |
| **⚠️ 风险** | **依赖客户端认 `model_context_window_exceeded` 这个 stop_reason**。原 ISSUE 记录了 100% 时发过 5 次、会话仍死 —— 那是因为 100% 时压缩本身没余量，但也无法排除「客户端压根不按这个信号压缩」。**必须用诊断计数验证，不能假定有效** |
| **计费影响** | **零** |
| **客户影响** | 压缩变频繁（compact 本身消耗 token，但远低于撞 400 重来） |

阈值调太低会让正常会话被无谓压缩（丢上下文、答案变差）。85% 的取值依据见 §2.4.6。

#### 方案二：把 usage 报成真值（§2.4.4，**已确认有严重商业影响，暂缓**）

| | |
|---|---|
| **收益** | 从源头修好，客户端按自己的阈值触发，不依赖 stop_reason |
| **⚠️ 商业风险** | **客户账单平均翻倍，长会话涨 3.5 倍** —— 已由 NewAPI 源码 + 拆分逻辑确认，不是推测（见 §2.4.5） |
| **计费影响** | 直接、成倍 |
| **客户影响** | 账单 + 压缩频率，双重可感知 |

已确认的机制：NewAPI 的 `HandleClaudeResponseData` 从 `message_start` 取 `input_tokens`
扣额度；`split_raw()` 按无量纲比例拆分，三个桶随 total 等比放大，没有廉价桶缓冲。

其余风险（若将来重启此方案）：

- **外推是估算**。报高了客户端过早压缩（丢上下文）；报低了还是不触发。必须配窗口上限护栏。
- **15% 的请求没有 `session_hash`**，退回现有行为 —— 部分修复，不是全量修复。
- 会话状态在内存里，多实例部署不共享。当前单实例，横向扩容会退化。

### 4.7 工具入参别名归一化（§3.1）

| | |
|---|---|
| **收益** | 消掉 `filePath`/`Query`/`SearchPath` 这批整轮拒绝 |
| **代价** | 别名表需要跟着客户端工具演进维护 |
| **⚠️ 风险** | **我们在改写模型的输出**——如果模型确实想传一个语义不同的字段，改错了就是让工具执行错误的操作 |
| **客户影响** | 正常情况下是正向的（本来整轮失败，现在成功） |

风险可控但非零。原则：**只做大小写 / 命名风格的机械映射，不做语义猜测**。
`filePath → file_path` 可以，"这个字段看起来像是想表达 X" 不行。

另外这会**掩盖上游的真实问题**（模型本来就该输出正确字段），
所以归一化的同时要保留计数埋点，别把问题彻底藏起来。

### 4.8 错误分类口径统一（§3.2）

| | |
|---|---|
| **收益** | 按 error_type 的统计变得可信；修完 §3.1 才能验证效果 |
| **代价** | 低 |
| **⚠️ 风险** | **历史数据断层**：改动前后不可比，现有报表和告警阈值要重新校准；如果有外部监控按 error_type 告警，会突然失效 |
| **客户影响** | 无 |

建议改动前后各留一段对照期，并在 CHANGELOG 里明确写出口径变更。

### 4.9 thinking WARN 降噪（§3.3）

| | |
|---|---|
| **收益** | 日志可回溯窗口从 4.3 小时显著拉长（这一条占 WARN 的 70%） |
| **代价** | 低 |
| **⚠️ 风险** | 降到 `debug` 后，生产默认 `RUST_LOG=info` 就**完全看不到了**——将来要查"某个模型是不是不返回 reasoning"会失去逐条线索 |
| **客户影响** | 无 |

建议保留按模型的聚合计数（每分钟一条汇总）而不是直接删，
趋势和个案至少保住趋势。

### 4.10 `client_disconnected` 不存 body（§3.4）

| | |
|---|---|
| **收益** | 立省 3.2 GB，正好腾给 §4.1 的 `stream_tail` |
| **代价** | 低 |
| **⚠️ 风险** | 客户投诉"我的请求断了"时**无法复现**——多数 `client_disconnected` 确实是客户端主动关，但也可能是我们太慢把客户逼断的 |
| **客户影响** | 无（但影响我们的售后排查能力） |

建议保留元数据 + 按比例采样存少量 body（比如 1%），
兼顾空间和可排查性。

### 4.11 `traces.db` WAL checkpoint

| | |
|---|---|
| **收益** | 回收 440 MB |
| **代价** | checkpoint 期间会短暂阻塞写入 |
| **⚠️ 风险** | 高峰期 checkpoint 可能让 trace 写入队列积压（虽然是异步有界队列，满了会丢诊断记录而不是阻塞客户请求） |
| **客户影响** | 无 |

放在低峰期跑。

---

## 5. 建议的执行顺序

| 批次 | 内容 | 依赖 | 风险 |
|---|---|---|---|
| **第 1 批** | §1.3 `stream_tail` 存原始字节（按 §6.1 配置）· §3.2 错误分类统一 · §3.4 `client_disconnected` 采样存 body · §3.3 WARN 降噪 · **§2.4 压缩信号阈值 → 85%** | 无 | 低，零计费影响、可灰度回滚 |
| **第 2 批** | §1.5 步骤 1–2（放开终止态与 stop_reason，按 §6.2 配护栏）· §3.1 工具别名归一化 · §2.1 空响应归因埋点 | 第 1 批（要靠它验证） | 中 |
| **第 3 批** | §1.5 步骤 3（按 block 边界续写）· §2.2 请求体预检压缩 · §2.1 换凭据重试 | 第 2 批 | 中高 |
| **第 4 批** | §1.5 步骤 4（thinking 续写）· §2.3 上游 400 定位 | 取样与评估 | 高，可能不做 |

第 1 批全是"不改行为、只改观测"的改动，**建议先合这一批再谈其他**——
现在最大的问题不是不知道怎么修，是修完了没法验证。

---

## 6. 需要你拍板的点

下游计费口径已查清（§2.4.5），原第 1 条阻塞项解除 —— 结论是**别动 usage**。
剩下的每一条都给了推荐值，可以直接采纳或改数。

### 6.1 唯一还需要你批的：`stream_tail` 存原文的数据面

改完等于把**客户对话正文和模型输出**落盘、Admin UI 可查。技术上 10 行代码，合规上不是。
需要你确认：谁能访问、保留多久、要不要额外脱敏。

我的推荐配置：

| 项 | 推荐值 | 理由 |
|---|---|---|
| `STREAM_TAIL_MAX_BYTES` | 256 KB → **32 KB** | 续写只需定位末尾几个 event-stream 帧；实测样本 92 KB 里绝大部分是无用前段 |
| 存哪些类型 | 仅 `stream_idle_timeout` / `stream_read_error` / `stream_interrupted` | 三类共 925 条/4 天，其余类型不存 |
| `stream_tail` 保留期 | **48 小时**（独立于快照库的 7 天） | 断流排障基本在 1–2 天内完成；缩短窗口是最有效的隐私缓解 |
| 脱敏 | 复用现有 `sanitize_payload_data`，对可识别敏感字段打码 | 二进制帧里的正文无法结构化脱敏，只能靠保留期 + 访问控制 |

按 32 KB × 925 条 × (48h/96h) ≈ **15 MB**，zstd 后更小 —— 比我上一版估的 300–600 MB 小两个数量级，
因为把上限从 256 KB 砍到 32 KB、保留期从 7 天砍到 2 天。§3.4 省下的 3.2 GB 完全够用。

### 6.2 推荐值（可直接采纳）

| 决策 | 推荐值 | 依据 |
|---|---|---|
| **压缩信号阈值** | **85%**，做成配置项 | 社区报告触发点 83.5%–92%，取下沿并留 15% 给 compact 本身（§2.4.6） |
| **续写轮数上限** | **2**（不复用 `autoContinueMax=3`） | 断流续写风险高于 max_tokens 续写，轮数要更保守 |
| **续写单请求 output token 硬上限** | **40000** | `stream_read_error` 均值 14187，覆盖「原轮 + 一轮续写」约 28000 并留余量；同时挡住 3 轮 × 14187 ≈ 57000 的失控场景 |
| **续写触发下限** | 已提交 **≥ 512** output token | 低于此不值得为一小段残片重发整个请求体（请求体常有 800 KB） |
| **`client_disconnected` body 采样率** | **1%** | 19088 条/4 天 ≈ 4770/天，1% ≈ 48 条/天 × 169 KB ≈ 8 MB/天，7 天 56 MB，可接受 |
| **thinking 续写** | **不做** | signature 对不上会让客户端直接报错，比不续写更糟 |
| **`ClientClosed` 续写** | **不续** | 客户端已断开，续写纯浪费额度 |

### 6.3 请求体裁剪：不用「选一边」，走三级阶梯

我上一版把这个写成「裁最旧 vs 裁最新，必须选一边」—— 看了数据后这个二选一是伪命题。

`payload_limit_preempted` 的典型样本是 `2933116 > 2799833`，**只超 4.8%**。
而生产里存在 `image_count=46 / image_total_b64_kb=8886` 这种请求。
也就是说：**绝大多数超限场景靠压图片就能解决，压根不用碰文本历史。**

推荐三级阶梯，前两级完全不动文本前缀 → **缓存保住**：

| 级别 | 动作 | 伤缓存？ |
|---|---|---|
| 1 | 缩当前轮图片（复用 `imageRetryHistoryMaxDimension=960 / JpegQuality=60`，但首轮就生效） | 否 |
| 2 | 缩历史图片 / 超过 N 张只留最近几张 | 否 |
| 3 | 截断最旧的 `tool_result` 正文，保留配对结构 | 是 |

只有走到第 3 级才伤前缀。按 4.8% 的典型超限幅度，绝大部分请求在第 1–2 级就够了。

### 6.4 其余建议

**第 1 批单独发一版** —— 它是唯一不改客户可见行为的一批（§4.1 的隐私问题按 §6.1 处理后风险可控），
能让后面三批有据可依。建议第 1 批加上 §2.4 的阈值改动（也是零计费影响、可灰度回滚），
这样第一版就能验证自动压缩是否真的被触发。
