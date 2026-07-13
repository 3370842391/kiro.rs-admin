# 工具 JSON 中断与图片总预算修复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 安全恢复漏发结束标记的完整工具调用，为未提交的半截工具调用增加一次重试，并在 Kiro 出站前治理多轮历史图片总量。

**Architecture:** `ToolJsonAccumulator` 负责严格分类完整、EOF 半截和非法 JSON，并以整批原子方式提交。handler 使用“首个语义事件前的试运行缓冲”决定是否可以透明重试；图片治理在最终 `KiroRequest` 上运行，由 `KiroProvider` 持有可热更新的类型化策略。

**Tech Stack:** Rust 1.92、Axum、Tokio/Futures、serde_json、image、Bun/React/TypeScript。

---

## 文件结构

- Create: `src/anthropic/tool_attempt.rs` — 工具 attempt 分类、一次重试策略和试运行缓冲状态。
- Create: `src/kiro/image_budget.rs` — 遍历 `KiroRequest`、历史图片重编码、总预算校验和统计。
- Modify: `src/anthropic/stream.rs` — 工具 JSON 残留严格分类与整批原子提交。
- Modify: `src/anthropic/handlers.rs` — 非流式、实时流、缓冲流的一次重试及图片预算集成。
- Modify: `src/image_resize.rs` — 增加不依赖环境变量的确定性重编码入口。
- Modify: `src/anthropic/converter.rs` — 保留原始图片到统一预算阶段，并停止删除重复历史图片。
- Modify: `src/kiro/provider.rs` — 持有运行时 `ImageBudgetPolicy` 并提供读写方法。
- Modify: `src/kiro/mod.rs` — 导出图片预算模块。
- Modify: `src/anthropic/mod.rs` — 导出工具 attempt 模块。
- Modify: `src/model/config.rs` — 持久化图片预算配置。
- Modify: `src/admin/types.rs` — Admin 图片预算 DTO。
- Modify: `src/admin/service.rs` — 校验、热更新和持久化图片预算。
- Modify: `src/admin/handlers.rs` — 图片预算 GET/PUT handler。
- Modify: `src/admin/router.rs` — 注册图片预算路由。
- Create: `admin-ui/src/api/image-budget.ts` — 图片预算 API。
- Create: `admin-ui/src/hooks/use-image-budget.ts` — React Query hooks。
- Create: `admin-ui/src/lib/image-budget.ts` — 前端校验函数。
- Create: `admin-ui/src/lib/image-budget.test.ts` — 前端校验测试。
- Create: `admin-ui/src/components/image-budget-dialog.tsx` — 图片预算管理弹窗。
- Modify: `admin-ui/src/components/topbar-tools.tsx` — 增加入口。
- Modify: `admin-ui/src/types/api.ts` — TypeScript DTO。

### Task 1: 严格分类并原子提交工具 JSON 残留

**Files:**
- Modify: `src/anthropic/stream.rs:934-1115`
- Test: `src/anthropic/stream.rs:3333-3505`

- [ ] **Step 1: 写合法 JSON 漏 stop 的 RED 测试**

```rust
#[test]
fn tool_json_accumulator_salvages_complete_json_without_stop() {
    let mut acc = ToolJsonAccumulator::new();
    let mut map = HashMap::new();
    map.insert("fs_write".into(), "Write".into());
    acc.push(
        &tool_evt("tool_1", "fs_write", r#"{"path":"/tmp/a","text":"ok"}"#, false),
        &map,
    )
    .unwrap();

    let (completed, error) = acc.finish(&map);
    assert!(error.is_none());
    assert_eq!(completed.len(), 1);
    assert_eq!(completed[0].name, "Write");
    assert_eq!(completed[0].input["file_path"], "/tmp/a");
    assert_eq!(completed[0].input["content"], "ok");
}
```

- [ ] **Step 2: 写非法、EOF 半截和批次原子性的 RED 测试**

