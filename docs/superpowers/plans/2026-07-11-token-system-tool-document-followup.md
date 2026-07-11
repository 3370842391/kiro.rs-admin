# Token、system、工具调用与文档兼容性跟进 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修正 Anthropic 兼容 API 的客户端可见 Token 口径，撤回会触发注入识别的 system JSON 信封，精简原生 reasoning 模型的 XML，改善 PDF 文本映射，并保证工具结束原因与实际内容块一致。

**Architecture:** 新增一个小型 Token 计量值对象，明确区分客户端可见输入与 Kiro 上游上下文占用；流式和非流式响应只从客户端可见值生成 Anthropic usage，而上游值继续驱动日志与上下文护栏。请求转换恢复 system 历史映射，PDF 使用 Markdown 引用式文本，流式状态机分别记录“看到上游工具信号”和“实际发出工具块”。

**Tech Stack:** Rust 2024、Axum、Serde/serde_json、Tokio、现有 Kiro EventStream、内置 Rust 单元与异步测试、Bun/Vite Admin UI。

---

## 文件结构

- Create: `src/anthropic/usage.rs` — 双轨输入 Token 值对象和缓存分摊入口。
- Modify: `src/anthropic/mod.rs` — 注册内部 usage 模块并修正文档注释。
- Modify: `src/anthropic/handlers.rs` — 非流式 usage、空响应错误及流式遥测口径。
- Modify: `src/anthropic/stream.rs` — 流式双轨 usage、工具块状态和终止错误。
- Modify: `src/anthropic/converter.rs` — system 历史映射和原生 reasoning XML 精简。
- Modify: `src/anthropic/document.rs` — PDF 引用文本格式与相关测试。

### Task 1: 建立双轨 Token 计量值对象

**Files:**
- Create: `src/anthropic/usage.rs`
- Modify: `src/anthropic/mod.rs:19-25`
- Test: `src/anthropic/usage.rs`

- [ ] **Step 1: 写出客户端可见 Token 不被上游覆盖的失败测试**

```rust
#[cfg(test)]
mod tests {
    use super::InputTokenUsage;
    use crate::anthropic::cache_metering::CacheUsage;

    #[test]
    fn api_usage_keeps_client_visible_total_when_upstream_is_larger() {
        let mut usage = InputTokenUsage::new(72);
        usage.observe_upstream_context(5_417);

        let (input, creation, read) = usage.split_api(&CacheUsage::default());
        assert_eq!((input, creation, read), (72, 0, 0));
        assert_eq!(usage.upstream_context_tokens(), Some(5_417));
    }

    #[test]
    fn cache_fields_sum_to_client_visible_total() {
        let usage = InputTokenUsage::new(100);
        let cache = CacheUsage {
            cache_read: 40,
            cache_covered_est: 60,
            prompt_total_est: 100,
            ..CacheUsage::default()
        };

        let (input, creation, read) = usage.split_api(&cache);
        assert_eq!(input + creation + read, 100);
    }

    #[test]
    fn api_usage_grows_only_with_client_visible_prompt() {
        let mut short = InputTokenUsage::new(72);
        short.observe_upstream_context(5_417);
        let mut long = InputTokenUsage::new(182);
        long.observe_upstream_context(6_340);

        assert_eq!(short.split_api(&CacheUsage::default()).0, 72);
        assert_eq!(long.split_api(&CacheUsage::default()).0, 182);
        assert_eq!(182 - 72, 110);
    }
}
```

- [ ] **Step 2: 运行测试并确认因为模块尚不存在而失败**

Run: `cargo test anthropic::usage::tests --lib`

Expected: FAIL，错误包含 `could not find usage in anthropic` 或 `InputTokenUsage` 未定义。

- [ ] **Step 3: 实现最小双轨值对象并注册模块**

在 `src/anthropic/mod.rs` 增加：

```rust
pub(crate) mod usage;
```

