# 疑点：自动压缩未触发 / 长会话撞字节墙死锁

> 状态：**诊断监控已实现，根因仍待线上采样确认；尚未修改或修复代理响应行为**
> 记录时间：2026-07-28；诊断能力更新于 2026-07-29
> 环境：测试站 `kiro-rs-test`（43.225.196.10:18792），原始证据采自 commit `3a0e9b8` 之后
> 相关模型：`claude-opus-4-8` / `claude-opus-5`，声明窗口 1,000,000 token

## 现象

长会话（~1300+ 条消息，含多张截图）跑到后期，请求持续返回：

```text
400 Bad Request {"message":"Input content length exceeds threshold.","reason":"CONTENT_LENGTH_EXCEEDS_THRESHOLD"}
```

客户端「继续」也一直失败，会话卡死无法恢复。用户预期：应在此之前自动触发 `/compact`
压缩历史，不该走到 400。

## 核心矛盾（本文件的重点）

排查中出现一个**互相矛盾**的证据组合，是当前未解决的关键：

1. 上游 token 占用**确实到过 99.99% / 100%**（见证据 B、F）——不是早期以为的「只有 55%」。
2. 100% 时代理**确实下发了** `model_context_window_exceeded`（见证据 G，240 分钟内 5 次）。
3. **但会话仍然死在 400**（见证据 C、H）。

也就是说：压缩信号发出去了，客户端却没有成功压缩、或压缩后仍然撞墙。这与
「auto-compact 由响应 usage.input_tokens 驱动」的公开机制（见参考链接）对不上——
按理 token 到 85%~92% 就该触发压缩，轮不到 100%，更轮不到持续 400。

**待查**：为什么 token 到 99%+、信号已下发，客户端仍未把历史压下去？

## 已增加的只读诊断监控（2026-07-29）

这次改动只观察现有请求和响应边界，**不改变 HTTP/SSE 字节、事件顺序、重试、usage、
计费或自动压缩触发逻辑**。因此它不是问题修复，只是为了用线上样本把根因缩小到可验证的分支。

### 开关

- 配置字段：`autoCompactDiagnosticsEnabled`，默认开启。
- Admin「请求日志 → 治理设置 → 自动压缩诊断」可随时独立开关。
- 它独立于 `traceEnabled`；关闭后请求入口立即短路，不做 session hash、请求形状扫描或诊断 JSON 生成。
- 不需要开启全局 DEBUG 日志。Docker 高压力结论使用独立的
  `auto_compact_diagnostics` target 以 WARN 输出；关闭开关后两类记录都会停止。

### Docker 安全结论

只有命中以下任一高压力条件才输出一行：上下文占比 ≥80%、请求体或上游请求体 ≥2,500,000
字节、或观察到 payload limit。字段仅包含：

- `diagnosis`
- SHA-256 `session_hash`（拿不到时为 `none`）
- 数字 `client_version`
- `request_body_bytes`、`upstream_request_max_bytes`
- `upstream_context_tokens`、`upstream_context_percentage`、`client_reported_tokens`
- `message_start_enqueued`、`context_window_exceeded_enqueued`
- `client_disconnected`、`payload_limit_observed`

Docker 不输出请求正文、工具参数、请求头、原始会话 ID、Token 或凭证。

### traces.db 详细安全计数

`traces` 表新增八列：

```text
session_hash
client_version
compaction_diagnosis
request_body_bytes
upstream_context_tokens
upstream_context_percentage
client_reported_tokens
compaction_diagnostics_json
```

并增加 `idx_traces_session_ts`、`idx_traces_compaction_diagnosis` 两个索引。详细 JSON 只保存
字节数、对象/事件计数、布尔边界和安全枚举，不保存请求正文、工具参数、请求头或凭证。

写入继续复用原有有界异步 trace 队列与后台 SQLite 事务：请求线程不等待数据库锁；队列满时
丢弃诊断记录而不是阻塞客户请求。跨请求会话推断也只在后台写事务中进行。

### Admin 查询

Admin 请求日志已支持：

- 按 `compactionDiagnosis` 分类筛选；
- `highPressureOnly=true` 只看高压力记录；
- 从详情里的 SHA-256 hash 联查同一 `sessionHash` 的前后请求；
- 展开查看请求大小、上下文占比、客户端收到的 input token、事件计数和 SSE 入队边界。