```rust
#[test]
fn tool_json_accumulator_distinguishes_invalid_from_incomplete_at_finish() {
    let mut incomplete = ToolJsonAccumulator::new();
    incomplete.push(&tool_evt("a", "fs_write", r#"{"path":"/a"#, false), &HashMap::new()).unwrap();
    assert!(matches!(incomplete.finish(&HashMap::new()).1, Some(ToolJsonAccumulatorError::IncompleteJson { .. })));

    let mut invalid = ToolJsonAccumulator::new();
    invalid.push(&tool_evt("b", "fs_write", r#"{"path":]"#, false), &HashMap::new()).unwrap();
    assert!(matches!(invalid.finish(&HashMap::new()).1, Some(ToolJsonAccumulatorError::InvalidJson { .. })));
}

#[test]
fn tool_json_accumulator_does_not_partially_commit_mixed_finish_batch() {
    let mut acc = ToolJsonAccumulator::new();
    acc.push(&tool_evt("complete", "noop", "{}", false), &HashMap::new()).unwrap();
    acc.push(&tool_evt("half", "fs_write", r#"{"path":"/a"#, false), &HashMap::new()).unwrap();
    let (completed, error) = acc.finish(&HashMap::new());
    assert!(completed.is_empty());
    assert!(matches!(error, Some(ToolJsonAccumulatorError::IncompleteJson { .. })));
}
```

- [ ] **Step 3: 运行 RED 测试并确认失败原因**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'; $env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'
cargo test --locked tool_json_accumulator_ -- --nocapture
```

Expected: 完整 JSON 测试得到 `IncompleteJson`；批次原子性测试错误地得到一个 completed 项。

- [ ] **Step 4: 实现严格残留分类**

在 `ToolJsonAccumulator::finish()` 中先分类全部 entry，只有整批无错误时返回 completed：

```rust
fn parse_finished_input(
    tool_use_id: &str,
    name: &str,
    input_json: &str,
) -> Result<serde_json::Value, ToolJsonAccumulatorError> {
    if input_json.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str::<serde_json::Value>(input_json).map_err(|error| {
        if error.is_eof() {
            ToolJsonAccumulatorError::IncompleteJson {
                tool_use_id: tool_use_id.to_owned(),
                name: name.to_owned(),
                bytes: input_json.len(),
            }
        } else {
            ToolJsonAccumulatorError::InvalidJson {
                tool_use_id: tool_use_id.to_owned(),
                name: name.to_owned(),
                message: error.to_string(),
            }
        }
    })
}
```

先把 `CompletedToolUse` 放入局部 `candidate`；发现任意错误后返回 `(Vec::new(), Some(error))`，不得返回部分 candidate。

- [ ] **Step 5: 更新旧混合测试的契约**

把 `tool_json_accumulator_finish_mixes_salvage_and_error` 重命名为 `tool_json_accumulator_finish_is_atomic_when_any_entry_errors`，断言 `salvaged.is_empty()`。

- [ ] **Step 6: 运行工具累积器和流终止测试**

Run:

```powershell
cargo test --locked tool_json_accumulator_ -- --nocapture
cargo test --locked incomplete_tool_signal_emits_error_without_success_terminal_events -- --nocapture
```

Expected: 全部 PASS；真半截仍没有 `message_delta`/`message_stop`。

- [ ] **Step 7: 提交 Task 1**

```powershell
git add -- src/anthropic/stream.rs
git commit -m "fix(tool): 安全打捞完整工具JSON残留"
```

### Task 2: 建立一次重试策略和非流式工具 attempt

**Files:**
- Create: `src/anthropic/tool_attempt.rs`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/handlers.rs:2470-2700`
- Test: `src/anthropic/tool_attempt.rs`
- Test: `src/anthropic/handlers.rs:4100-4220`

- [ ] **Step 1: 写重试判定 RED 测试**