创建 `src/anthropic/usage.rs`：

```rust
use super::cache_metering::CacheUsage;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputTokenUsage {
    client_visible_tokens: i32,
    upstream_context_tokens: Option<i32>,
}

impl InputTokenUsage {
    pub(crate) fn new(client_visible_tokens: i32) -> Self {
        Self {
            client_visible_tokens: client_visible_tokens.max(0),
            upstream_context_tokens: None,
        }
    }

    pub(crate) fn observe_upstream_context(&mut self, tokens: i32) {
        self.upstream_context_tokens = Some(tokens.max(0));
    }

    pub(crate) fn client_visible_tokens(&self) -> i32 {
        self.client_visible_tokens
    }

    pub(crate) fn upstream_context_tokens(&self) -> Option<i32> {
        self.upstream_context_tokens
    }

    pub(crate) fn split_api(&self, cache: &CacheUsage) -> (i32, i32, i32) {
        cache.split_against_total(self.client_visible_tokens)
    }
}
```

- [ ] **Step 4: 运行双轨计量测试并确认通过**

Run: `cargo test anthropic::usage::tests --lib`

Expected: PASS，3 项测试通过。

- [ ] **Step 5: 提交双轨计量基础**

```text
git add src/anthropic/mod.rs src/anthropic/usage.rs
git commit -m "refactor(token): 分离客户端与上游计量"
```

### Task 2: 接入流式与非流式 usage

**Files:**
- Modify: `src/anthropic/handlers.rs:408-414, 1575-1799`
- Modify: `src/anthropic/stream.rs:1410-1621, 2567-2708`
- Test: `src/anthropic/handlers.rs`
- Test: `src/anthropic/stream.rs`

- [ ] **Step 1: 写出非流式与普通流式使用客户端可见总量的失败测试**

在 `src/anthropic/handlers.rs` 的测试模块增加：

```rust
#[test]
fn non_stream_usage_ignores_upstream_context_for_api_total() {
    let mut usage = crate::anthropic::usage::InputTokenUsage::new(72);
    usage.observe_upstream_context(5_417);
    let split = usage.split_api(&crate::anthropic::cache_metering::CacheUsage::default());
    assert_eq!(split, (72, 0, 0));
}
```

在 `src/anthropic/stream.rs` 的测试模块增加：

```rust
#[test]
fn context_usage_does_not_override_stream_api_usage() {
    let mut ctx = StreamContext::new_with_thinking(
        "claude-opus-4.8",
        72,
        false,
        HashMap::new(),
        HashSet::new(),
    );
    ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
        context_usage_percentage: 0.5417,
    }));

    assert_eq!(ctx.resolved_usage(), (72, 0, 0));
    assert_eq!(ctx.upstream_context_tokens(), Some(5_417));
}

#[test]
fn buffered_cc_stream_rewrites_message_start_with_client_visible_usage() {
    let mut ctx = BufferedStreamContext::new(
        "claude-opus-4.8",
        72,
        false,
        HashMap::new(),
        HashSet::new(),
    );
    ctx.process_and_buffer(&Event::ContextUsage(ContextUsageEvent {
        context_usage_percentage: 0.5417,
    }));
    let events = ctx.finish_and_get_all_events();
    let start = events.iter().find(|event| event.event == "message_start").unwrap();
    assert_eq!(start.data["message"]["usage"]["input_tokens"], 72);
}

#[test]
fn full_upstream_context_still_sets_overflow_stop_reason() {
    let mut ctx = StreamContext::new_with_thinking(
        "claude-opus-4.8",
        72,
        false,
        HashMap::new(),
        HashSet::new(),
    );
    ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent {
        context_usage_percentage: 100.0,
    }));
    assert_eq!(ctx.state_manager.get_stop_reason(), "model_context_window_exceeded");
}
```

- [ ] **Step 2: 运行聚焦测试并确认旧逻辑仍返回约 5,417**