标准实时/缓冲 SSE、strict-JSON 本地流式封装和 mixed web-search 最终响应都纳入同一观测口径；
`contextWindowExceededEnqueued` 会直接说明携带 `model_context_window_exceeded` 的
`message_delta` 是否已走到客户端响应边界。

重点观察两种跨请求结论：

- `suspected_client_compaction_not_triggered`：前次已暴露高上下文信号，本次请求体仍不小于前次
  85% 且仍 ≥2.5MB；
- `suspected_compaction_insufficient`：本次请求体至少缩小 20%，但仍撞 payload limit。

这些名称表示**保守推断**，不是已经证明的根因。需要部署后收集同会话连续样本，再决定是否修复
客户端兼容、信号暴露路径或字节侧治理。

## 两把尺：token 窗口 vs 字节墙

| 限制 | 量什么 | 实测上限 |
|---|---|---|
| context window | token | 1,000,000（证据 B 反推确认） |
| CONTENT_LENGTH | 请求体**字节** | ~3.0–3.5 MB（证据 C） |

失败请求 `primary_bytes ≈ 3.48 MB`，图片激进压缩后 `retry_bytes ≈ 3.08 MB`——
压完仍超，说明主体是**文本历史**而非图片（图片压缩只省 ~41 万字节）。

## 测试站日志证据（2026-07-28，UTC）

### A. 容器 / 版本

```text
started = 2026-07-28T07:02:07Z
二进制 mtime = 2026-07-28 07:02:04Z   （即已部署当时最新代码）
```

### B. 窗口反推：token ÷ percentage = 真实窗口

```text
upstream_context_tokens=998388  context_usage_percentage=99.8389
upstream_context_tokens=999021  context_usage_percentage=99.9022
upstream_context_tokens=999985  context_usage_percentage=99.9986
```

`998388 / 99.84% ≈ 1,000,000` → 上游按 **1M** 窗口计算，与我们声明的
`max_input_tokens: 1_000_000`（`src/anthropic/model_catalog.rs:259`）一致。
**窗口口径两边不矛盾**（曾怀疑上游按 200K，已排除）。

### C. 字节墙：失败重试的 primary / retry 体积

```text
primary_bytes=3483104  retry_bytes=3073233
primary_bytes=3485581  retry_bytes=3075710
primary_bytes=3488601  retry_bytes=3078730
primary_bytes=3488611  retry_bytes=3078740
```

图片激进压缩把 ~3.48MB 压到 ~3.08MB（省 ~41 万字节）后**仍被上游拒**。
→ 字节墙约在 3MB 附近，且主体是文本、压图片救不回来。

### D. CONTENT_LENGTH 预警计数（全量日志，7 天）

```text
1172 次   （全部是 handlers 的 WARN 预警文案，非独立 ERROR 行）
```

注：`CONTENT_LENGTH_EXCEEDS_THRESHOLD` 这个串在代码里主要出现于
`src/anthropic/handlers.rs:603` 的分类文案与预警 WARN；上游真实返回时被
`call_api_with_content_length_retry` 捕获并触发图片压缩重试（证据 C）。

### E. 图片预算 before / after 样本（KB）

```text
image_before_b64_kb=1409  image_after_b64_kb=844
image_before_b64_kb=1409  image_after_b64_kb=985
image_before_b64_kb=2528  image_after_b64_kb=849
```

### F. 峰值上游 token

```text
1000000
1000000
```

### G. 100% 时是否下发 model_context_window_exceeded

```text
240 分钟内出现 5 次
```

→ 信号**确实发了**。

### H. 99%+ 事件与 400 的时间线（交错）

```text
10:07:00Z  primary_bytes=3483104        ← 400 重试
10:07:22Z  percentage=99.9986           ← token 触顶
10:10:30Z  primary_bytes=3485581
10:10:38Z  primary_bytes=3485581
10:37:43Z  primary_bytes=3488601
10:37:51Z  primary_bytes=3488601
10:40:47Z  primary_bytes=3488611
10:43:55Z  primary_bytes=3485581
```

token 触顶与 400 交替出现，跨 ~37 分钟持续失败——**未见自动压缩把体积降下来**。