```rust
#[test]
fn retries_only_first_incomplete_uncommitted_attempt() {
    let retryable = ToolAttemptState {
        attempt_index: 0,
        terminal_error: Some(ToolJsonAccumulatorError::IncompleteJson {
            tool_use_id: "t".into(), name: "fs_write".into(), bytes: 56,
        }),
        semantic_output_started: false,
        tool_forwarded: false,
    };
    assert!(retryable.should_retry());
    assert!(!ToolAttemptState { attempt_index: 1, ..retryable.clone() }.should_retry());
    assert!(!ToolAttemptState { semantic_output_started: true, ..retryable.clone() }.should_retry());
    assert!(!ToolAttemptState { tool_forwarded: true, ..retryable }.should_retry());
}
```

- [ ] **Step 2: 运行 RED 测试**

Run: `cargo test --locked tool_attempt -- --nocapture`

Expected: FAIL，因为 `anthropic::tool_attempt` 尚不存在。

- [ ] **Step 3: 创建重试策略模块**

```rust
#[derive(Debug, Clone)]
pub struct ToolAttemptState {
    pub attempt_index: u8,
    pub terminal_error: Option<ToolJsonAccumulatorError>,
    pub semantic_output_started: bool,
    pub tool_forwarded: bool,
}

impl ToolAttemptState {
    pub fn should_retry(&self) -> bool {
        self.attempt_index == 0
            && !self.semantic_output_started
            && !self.tool_forwarded
            && matches!(self.terminal_error, Some(ToolJsonAccumulatorError::IncompleteJson { .. }))
    }
}
```

在 `src/anthropic/mod.rs` 增加 `pub(crate) mod tool_attempt;`。

- [ ] **Step 4: 把非流式解析抽成单次收集函数**

在 `handlers.rs` 新增：

```rust
struct NonStreamToolAttempt {
    body: ParsedNonStreamBody,
    state: ToolAttemptState,
    credential_id: u64,
    received_bytes: u64,
}

async fn collect_non_stream_tool_attempt(
    provider: Arc<KiroProvider>,
    request_body: &str,
    setup: &NonStreamSetup,
    attempt_index: u8,
) -> anyhow::Result<NonStreamToolAttempt>;
```

把当前 `handle_non_stream_request()` 中“调用 provider、读取 body、decode、finish accumulator”的代码移入该函数；不要在收集函数里记录最终 hook 或返回 Axum Response。

- [ ] **Step 5: 写第一次半截、第二次完整的 RED 测试**

使用与严格 JSON recovery 相同的 closure 测试形状：

```rust
fn fake_tool_attempt(
    attempt_index: u8,
    error: Option<ToolJsonAccumulatorError>,
    tool_uses: Vec<serde_json::Value>,
) -> NonStreamToolAttempt {
    NonStreamToolAttempt {
        body: ParsedNonStreamBody::for_test(tool_uses),
        state: ToolAttemptState {
            attempt_index,
            terminal_error: error,
            semantic_output_started: false,
            tool_forwarded: false,
        },
        credential_id: attempt_index as u64 + 1,
        received_bytes: 64,
    }
}

#[tokio::test]
async fn non_stream_tool_retry_returns_only_second_complete_attempt() {
    let mut attempts = VecDeque::from([
        fake_tool_attempt(0, Some(incomplete_error()), vec![]),
        fake_tool_attempt(1, None, vec![tool_block("tool_2", "Write")]),
    ]);
    let recovered = recover_non_stream_tool_attempts(|_| async {
        Ok(attempts.pop_front().unwrap())
    }).await.unwrap();
    assert_eq!(recovered.attempts.len(), 2);
    assert_eq!(recovered.final_attempt.body.tool_uses.len(), 1);
    assert_eq!(recovered.final_attempt.body.tool_uses[0]["id"], "tool_2");
}
```

- [ ] **Step 6: 实现最多两次的收集循环**