Run: `cargo test non_stream_usage_ignores_upstream_context_for_api_total --lib`

Run: `cargo test context_usage_does_not_override_stream_api_usage --lib`

Run: `cargo test buffered_cc_stream_rewrites_message_start_with_client_visible_usage --lib`

Run: `cargo test full_upstream_context_still_sets_overflow_stop_reason --lib`

Expected: FAIL；流式断言显示旧 `resolved_usage()` 采用 `contextUsageEvent` 值。

- [ ] **Step 3: 替换非流式的模糊覆盖逻辑**

删除 `resolve_usage_input_tokens`。在非流式处理开始处创建：

```rust
let mut token_usage = crate::anthropic::usage::InputTokenUsage::new(input_tokens);
```

处理 `Event::ContextUsage` 时保留百分比达到 100% 的护栏，并改为：

```rust
let upstream_context_tokens =
    (context_usage.context_usage_percentage * get_context_window_size(model) as f64 / 100.0)
        as i32;
token_usage.observe_upstream_context(upstream_context_tokens);
tracing::debug!(
    client_visible_tokens = token_usage.client_visible_tokens(),
    upstream_context_tokens,
    context_usage_percentage = context_usage.context_usage_percentage,
    "received upstream context usage"
);
```

构造响应前改为：

```rust
let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
    token_usage.split_api(&cache_usage);
```

- [ ] **Step 4: 将 StreamContext 的两个裸字段替换为值对象**

把 `input_tokens` 与 `context_input_tokens` 替换为：

```rust
input_usage: crate::anthropic::usage::InputTokenUsage,
```

并实现：

```rust
pub fn resolved_usage(&self) -> (i32, i32, i32) {
    self.input_usage.split_api(&self.cache_usage)
}

pub fn upstream_context_tokens(&self) -> Option<i32> {
    self.input_usage.upstream_context_tokens()
}
```

`create_message_start_event` 使用 `self.input_usage.client_visible_tokens()`；`Event::ContextUsage` 只调用 `observe_upstream_context`，同时保留现有 100% 上下文结束原因。

- [ ] **Step 5: 修正 BufferedStreamContext 与模块注释**

`finish_and_get_all_events` 仍用 `inner.resolved_usage()` 回填 `message_start`，但注释改为“缓冲后回填客户端可见 usage 与缓存拆分”。`src/anthropic/mod.rs` 和 `handlers.rs` 中所有“等待 contextUsageEvent 以获得准确 input_tokens”的注释同步改为“等待上游事件完成后统一收尾，但 API usage 保持客户端可见口径”。

- [ ] **Step 6: 运行流式、非流式和缓存计量测试**

Run: `cargo test anthropic::usage --lib`

Run: `cargo test anthropic::stream --lib`

Run: `cargo test anthropic::cache_metering --lib`

Run: `cargo test anthropic::handlers --lib`

Expected: PASS；usage 总和等于客户端可见总量，上游值仍可从 `upstream_context_tokens()` 读取。

- [ ] **Step 7: 提交 usage 接入**

```text
git add src/anthropic/mod.rs src/anthropic/handlers.rs src/anthropic/stream.rs
git commit -m "fix(token): 统一客户端可见用量口径"
```

### Task 3: 回退 system JSON 信封并保护多轮顺序

**Files:**
- Modify: `src/anthropic/converter.rs:680-790, 1638-1723`
- Test: `src/anthropic/converter.rs:3028-3214`

- [ ] **Step 1: 将 JSON 信封测试改成历史映射失败测试**