## 关键代码位置

- 压缩信号触发：`src/anthropic/stream.rs:2379`
  ```rust
  if context_usage.context_usage_percentage >= 100.0 {
      self.state_manager.set_stop_reason("model_context_window_exceeded");
  }
  ```
  只在 `>= 100%` 时发。若客户端按 85%~92% 触发压缩，这个信号来得太晚（属事后通知）。
- usage 上报口径：`src/anthropic/usage.rs:36` `split_api()` 已改为
  **上游真实占用优先**（`upstream_context_tokens.unwrap_or(client_visible_tokens)`），
  commit `5f785cf`。即回报给客户端的 input_tokens 已是上游真实值，非本地估算。
- 字节超限分类：`src/anthropic/handlers.rs:603`（返回 400 invalid_request_error）。
- 图片压缩重试：`call_api_with_content_length_retry`（provider 层），仅压图片，
  纯文本无可压 → 证据 C 显示压完仍超。
- 计费与 token 解耦（重要）：费用 `cost = credits × credit_price`
  （`src/admin/profit.rs:398`），`credits` 来自上游 `metering.usage`
  （`src/anthropic/stream.rs:2396`），与回报的 input_tokens 是 `UsageRecord` 里
  **两个独立字段**（`src/admin/usage_stats.rs:41,49`）。
  → 调整 input_tokens 上报**不影响本侧费用报表**，但可能影响「按 token 向终端客户结算」的场景。

## 待验证 / 待决策

1. **[最高优先] 部署诊断分支并采集同一 session hash 的连续高压力样本。**重点确认：
   - `message_start_enqueued` 是否为 true；
   - `client_reported_tokens` 是否接近 `upstream_context_tokens`；
   - 下一次请求体是否明显缩小；
   - 缩小后是否仍出现 `payload_limit_observed`。

2. **为什么信号已发、token 已 100%，客户端仍未压下去？**
   - 可能：客户端 auto-compact 用的窗口口径 ≠ 我们声明的 1M（社区 issue #50204 / #34332
     报告过「扩展上下文模型 auto-compact 口径不一致」）。
   - 可能：压缩本身需要一次成功请求，但请求已被 400 挡住 → compact 也带全量历史 → 同样 400 →
     死锁（issue #24976 / #48893 描述过类似「上下文耗尽后 compact 自身失败」）。
   - 诊断目标：从代理侧确认 usage 是否真正入队给客户端，以及下一请求的安全形状如何变化；仍需客户端侧
     观察收到信号后是否发起 compact 请求。

3. **用户观察「别人的反代能自动触发压缩」**——差异来源未确认。三种假设：
   - 别人上游窗口声明为 200K → 相同 token 数算出的百分比 ×5，早早触发压缩；
   - 别人回报的 token 值更高（提前触发）；
   - 别人没有这种超大图片 + 超长文本的会话，未撞墙。

4. **字节侧治理（当前唯一确定可落地的方向）**：
   - A. 更激进的历史图片压缩 / 超过 N 张只留最近几张；
   - B. 字节超阈值时服务端裁最旧历史轮次（有损，客户端不知情）。
   - 二者都作用在真正的墙（字节）上，不依赖 token 信号；但都可能影响客户体验，本诊断分支没有实施。

## 参考

- Anthropic 官方 issue：auto-compact 由 real server-side token count 驱动，UI 指示器不同步
  — https://github.com/anthropics/claude-code/issues/50204
- auto-compact 在 ~76K（1M 窗口的 ~92%）过早触发，丢弃 924K headroom
  — https://github.com/anthropics/claude-code/issues/34332
- Context Limit Reached 应 auto-compact 而非直接失败
  — https://github.com/anthropics/claude-code/issues/24976
- 200K 上下文耗尽后 auto-compact 失败（compaction API 需要扩展上下文）
  — https://github.com/anthropics/claude-code/issues/48893
- auto-compact 读 API 响应的 usage 字段驱动（非本地 tokenizer）
  — https://wowhow.hashnode.dev/claude-code-context-management-definitive-guide-2026
- 触发阈值 ~83.5% — https://claudefa.st/blog/guide/mechanics/context-buffer-management
- Anthropic compaction 文档 — https://platform.claude.com/docs/en/docs/build-with-claude/compaction