```rust
for attempt_index in 0..=1 {
    let attempt = collect(attempt_index).await?;
    let retry = attempt.state.should_retry();
    attempts.push(attempt);
    if !retry {
        return Ok(RecoveredNonStreamAttempt { attempts });
    }
}
unreachable!("second attempt is always returned")
```

第一次 attempt 的 tool blocks、文本、thinking 和 usage 不得进入最终客户端响应；trace 仍保存两次 provider attempt。

- [ ] **Step 7: 集成 `handle_non_stream_request()`**

用 recovery helper 获取最终 attempt。第二次仍 `IncompleteJson` 时沿用 HTTP 502 `upstream_tool_json_error`；成功时只基于最终 attempt 构造 Anthropic message。hook 只记录一次最终客户端结果。

- [ ] **Step 8: 运行非流式重试测试与 handler 回归**

Run:

```powershell
cargo test --locked non_stream_tool_retry -- --nocapture
cargo test --locked non_stream -- --nocapture
```

Expected: 新测试和现有非流式工具、thinking、usage 测试全部 PASS。

- [ ] **Step 9: 提交 Task 2**

```powershell
git add -- src/anthropic/tool_attempt.rs src/anthropic/mod.rs src/anthropic/handlers.rs
git commit -m "fix(tool): 为未提交的半截工具调用增加重试"
```

### Task 3: 为实时流增加首语义事件前的试运行缓冲

**Files:**
- Modify: `src/anthropic/tool_attempt.rs`
- Modify: `src/anthropic/handlers.rs:1881-2430`
- Modify: `src/anthropic/handlers.rs:3399-3675`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写试运行缓冲 RED 测试**

```rust
#[test]
fn probation_buffer_discards_first_incomplete_attempt_before_commit() {
    let mut buffer = ProbationBuffer::default();
    buffer.push(message_start_event());
    buffer.push(error_event_for(incomplete_error()));
    assert!(buffer.retryable_terminal());
    assert!(buffer.take_for_retry().is_empty());
}

#[test]
fn probation_buffer_commits_after_first_text_and_never_retries() {
    let mut buffer = ProbationBuffer::default();
    buffer.push(message_start_event());
    buffer.push(text_delta_event("hello"));
    assert!(buffer.semantic_output_started());
    assert!(!buffer.retryable_terminal());
    assert_eq!(buffer.take_committed().len(), 2);
}
```

- [ ] **Step 2: 实现 `ProbationBuffer`**

```rust
#[derive(Default)]
pub struct ProbationBuffer {
    pending: Vec<SseEvent>,
    semantic_output_started: bool,
    tool_forwarded: bool,
    terminal_error: Option<ToolJsonAccumulatorError>,
}
```

`message_start`、SSE comment ping 不算语义输出；非空 text/thinking/redacted thinking 或完整 tool_use 才提交。出现半截 terminal error 且尚未提交时，清空 pending 并允许一次重试。

- [ ] **Step 3: 写实时流重试边界 RED 测试**

使用 test-only `run_probationary_attempts()` 驱动两组事件，避免依赖真实网络：

```rust
#[tokio::test]
async fn realtime_direct_tool_incomplete_then_complete_emits_one_message_start() {
    let mut attempts = VecDeque::from([
        ProbationaryAttempt::terminal(incomplete_error(), vec![message_start_event()]),
        ProbationaryAttempt::success(vec![message_start_event(), tool_use_event("tool_2"), message_stop_event()]),
    ]);
    let output = run_probationary_attempts(|_| async { Ok(attempts.pop_front().unwrap()) }).await.unwrap();
    assert_eq!(output.iter().filter(|e| e.event == "message_start").count(), 1);
    assert!(output.iter().any(|e| e.data.to_string().contains("tool_2")));
}

#[tokio::test]
async fn realtime_text_then_incomplete_tool_does_not_replay_text() {
    let calls = AtomicUsize::new(0);
    let output = run_probationary_attempts(|_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(ProbationaryAttempt::committed_then_error(vec![text_delta_event("hello")], incomplete_error())) }
    }).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(output.iter().filter(|e| e.data.to_string().contains("hello")).count(), 1);
    assert!(output.iter().any(|e| e.event == "error"));
}

#[tokio::test]
async fn realtime_complete_tool_then_second_incomplete_tool_does_not_retry() {
    let calls = AtomicUsize::new(0);
    let output = run_probationary_attempts(|_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async { Ok(ProbationaryAttempt::tool_committed_then_error("tool_1", incomplete_error())) }
    }).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(output.iter().filter(|e| e.data.to_string().contains("tool_1")).count(), 2);
    assert!(output.iter().any(|e| e.event == "error"));
}
```