```rust
#[test]
fn system_blocks_are_preserved_as_leading_history_without_json_envelope() {
    use super::super::types::SystemMessage;

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.system = Some(vec![
        SystemMessage { text: "first rule".into(), cache_control: None },
        SystemMessage { text: "second rule".into(), cache_control: None },
    ]);
    req.messages[0].content = serde_json::json!("user text");

    let result = convert_request(&req).unwrap();
    let Message::User(system) = &result.conversation_state.history[0] else {
        panic!("system must be represented as leading user history");
    };
    assert_eq!(system.user_input_message.content, "first rule\nsecond rule");
    assert_eq!(
        result.conversation_state.current_message.user_input_message.content,
        "user text"
    );
    let wire = serde_json::to_string(&result.conversation_state).unwrap();
    assert!(!wire.contains("client_system_instructions"));
    assert!(!wire.contains("user_content"));
}
```

保留并扩充现有 `test_multiturn_history_contains_no_synthetic_ok`，额外断言原 user/assistant 内容顺序和 `tool_use`/`tool_result` 数量不变。

- [ ] **Step 2: 运行 system 与多轮测试并确认 JSON 信封断言失败**

Run: `cargo test system_blocks_are_preserved_as_leading_history_without_json_envelope --lib`

Run: `cargo test test_multiturn_history_contains_no_synthetic_ok --lib`

Expected: FAIL；当前 system 仍位于 current message JSON 中。

- [ ] **Step 3: 恢复 system 的原始历史表示**

增加一个只负责 system 历史表示的帮助函数：

```rust
fn push_system_history(
    history: &mut Vec<Message>,
    req: &MessagesRequest,
    model_id: &str,
) {
    if let Some(system) = &req.system {
        let content = system
            .iter()
            .map(|block| block.text.as_str())
            .filter(|text| !text.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        if !content.is_empty() {
            history.push(Message::User(HistoryUserMessage::new(content, model_id)));
        }
    }
}
```

让 `build_history` 重新接收 `req`，并在初始化 `history` 后立即调用：

```rust
fn build_history(
    req: &MessagesRequest,
    messages: &[super::types::Message],
    model_id: &str,
    tool_name_map: &mut HashMap<String, String>,
    mode: ToolCompatibilityMode,
) -> Result<Vec<Message>, ConversionError> {
    let mut history = Vec::new();
    push_system_history(&mut history, req, model_id);
    // 从 `let mut user_buffer` 开始的现有消息合并代码保持逐行不变。
```

调用点改为 `build_history(req, history_messages, ...)`。删除 `build_current_message_content` 的 JSON 序列化，当前消息直接使用 `text_content`：

```rust
let content = text_content;
```

- [ ] **Step 4: 运行 converter 全部测试并修正旧信封断言**

Run: `cargo test anthropic::converter::tests --lib`

Expected: PASS；不再有测试尝试解析 `client_system_instructions` JSON，连续 user 合并、工具配对和独立会话 ID 测试保持通过。

- [ ] **Step 5: 提交 system 回退**

```text
git add src/anthropic/converter.rs
git commit -m "fix(system): 回退注入式消息信封"
```

### Task 4: 对原生 reasoning 模型移除 thinking XML

**Files:**
- Modify: `src/anthropic/converter.rs:376-578, 1610-1667`
- Test: `src/anthropic/converter.rs:2394-2485, 3132-3164`

- [ ] **Step 1: 写出 Opus 4.8 不包含 XML 且保留原生字段的失败测试**

```rust
#[test]
fn opus_4_8_uses_native_reasoning_without_thinking_xml() {
    use super::super::types::{SystemMessage, Thinking};

    let mut req = minimal_request_with_output_config("claude-opus-4.8");
    req.thinking = Some(Thinking {
        thinking_type: "enabled".into(),
        budget_tokens: 20_000,
    });
    req.system = Some(vec![SystemMessage {
        text: "Keep the answer exact.".into(),
        cache_control: None,
    }]);

    let result = convert_request(&req).unwrap();
    let wire = serde_json::to_string(&result.conversation_state).unwrap();
    assert!(!wire.contains("<thinking_mode>"));
    assert!(!wire.contains("<max_thinking_length>"));
    assert_eq!(
        result.additional_model_request_fields
            .unwrap().output_config.unwrap().effort,
        "high"
    );
}

#[test]
fn legacy_model_keeps_text_reasoning_fallback() {
    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.thinking = Some(Thinking {
        thinking_type: "enabled".into(),
        budget_tokens: 2_048,
    });
    let result = convert_request(&req).unwrap();
    assert!(serde_json::to_string(&result.conversation_state)
        .unwrap()
        .contains("<thinking_mode>enabled</thinking_mode>"));
}
```