测试断言最终只出现一个 `message_start`，成功路径只有第二轮工具 ID；已提交路径只发送 error，不发送成功 `message_stop`。

- [ ] **Step 4: 把 `create_sse_stream()` 改为最多两次 attempt 的状态机**

状态机持有 `provider`、原始 `request_body`、`group`、attempt index、当前 response stream、全新 `StreamContext` 和 `ProbationBuffer`。第一次未提交的 `IncompleteJson` 结束时重新调用 `call_api_stream()` 并创建新 context；ping 可以直接发送但不得提交 pending。

第二次、已输出 text/thinking 或已转发任何 tool_use 时不重试。重试不得追加提示词，也不得发送第二个 `message_start`。

- [ ] **Step 5: 让 early handshake 与 CC 缓冲路径复用同一判定**

early handshake 保留 HTTP 200 和 comment ping；语义事件仍进入 probation buffer。CC 缓冲路径本来没有语义输出，第一次半截时丢弃整个 buffered attempt 并重试，第二次才生成客户端事件。

- [ ] **Step 6: 运行三条流路径测试**

Run:

```powershell
cargo test --locked realtime_ -- --nocapture
cargo test --locked buffered_ -- --nocapture
cargo test --locked early_stream_ -- --nocapture
```

Expected: direct tool 可恢复；已有语义输出不重放；所有失败路径没有成功终止帧。

- [ ] **Step 7: 提交 Task 3**

```powershell
git add -- src/anthropic/tool_attempt.rs src/anthropic/handlers.rs
git commit -m "fix(stream): 安全重试未提交的工具流"
```

### Task 4: 实现 Kiro 出站图片总预算

**Files:**
- Create: `src/kiro/image_budget.rs`
- Modify: `src/kiro/mod.rs`
- Modify: `src/image_resize.rs`
- Modify: `src/anthropic/converter.rs`
- Test: `src/kiro/image_budget.rs`
- Test: `src/anthropic/converter.rs`

- [ ] **Step 1: 写图片预算 RED 测试**

测试构造有两张历史图片和一张当前图片的 `KiroRequest`：

```rust
#[test]
fn compresses_only_history_and_preserves_all_image_blocks() {
    let mut request = request_with_history_and_current_images();
    let current_before = current_image_bytes(&request).to_owned();
    let before_count = image_count(&request);
    let stats = apply_image_budget(&mut request, test_policy(120_000)).unwrap();
    assert_eq!(image_count(&request), before_count);
    assert_eq!(current_image_bytes(&request), current_before);
    assert!(stats.after_base64_bytes <= 120_000);
    assert!(stats.resized_history_images > 0);
}

#[test]
fn impossible_budget_returns_error_without_deleting_images() {
    let mut request = request_with_current_gif_only();
    let before = request.clone();
    let error = apply_image_budget(&mut request, test_policy(32_000)).unwrap_err();
    assert!(matches!(error, ImageBudgetError::Exceeded { .. }));
    assert_eq!(image_count(&request), image_count(&before));
}
```

- [ ] **Step 2: 运行 RED 测试**

Run: `cargo test --locked image_budget -- --nocapture`

Expected: FAIL，因为模块和 API 尚不存在。

- [ ] **Step 3: 让 converter 保留所有原始图片**