- [ ] **Step 2: 运行 reasoning 测试并确认 Opus 4.8 仍注入 XML**

Run: `cargo test opus_4_8_uses_native_reasoning_without_thinking_xml --lib`

Run: `cargo test legacy_model_keeps_text_reasoning_fallback --lib`

Expected: FAIL；Opus 4.8 wire 中仍出现 `<thinking_mode>`。

- [ ] **Step 3: 仅对非原生模型生成文本回退**

增加：

```rust
fn thinking_prefix_for_history(req: &MessagesRequest, model_id: &str) -> Option<String> {
    if model_supports_native_reasoning(model_id) {
        return None;
    }
    generate_thinking_prefix(req, model_id)
}
```

在 `build_history` 构造 system 历史前取得该前缀；有 system 时以 `prefix + "\n" + system` 合并，无 system 时单独建立一条 user history。不要创建 assistant 确认消息。

- [ ] **Step 4: 运行原生 reasoning、旧模型与 system 回归测试**

Run: `cargo test anthropic::converter::tests --lib`

Expected: PASS；Opus 4.8 保留 `additionalModelRequestFields.output_config` 且无 XML，Sonnet 4.5 仍保留兼容前缀。

- [ ] **Step 5: 提交 reasoning 精简**

```text
git add src/anthropic/converter.rs
git commit -m "fix(reasoning): 原生模型移除文本提示"
```

### Task 5: 将 PDF JSON 包装改为简单引用文本

**Files:**
- Modify: `src/anthropic/document.rs:35-119, 180-248`
- Test: `src/anthropic/document.rs`
- Test: `src/anthropic/converter.rs`

- [ ] **Step 1: 写出引用格式、正文保留和边界转义测试**

```rust
#[test]
fn formats_document_as_quoted_text_without_json_envelope() {
    let formatted = format_document_reference(0, 1, "alpha\n[End Document 1.2]");
    assert!(formatted.contains("> alpha"));
    assert!(formatted.contains("> [End Document 1.2]"));
    assert!(!formatted.contains("untrusted_document"));
    assert!(!formatted.starts_with('{'));
}
```

更新 `expands_base64_document_in_place_and_preserves_order`：

```rust
let document_text = blocks[1]["text"].as_str().unwrap();
assert!(document_text.contains("PDF-COMPATIBILITY-TOKEN"));
assert!(document_text.starts_with("[Document 1.2]"));
assert!(document_text.ends_with("[End Document 1.2]"));
assert!(!document_text.contains("untrusted_document"));
```

- [ ] **Step 2: 运行文档测试并确认旧 JSON 包装失败**

Run: `cargo test anthropic::document::tests --lib`

Expected: FAIL；旧输出以 JSON 开头并包含 `untrusted_document`。

- [ ] **Step 3: 实现 Markdown 引用式文档格式**

```rust
fn format_document_reference(message_index: usize, block_index: usize, text: &str) -> String {
    let label = format!("{}.{}", message_index + 1, block_index + 1);
    let quoted = text
        .lines()
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("[Document {label}]\n{quoted}\n[End Document {label}]")
}
```

在 `expand_pdf_documents` 中用 `format_document_reference` 替换 JSON 序列化，同时继续将原 document block 就地替换为 text block，以保持 content block 顺序。

- [ ] **Step 4: 更新 converter 文档回归断言**