先写回归测试：同一图片在两轮历史中出现两次，转换后仍有两个 image block；当前轮图片 base64 与客户端输入一致。随后删除 history SHA256 去重/placeholder 分支，并让 `extract_kiro_image` 只校验 media type/base64 形状，不再调用环境变量驱动的 `maybe_shrink_image`。统一预算器将成为唯一重编码入口，避免二次 JPEG 压缩。

- [ ] **Step 4: 在 `image_resize.rs` 增加确定性重编码入口**

```rust
pub struct ResizeTarget {
    pub max_long_side: u32,
    pub jpeg_quality: u8,
}

pub struct ResizedImage {
    pub format: String,
    pub base64_data: String,
}

pub fn shrink_image_with_target(
    base64_data: &str,
    media_format: &str,
    target: ResizeTarget,
) -> Result<ResizedImage, ImageResizeError>;
```

从原始 base64 解码一次、按最大边缩放一次、JPEG 编码一次，并把 format 更新为 `jpeg`；GIF 和无法识别格式返回 typed `Unsupported`，不得静默删除或改成空字符串。实际 base64 长度使用编码后 `String::len()`，不能用 `raw_len * 4 / 3` 估算。

- [ ] **Step 5: 实现策略、统计和遍历**

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ImageBudgetPolicy {
    pub enabled: bool,
    pub total_base64_budget_bytes: usize,
    pub history_max_dimension: u32,
    pub history_jpeg_quality: u8,
    pub retry_history_max_dimension: u32,
    pub retry_history_jpeg_quality: u8,
}
```

遍历 `conversation_state.history` 中所有 `Message::User(...).images`，从最旧到最新压缩；当前 `current_message.user_input_message.images` 只计数不修改。达到预算立即停止。最终仍超限返回 `ImageBudgetError::Exceeded { count, total, budget }`。

- [ ] **Step 6: 增加嵌套 tool_result 与 CC 一致性夹具**

使用 converter 真实输入构造顶层图片和 `tool_result.content[]` 图片，先转换为 `KiroRequest` 再运行预算器，断言两类图片都出现在最终统计中。相同预算器直接作用于 `/v1` 和 `/cc` 共用的 Kiro 结构。

- [ ] **Step 7: 运行图片与 converter 测试**

Run:

```powershell
cargo test --locked image_budget -- --nocapture
cargo test --locked image_resize -- --nocapture
cargo test --locked tool_result -- --nocapture
```

Expected: 全部 PASS；当前轮图片字节和图片 block 数不变。

- [ ] **Step 8: 提交 Task 4**

```powershell
git add -- src/kiro/image_budget.rs src/kiro/mod.rs src/image_resize.rs src/anthropic/converter.rs
git commit -m "feat(image): 增加出站图片总预算"
```

### Task 5: 集成图片预检和上游阈值一次降级重试

**Files:**
- Modify: `src/anthropic/handlers.rs:1551-1875`
- Modify: `src/anthropic/handlers.rs:3097-3390`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写共享出站准备函数 RED 测试**

```rust
#[test]
fn prepared_bodies_keep_current_images_and_offer_smaller_history_retry() {
    let prepared = prepare_outbound_bodies(request_with_large_history(), normal_policy()).unwrap();
    assert!(prepared.retry_body.is_some());
    assert!(prepared.retry_body.as_ref().unwrap().len() < prepared.primary_body.len());
    assert_eq!(extract_current_image(&prepared.primary_body), extract_current_image(prepared.retry_body.as_ref().unwrap()));
}
```

- [ ] **Step 2: 实现 `PreparedRequestBodies`**

```rust
struct PreparedRequestBodies {
    primary_body: String,
    threshold_retry_body: Option<String>,
    image_stats: ImageBudgetStats,
}
```

从同一个原始 `KiroRequest` clone 两份：primary 使用普通历史尺寸/质量；retry 使用 retry 尺寸/质量。两份都不能修改当前轮图片。预检无法满足 primary 预算时返回 `PrepareRequestError::ImageBudget` 并映射为 HTTP 400 `invalid_request_error`。

- [ ] **Step 3: 写阈值错误重试 RED 测试**

```rust
#[tokio::test]
async fn content_length_threshold_retries_once_with_smaller_history_body() {
    let result = call_with_content_length_retry(
        "primary", Some("smaller"),
        |body| async move {
            if body == "primary" { Err(content_length_error()) } else { Ok("accepted") }
        },
    ).await.unwrap();
    assert_eq!(result.attempts, 2);
    assert_eq!(result.value, "accepted");
}
```

另写无 retry body、第二次仍失败和非阈值 400 不重试测试。

- [ ] **Step 4: 实现统一错误识别和最多一次重试**

```rust
fn is_content_length_threshold(error: &anyhow::Error) -> bool {
    let text = error.to_string();
    text.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
        || text.contains("ContentLengthExceededException")
}
```

只在有 `threshold_retry_body` 且第一次为该错误时调用第二次；不对其他 400、认证错误、限流或客户端已收到模型输出的情况重试。

- [ ] **Step 5: 在 `/v1` 和 `/cc` 共用准备与重试逻辑**

删除只扫描入站顶层图片的行为，日志改为最终出站聚合字段：总数、历史数、当前数、压缩前后 KiB、重编码数。不得记录 request body 或 base64。

- [ ] **Step 6: 修正错误映射文案**

上游阈值耗尽后返回：

```text
Request content exceeds the upstream byte-size limit after historical image compression. Reduce images in the current turn or start a new conversation.
```

错误类型为 `invalid_request_error`，不能再称为 token context window full。

- [ ] **Step 7: 运行 handler 图片回归**

Run: `cargo test --locked content_length -- --nocapture`

Expected: 预检错误不调用 provider；阈值错误最多两次；两条路由行为一致。

- [ ] **Step 8: 提交 Task 5**

```powershell
git add -- src/anthropic/handlers.rs
git commit -m "fix(image): 治理超长图片请求并安全重试"
```

### Task 6: 增加图片预算配置 API 和管理端

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Create: `admin-ui/src/api/image-budget.ts`
- Create: `admin-ui/src/hooks/use-image-budget.ts`
- Create: `admin-ui/src/lib/image-budget.ts`
- Create: `admin-ui/src/lib/image-budget.test.ts`
- Create: `admin-ui/src/components/image-budget-dialog.tsx`
- Modify: `admin-ui/src/components/topbar-tools.tsx`
- Modify: `admin-ui/src/types/api.ts`

- [ ] **Step 1: 写 Rust 策略校验 RED 测试**

```rust
#[test]
fn image_budget_policy_validates_ranges_and_retry_not_larger() {
    assert!(ImageBudgetPolicy::default().validate().is_ok());
    let mut invalid = ImageBudgetPolicy::default();
    invalid.retry_history_jpeg_quality = invalid.history_jpeg_quality + 1;
    assert!(invalid.validate().is_err());
}
```

- [ ] **Step 2: 在 Config 增加固定默认字段**

```rust
#[serde(default = "default_true")]
pub image_budget_enabled: bool,
#[serde(default = "default_image_total_budget")]
pub image_total_base64_budget_bytes: usize,
#[serde(default = "default_image_history_dimension")]
pub image_history_max_dimension: u32,
#[serde(default = "default_image_history_quality")]
pub image_history_jpeg_quality: u8,
#[serde(default = "default_image_retry_dimension")]
pub image_retry_history_max_dimension: u32,
#[serde(default = "default_image_retry_quality")]
pub image_retry_history_jpeg_quality: u8,
```

默认值依次为 `true, 819200, 1280, 72, 960, 60`。

- [ ] **Step 3: 在 provider 增加热更新策略**

`KiroProvider` 新增 `image_budget_policy: parking_lot::RwLock<ImageBudgetPolicy>`，构造时从 Config 初始化，并提供：

```rust
pub fn image_budget_policy(&self) -> ImageBudgetPolicy;
pub fn set_image_budget_policy(&self, policy: ImageBudgetPolicy) -> anyhow::Result<()>;
```

- [ ] **Step 4: 增加 Admin API**

注册：

```text
GET /api/admin/config/image-budget
PUT /api/admin/config/image-budget
```

PUT 先校验、再保存 `config.json`、保存成功后才更新 provider 运行时策略。允许范围严格使用设计规格；持久化失败返回 500 且运行时不变。

- [ ] **Step 5: 写前端校验 RED 测试**

```ts
test('rejects retry settings larger than primary settings', () => {
  expect(validateImageBudget({
    totalBase64BudgetBytes: 819200,
    historyMaxDimension: 1280,
    historyJpegQuality: 72,
    retryHistoryMaxDimension: 1600,
    retryHistoryJpegQuality: 80,
  })).toContain('重试')
})
```

Run: `bun test src/lib/image-budget.test.ts`

Expected: FAIL，因为模块不存在。

- [ ] **Step 6: 实现 API、hook、校验和弹窗**

弹窗展示开关、总预算 KiB、普通/重试边长和质量，并固定展示：

```text
只自动压缩历史图片，不会删除图片，也不会修改当前轮图片。
```

保存成功后 invalidate `['image-budget']`。

- [ ] **Step 7: 运行前端测试与构建**

Run:

```powershell
cd admin-ui
bun test src/lib/image-budget.test.ts
bun run build
```

Expected: 测试 PASS，TypeScript/Vite 构建成功。

- [ ] **Step 8: 运行后端配置测试**

Run: `cargo test --locked image_budget_policy -- --nocapture`

Expected: 默认、范围、持久化失败不热更新测试全部 PASS。

- [ ] **Step 9: 提交 Task 6**

```powershell
git add -- src/model/config.rs src/kiro/provider.rs src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs admin-ui/src/api/image-budget.ts admin-ui/src/hooks/use-image-budget.ts admin-ui/src/lib/image-budget.ts admin-ui/src/lib/image-budget.test.ts admin-ui/src/components/image-budget-dialog.tsx admin-ui/src/components/topbar-tools.tsx admin-ui/src/types/api.ts
git commit -m "feat(admin): 增加图片预算治理设置"
```

### Task 7: 完成可靠性回归与真实探针

**Files:**
- Modify: `src/bin/anthropic_probe.rs`
- Test: `src/anthropic/stream.rs`
- Test: `src/anthropic/handlers.rs`
- Test: `src/kiro/image_budget.rs`

- [ ] **Step 1: 为本地探针增加工具与多图场景**

增加 `tool-json-missing-stop`、`tool-json-truncated`、`multi-image-history` 三个可单独运行的 case；输出只包含 PASS/FAIL、状态码、事件类型和图片统计，不打印工具参数或 base64。

- [ ] **Step 2: 运行 Rust 目标回归**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'; $env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'
cargo test --locked tool_json -- --nocapture
cargo test --locked image_budget -- --nocapture
cargo test --locked content_length -- --nocapture
cargo test --locked tool_choice -- --nocapture
```

Expected: 全部 PASS。

- [ ] **Step 3: 运行格式和静态检查**

Run:

```powershell
cargo fmt --all -- --check
cargo check --locked
git diff --check
```

Expected: 全部退出 0。

- [ ] **Step 4: 在 8991 执行真实验证**

部署当前 commit 到测试容器后执行流式/非流式 `fs_write`、连续工具调用和 12 张历史截图请求。确认：完整 JSON 可执行、真半截不执行、恢复最多一次、图片不删除、8990 未变更。

- [ ] **Step 5: 提交探针**

```powershell
git add -- src/bin/anthropic_probe.rs
git commit -m "test(probe): 覆盖工具中断与图片预算"
```