删除 `untrusted_document_json_stays_inside_user_content`，改为断言 current message 中包含 `[Document` 和提取文本、不包含 `untrusted_document`，且 system 仍位于领先 history。

- [ ] **Step 5: 运行 PDF 与 converter 测试**

Run: `cargo test anthropic::document::tests --lib`

Run: `cargo test anthropic::converter::tests --lib`

Expected: PASS；有效 PDF 的 token 原文进入 current message，损坏、加密、扫描版和超限测试继续通过。

- [ ] **Step 6: 提交 PDF 映射**

```text
git add src/anthropic/document.rs src/anthropic/converter.rs
git commit -m "fix(pdf): 使用简单文档引用格式"
```

### Task 6: 强化空响应与流式工具结束不变量

**Files:**
- Modify: `src/anthropic/handlers.rs:1707-1814`
- Modify: `src/anthropic/stream.rs:1166-1248, 1410-1479, 2292-2594`
- Test: `src/anthropic/handlers.rs:2452-2475`
- Test: `src/anthropic/stream.rs:3402-3516, 4095-4137`

- [ ] **Step 1: 写出“看到信号但未发工具块”与空响应的失败测试**

```rust
#[test]
fn buffered_tool_signal_without_emitted_block_ends_with_error() {
    let mut ctx = StreamContext::new_with_thinking(
        "claude-opus-4.8",
        10,
        false,
        HashMap::new(),
        test_known_tools(),
    );
    let _ = ctx.generate_initial_events();
    let _ = ctx.process_tool_use(&ToolUseEvent {
        name: "test_tool".into(),
        tool_use_id: "tool_1".into(),
        input: "{\"half\":".into(),
        stop: false,
    });
    let events = ctx.generate_final_events();
    assert!(events.iter().any(|event| event.event == "error"));
    assert!(!events.iter().any(|event| {
        event.event == "message_delta"
            && event.data["delta"]["stop_reason"] == "tool_use"
    }));
}

#[test]
fn empty_upstream_content_is_not_a_successful_non_stream_response() {
    let content: Vec<serde_json::Value> = Vec::new();
    assert!(validate_non_stream_content(&content).is_err());
}
```

- [ ] **Step 2: 运行工具与空响应测试并确认失败**

Run: `cargo test buffered_tool_signal_without_emitted_block_ends_with_error --lib`

Run: `cargo test empty_upstream_content_is_not_a_successful_non_stream_response --lib`

Expected: FAIL；状态机仍可能从“见过工具事件”推导 `tool_use`，且非流式没有空内容验证函数。

- [ ] **Step 3: 分离上游工具信号与实际工具块状态**

在 `StreamContext` 增加：

```rust
saw_upstream_tool_use: bool,
terminal_protocol_error: Option<String>,
```

`process_tool_use` 只设置 `saw_upstream_tool_use = true`；`emit_completed_tool_use` 成功发出 `content_block_start` 时才调用 `state_manager.set_has_tool_use(true)`。纯 thinking 判断使用 `!self.saw_upstream_tool_use`，而 `SseStateManager::get_stop_reason` 只根据实际发出块的 `has_tool_use` 返回 `tool_use`。

- [ ] **Step 4: 在流结束前生成协议错误而不是矛盾的成功事件**

工具累积器收尾后，如果 `saw_upstream_tool_use` 为 true、状态机没有实际工具块，且没有更具体的 JSON 错误，设置：

```rust
self.terminal_protocol_error = Some(
    "upstream ended with tool_use but produced no valid tool_use content block".to_string(),
);
```

若没有 text、thinking 或 tool block，则设置：

```rust
self.terminal_protocol_error = Some("upstream returned no assistant content".to_string());
```

由状态管理器提供只读判断，避免从结束原因反推内容：

```rust
pub fn has_emitted_content_blocks(&self) -> bool {
    !self.active_blocks.is_empty()
}
```

存在终止错误时发送 Anthropic SSE `error` 并停止生成 `message_delta` / `message_stop` 成功结尾。将错误读取入口统一为：

```rust
pub fn terminal_error_message(&self) -> Option<String> {
    self.terminal_protocol_error
        .clone()
        .or_else(|| self.tool_json_error.as_ref().map(|error| error.message()))
}
```

`BufferedStreamContext::terminal_error_message` 直接转发该方法；`handlers.rs` 中普通和缓冲流的旧 `tool_json_error_message()` 调用全部替换为 `terminal_error_message()`，并把请求记录为 error。

- [ ] **Step 5: 为非流式响应增加空内容验证**

```rust
fn validate_non_stream_content(content: &[serde_json::Value]) -> Result<(), &'static str> {
    if content.is_empty() {
        Err("upstream returned no assistant content")
    } else {
        Ok(())
    }
}
```

在 `normalize_non_stream_content_blocks` 之后、Token 和成功响应构造之前调用；失败时返回 HTTP 502 和 `upstream_empty_response`。

- [ ] **Step 6: 运行完整工具与 stream 测试**

Run: `cargo test anthropic::stream::tests --lib`

Run: `cargo test anthropic::handlers::tests --lib`

Expected: PASS；只有实际输出工具块时才出现 `stop_reason=tool_use`，普通叙述不被转换，严格 `<invoke>` 恢复继续通过，空内容不再形成 200 成功响应。

- [ ] **Step 7: 提交终止不变量修复**

```text
git add src/anthropic/handlers.rs src/anthropic/stream.rs
git commit -m "fix(tool): 对齐工具块与结束原因"
```

### Task 7: 全量回归与交付检查

**Files:**
- Verify: `src/anthropic/usage.rs`
- Verify: `src/anthropic/handlers.rs`
- Verify: `src/anthropic/stream.rs`
- Verify: `src/anthropic/converter.rs`
- Verify: `src/anthropic/document.rs`
- Verify: `admin-ui/`

- [ ] **Step 1: 检查禁止字符串只存在于测试/历史说明，不进入生产 wire 构造**

Run: `rg -n 'client_system_instructions|untrusted_document|<thinking_mode>|<max_thinking_length>' src/anthropic`

Expected: `client_system_instructions`、`untrusted_document` 只出现在负向测试断言；thinking XML 只出现在旧模型兼容生成函数和对应测试。

- [ ] **Step 2: 运行格式检查**

Run: `cargo fmt --check`

Expected: PASS。若出现仅由本轮文件造成的格式差异，运行 `cargo fmt` 后重新检查，并在提交前确认格式化没有改动无关文件。

- [ ] **Step 3: 运行完整 Rust 测试**

Run: `cargo test`

Expected: PASS；至少保持当前 592 项基线测试并包含本计划新增回归。

- [ ] **Step 4: 运行严格 Clippy 并区分历史基线**

Run: `cargo clippy --all-targets --all-features -- -D warnings`

Expected: 本轮修改文件不产生新 lint。若仍出现当前约 174 项历史 lint，保存错误摘要并使用 `git diff 49ec080 --name-only` 证明未修改无关 lint 文件；不能为清零历史基线做大面积重写。

- [ ] **Step 5: 构建 Admin UI**

Run: `Set-Location admin-ui; bun run build; Set-Location ..`

Expected: PASS，生成生产构建。

- [ ] **Step 6: 检查补丁与工作区**

Run: `git diff --check; git status --short; git log --oneline -8`

Expected: `git diff --check` 无输出；只有明确说明的历史用户文件可保持未提交，本轮源代码和测试均已包含在本地提交中。

- [ ] **Step 7: 记录交付结论**

交付说明必须分别报告：API usage 已改为客户端可见口径；Kiro `contextUsageEvent` 仍用于日志和 100% 上下文护栏；没有声称 Kiro 实际 credits 或隐藏 foundational prompt 已减少；未执行 `git push`。
