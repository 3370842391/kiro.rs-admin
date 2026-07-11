# Local Anthropic Conformance Probes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立本地 Anthropic 兼容性探针，并修复 thinking 缺失、`tool_choice` 未生效、PDF 全链路验证不足和账号模型可用性未校验的问题。

**Architecture:** 请求转换层产生显式的工具选择策略和有界 thinking 兼容提示；流式与非流式响应层共同验证 thinking/tool 不变量。Kiro Provider 在凭据选定后使用带 TTL 的账号级模型列表缓存验证目标模型；独立 Rust 探针二进制从客户端视角执行 Canary、thinking、tool、PDF 和 SSE 检查。

**Tech Stack:** Rust 2024、Axum、Tokio、Reqwest、Serde/Serde JSON、AWS EventStream、现有 `pdf-extract`、Cargo tests。

---

## 文件结构

- Modify: `src/anthropic/types.rs` — 将松散 JSON `tool_choice` 改成带类型的 Anthropic 请求模型。
- Modify: `src/anthropic/converter.rs` — 解析工具选择策略、过滤/标记工具，并恢复有界 thinking 文本回退。
- Modify: `src/anthropic/handlers.rs` — 把策略传入响应处理器，校验非流式 thinking/tool 输出，抽取 PDF 预处理入口。
- Modify: `src/anthropic/stream.rs` — 在 SSE 收尾阶段校验 thinking/tool 策略并产生协议错误事件。
- Create: `src/kiro/model_capabilities.rs` — 账号级模型可用性缓存和查询结果类型。
- Modify: `src/kiro/mod.rs` — 注册模型能力模块。
- Modify: `src/kiro/provider.rs` — 在凭据故障转移循环中执行模型可用性预检。
- Create: `src/bin/anthropic_probe.rs` — 可直接针对本地服务运行的黑盒探针 CLI。
- Modify: `README.md` — 增加本地探针运行方法和结果边界说明。

## Task 1：类型化 `tool_choice` 并生成转换策略

**Files:**
- Modify: `src/anthropic/types.rs:114-137`
- Modify: `src/anthropic/converter.rs:580-809`
- Test: `src/anthropic/types.rs`
- Test: `src/anthropic/converter.rs`

- [ ] **Step 1: 编写 `tool_choice` 反序列化失败测试**

在 `src/anthropic/types.rs` 的测试模块加入：

```rust
#[test]
fn tool_choice_deserializes_all_supported_variants() {
    let auto: ToolChoice = serde_json::from_value(serde_json::json!({"type": "auto"})).unwrap();
    let any: ToolChoice = serde_json::from_value(serde_json::json!({"type": "any"})).unwrap();
    let tool: ToolChoice = serde_json::from_value(
        serde_json::json!({"type": "tool", "name": "lookup_weather"}),
    )
    .unwrap();
    let none: ToolChoice = serde_json::from_value(serde_json::json!({"type": "none"})).unwrap();

    assert!(matches!(auto, ToolChoice::Auto));
    assert!(matches!(any, ToolChoice::Any));
    assert!(matches!(tool, ToolChoice::Tool { name } if name == "lookup_weather"));
    assert!(matches!(none, ToolChoice::None));
}

#[test]
fn tool_choice_rejects_tool_without_name() {
    let error = serde_json::from_value::<ToolChoice>(serde_json::json!({"type": "tool"}))
        .unwrap_err();
    assert!(error.to_string().contains("name"));
}
```

- [ ] **Step 2: 运行测试并确认因类型不存在而失败**

Run: `cargo test anthropic::types::tests::tool_choice_ -- --nocapture`

Expected: FAIL，错误包含 `cannot find type ToolChoice`。

- [ ] **Step 3: 实现请求类型**

在 `src/anthropic/types.rs` 中加入：

```rust
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolChoice {
    Auto,
    Any,
    Tool { name: String },
    None,
}
```

并把 `MessagesRequest` 字段改成：

```rust
pub tool_choice: Option<ToolChoice>,
```

- [ ] **Step 4: 运行类型测试并确认通过**

Run: `cargo test anthropic::types::tests::tool_choice_ -- --nocapture`

Expected: 2 tests PASS。

- [ ] **Step 5: 编写转换策略失败测试**

在 `src/anthropic/converter.rs` 测试模块加入：

```rust
#[test]
fn required_specific_tool_filters_upstream_tools_and_keeps_client_name() {
    use super::super::types::ToolChoice;

    let mut req = minimal_request_with_output_config("claude-opus-4-8");
    req.output_config = None;
    req.tools = Some(vec![
        test_tool("lookup_weather"),
        test_tool("read_calendar"),
    ]);
    req.tool_choice = Some(ToolChoice::Tool {
        name: "lookup_weather".into(),
    });

    let converted = convert_request(&req).unwrap();
    assert_eq!(
        converted.tool_choice_policy,
        ToolChoicePolicy::RequiredSpecific("lookup_weather".into())
    );
    let tools = &converted
        .conversation_state
        .current_message
        .user_input_message
        .user_input_message_context
        .tools;
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool_specification.name, "lookup_weather");
    assert!(tools[0]
        .tool_specification
        .description
        .contains("must call this tool"));
}

#[test]
fn required_specific_tool_rejects_unknown_name() {
    use super::super::types::ToolChoice;

    let mut req = minimal_request_with_output_config("claude-opus-4-8");
    req.output_config = None;
    req.tools = Some(vec![test_tool("lookup_weather")]);
    req.tool_choice = Some(ToolChoice::Tool {
        name: "missing_tool".into(),
    });

    assert!(matches!(
        convert_request(&req),
        Err(ConversionError::InvalidToolChoice(message)) if message.contains("missing_tool")
    ));
}
```

如果当前测试模块没有 `test_tool`，加入：

```rust
fn test_tool(name: &str) -> super::super::types::Tool {
    super::super::types::Tool {
        name: name.to_string(),
        description: "test tool".to_string(),
        input_schema: std::collections::BTreeMap::from([
            ("type".to_string(), serde_json::json!("object")),
            ("properties".to_string(), serde_json::json!({})),
        ]),
        tool_type: None,
        max_uses: None,
        cache_control: None,
    }
}
```

- [ ] **Step 6: 运行转换测试并确认因策略字段不存在而失败**

Run: `cargo test anthropic::converter::tests::required_specific_tool_ -- --nocapture`

Expected: FAIL，错误指向 `ToolChoicePolicy` 或 `tool_choice_policy` 不存在。

- [ ] **Step 7: 实现工具选择策略和转换校验**

在 `src/anthropic/converter.rs` 加入：

```rust
const REQUIRED_TOOL_DESCRIPTION_SUFFIX: &str =
    " Required by the client: you must call this tool before answering.";
const REQUIRED_ANY_TOOL_DESCRIPTION_SUFFIX: &str =
    " Required by the client: call at least one provided tool before answering.";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ToolChoicePolicy {
    Auto,
    RequiredAny,
    RequiredSpecific(String),
    Disabled,
}

fn resolve_tool_choice_policy(
    choice: Option<&super::types::ToolChoice>,
    client_tools: &[super::types::Tool],
) -> Result<ToolChoicePolicy, ConversionError> {
    use super::types::ToolChoice;

    match choice {
        None | Some(ToolChoice::Auto) => Ok(ToolChoicePolicy::Auto),
        Some(ToolChoice::Any) if client_tools.is_empty() => Err(
            ConversionError::InvalidToolChoice("tool_choice any requires at least one tool".into()),
        ),
        Some(ToolChoice::Any) => Ok(ToolChoicePolicy::RequiredAny),
        Some(ToolChoice::None) => Ok(ToolChoicePolicy::Disabled),
        Some(ToolChoice::Tool { name }) => {
            if client_tools.iter().any(|tool| tool.name == *name) {
                Ok(ToolChoicePolicy::RequiredSpecific(name.clone()))
            } else {
                Err(ConversionError::InvalidToolChoice(format!(
                    "tool_choice references undeclared tool: {name}"
                )))
            }
        }
    }
}
```

扩展 `ConversionError`：

```rust
InvalidToolChoice(String),
```

并在 `Display` 中加入：

```rust
ConversionError::InvalidToolChoice(reason) => write!(f, "工具选择无效: {reason}"),
```

扩展 `ConversionResult`：

```rust
pub tool_choice_policy: ToolChoicePolicy,
```

在 `convert_request_with_mode` 转换工具后执行：

```rust
let client_tools = req.tools.as_deref().unwrap_or(&[]);
let tool_choice_policy = resolve_tool_choice_policy(req.tool_choice.as_ref(), client_tools)?;

match &tool_choice_policy {
    ToolChoicePolicy::RequiredSpecific(client_name) => {
        tools.retain(|tool| {
            let upstream = &tool.tool_specification.name;
            upstream == client_name
                || tool_name_map.get(upstream).is_some_and(|original| original == client_name)
        });
        for tool in &mut tools {
            tool.tool_specification
                .description
                .push_str(REQUIRED_TOOL_DESCRIPTION_SUFFIX);
        }
    }
    ToolChoicePolicy::RequiredAny => {
        for tool in &mut tools {
            tool.tool_specification
                .description
                .push_str(REQUIRED_ANY_TOOL_DESCRIPTION_SUFFIX);
        }
    }
    ToolChoicePolicy::Disabled => tools.clear(),
    ToolChoicePolicy::Auto => {}
}
```

最后把 `tool_choice_policy` 写入 `ConversionResult`。

- [ ] **Step 8: 更新 handler 对新转换错误的映射**

在 `src/anthropic/handlers.rs` 两个 `ConversionError` match 中加入：

```rust
ConversionError::InvalidToolChoice(reason) => (
    "invalid_request_error",
    format!("工具选择无效: {reason}"),
),
```

- [ ] **Step 9: 运行相关测试并确认通过**

Run: `cargo test tool_choice -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 10: 创建本地提交**

```powershell
git add -- src/anthropic/types.rs src/anthropic/converter.rs src/anthropic/handlers.rs
git commit -m "fix(tool): 实现 Anthropic 工具选择策略"
```

## Task 2：在流式和非流式响应中强制工具选择不变量

**Files:**
- Modify: `src/anthropic/handlers.rs:1515-1833`
- Modify: `src/anthropic/stream.rs:1410-2662`
- Test: `src/anthropic/handlers.rs`
- Test: `src/anthropic/stream.rs`

- [ ] **Step 1: 编写非流式策略验证失败测试**

在 `src/anthropic/handlers.rs` 测试模块加入：

```rust
#[test]
fn required_any_rejects_non_stream_text_only_content() {
    let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
    assert_eq!(
        validate_tool_choice_content(&ToolChoicePolicy::RequiredAny, &content),
        Err("client required a tool call but upstream produced none")
    );
}

#[test]
fn required_specific_rejects_different_tool() {
    let content = vec![serde_json::json!({
        "type": "tool_use",
        "id": "toolu_1",
        "name": "read_calendar",
        "input": {}
    })];
    assert!(validate_tool_choice_content(
        &ToolChoicePolicy::RequiredSpecific("lookup_weather".into()),
        &content,
    )
    .unwrap_err()
    .contains("lookup_weather"));
}
```

- [ ] **Step 2: 运行测试并确认失败**

Run: `cargo test anthropic::handlers::tests::required_ -- --nocapture`

Expected: FAIL，`validate_tool_choice_content` 不存在。

- [ ] **Step 3: 实现非流式验证器**

在 `src/anthropic/handlers.rs` 加入：

```rust
use super::converter::{
    ConversionError, ToolChoicePolicy, convert_request_with_mode,
};
```

```rust
fn validate_tool_choice_content(
    policy: &super::converter::ToolChoicePolicy,
    content: &[serde_json::Value],
) -> Result<(), String> {
    use super::converter::ToolChoicePolicy;

    let tool_names: Vec<&str> = content
        .iter()
        .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("name").and_then(serde_json::Value::as_str))
        .collect();

    match policy {
        ToolChoicePolicy::Auto => Ok(()),
        ToolChoicePolicy::Disabled if tool_names.is_empty() => Ok(()),
        ToolChoicePolicy::Disabled => Err("client disabled tool calls but upstream produced one".into()),
        ToolChoicePolicy::RequiredAny if tool_names.is_empty() => {
            Err("client required a tool call but upstream produced none".into())
        }
        ToolChoicePolicy::RequiredAny => Ok(()),
        ToolChoicePolicy::RequiredSpecific(expected)
            if tool_names.iter().any(|name| *name == expected) => Ok(()),
        ToolChoicePolicy::RequiredSpecific(expected) => Err(format!(
            "client required tool {expected} but upstream did not produce it"
        )),
    }
}
```

给 `handle_non_stream_request` 增加参数：

```rust
tool_choice_policy: super::converter::ToolChoicePolicy,
```

在 `validate_non_stream_content` 前调用验证器；失败时返回 HTTP 502 和：

```rust
Json(ErrorResponse::new("upstream_tool_choice_error", message))
```

- [ ] **Step 4: 编写流式策略失败测试**

在 `src/anthropic/stream.rs` 测试模块加入：

```rust
#[test]
fn required_any_stream_ends_with_error_without_tool_use() {
    let mut ctx = StreamContext::new_with_constraints(
        "claude-opus-4.8",
        10,
        false,
        HashMap::new(),
        HashSet::new(),
        ToolChoicePolicy::RequiredAny,
    );
    let mut events = ctx.generate_initial_events();
    events.extend(ctx.process_assistant_response("plain text"));
    events.extend(ctx.generate_final_events());

    assert!(events.iter().any(|event| {
        event.event == "error"
            && event.data["error"]["type"] == "upstream_tool_choice_error"
    }));
    assert!(!events.iter().any(|event| event.event == "message_stop"));
}
```

- [ ] **Step 5: 运行测试并确认失败**

Run: `cargo test anthropic::stream::tests::required_any_stream_ -- --nocapture`

Expected: FAIL，`new_with_constraints` 不存在。

- [ ] **Step 6: 实现流式策略字段与收尾验证**

在 `src/anthropic/stream.rs` 顶部加入：

```rust
use super::converter::ToolChoicePolicy;
```

在 `StreamContext` 加入：

```rust
tool_choice_policy: super::converter::ToolChoicePolicy,
emitted_tool_names: std::collections::HashSet<String>,
terminal_protocol_error_type: Option<&'static str>,
```

保留现有 `new_with_thinking` 作为默认兼容构造器，并新增：

```rust
pub fn new_with_constraints(
    model: impl Into<String>,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
) -> Self {
    Self {
        tool_choice_policy,
        emitted_tool_names: std::collections::HashSet::new(),
        // 其余字段保持 new_with_thinking 当前初始化值
        state_manager: SseStateManager::new(),
        model: model.into(),
        message_id: format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        input_usage: super::usage::InputTokenUsage::new(input_tokens),
        output_tokens: 0,
        tool_block_indices: HashMap::new(),
        tool_name_map,
        known_tool_names,
        code_fence_open: false,
        fence_scan_partial: String::new(),
        thinking_enabled,
        thinking_buffer: String::new(),
        invoke_sniff_buffer: String::new(),
        in_thinking_block: false,
        thinking_extracted: false,
        thinking_block_index: None,
        pending_thinking_signature: None,
        text_block_index: None,
        strip_thinking_leading_newline: false,
        cache_usage: super::cache_metering::CacheUsage::default(),
        credits: 0.0,
        repeat_guard_last_line: String::new(),
        repeat_guard_run: 0,
        repeat_guard_tripped: false,
        tool_json_accumulator: ToolJsonAccumulator::new(),
        tool_json_error: None,
        tool_use_xml_filter: ToolUseXmlLeakFilter::default(),
        saw_upstream_tool_use: false,
        has_visible_output: false,
        terminal_protocol_error: None,
        terminal_protocol_error_type: None,
    }
}
```

把现有构造器改为调用新构造器并传 `ToolChoicePolicy::Auto`。

在 `emit_completed_tool_use` 恢复客户端工具名后加入：

```rust
self.emitted_tool_names.insert(completed.name.clone());
```

在 `generate_final_events` 生成正常 final events 前加入：

```rust
let tool_choice_error = match &self.tool_choice_policy {
    ToolChoicePolicy::Auto => None,
    ToolChoicePolicy::Disabled if self.emitted_tool_names.is_empty() => None,
    ToolChoicePolicy::Disabled => {
        Some("client disabled tool calls but upstream produced one".to_string())
    }
    ToolChoicePolicy::RequiredAny if self.emitted_tool_names.is_empty() => {
        Some("client required a tool call but upstream produced none".to_string())
    }
    ToolChoicePolicy::RequiredAny => None,
    ToolChoicePolicy::RequiredSpecific(expected)
        if self.emitted_tool_names.contains(expected) => None,
    ToolChoicePolicy::RequiredSpecific(expected) => Some(format!(
        "client required tool {expected} but upstream did not produce it"
    )),
};
if let Some(message) = tool_choice_error {
    self.terminal_protocol_error = Some(message);
    self.terminal_protocol_error_type = Some("upstream_tool_choice_error");
}
```

把 SSE 错误类型选择改为：

```rust
let error_type = self
    .tool_json_error
    .as_ref()
    .map(ToolJsonAccumulatorError::error_type)
    .or(self.terminal_protocol_error_type)
    .unwrap_or("upstream_protocol_error");
```

- [ ] **Step 7: 让 handlers 传递转换策略**

流式和非流式调用都从 `conversion_result.tool_choice_policy.clone()` 取值。`StreamContext` 使用 `new_with_constraints`，非流式 handler 接收同一策略。

- [ ] **Step 8: 运行工具策略测试和现有 stream 测试**

Run: `cargo test required_ -- --nocapture`

Run: `cargo test test_tool -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 9: 创建本地提交**

```powershell
git add -- src/anthropic/handlers.rs src/anthropic/stream.rs
git commit -m "fix(tool): 校验强制工具调用响应"
```

## Task 3：恢复有界 thinking 回退并拒绝伪成功

**Files:**
- Modify: `src/anthropic/converter.rs:383-577,1611-1675`
- Modify: `src/anthropic/handlers.rs:1713-1766,1843-1905`
- Modify: `src/anthropic/stream.rs:1410-2662`
- Test: `src/anthropic/converter.rs`
- Test: `src/anthropic/handlers.rs`
- Test: `src/anthropic/stream.rs`

- [ ] **Step 1: 把 Opus 4.8 thinking 测试改成期望双路径回退**

将原测试 `opus_4_8_uses_native_reasoning_without_thinking_xml` 替换为：

```rust
#[test]
fn opus_4_8_keeps_native_reasoning_and_one_bounded_text_fallback() {
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
    assert_eq!(wire.matches("<thinking_mode>").count(), 1);
    assert_eq!(wire.matches("<max_thinking_length>").count(), 1);
    assert_eq!(
        result
            .additional_model_request_fields
            .unwrap()
            .output_config
            .unwrap()
            .effort,
        "high"
    );
}
```

- [ ] **Step 2: 运行测试并确认旧逻辑下失败**

Run: `cargo test anthropic::converter::tests::opus_4_8_keeps_ -- --nocapture`

Expected: FAIL，XML 出现次数为 0。

- [ ] **Step 3: 恢复请求依赖的有界回退**

把 `thinking_prefix_for_history` 改为：

```rust
fn thinking_prefix_for_history(req: &MessagesRequest, model_id: &str) -> Option<String> {
    generate_thinking_prefix(req, model_id)
}
```

保留 `build_additional_model_request_fields` 原生字段。该回退只在客户端明确发送 `thinking` 时出现一次，长度只与预算数字和 effort 枚举有关，不随用户正文增长。

- [ ] **Step 4: 编写非流式 thinking 缺失失败测试**

在 `src/anthropic/handlers.rs` 测试模块加入：

```rust
#[test]
fn enabled_thinking_rejects_plain_text_without_reasoning_block() {
    let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
    assert_eq!(
        validate_required_thinking(true, &content),
        Err("client requested thinking but upstream produced no thinking content")
    );
}

#[test]
fn redacted_thinking_satisfies_required_thinking() {
    let content = vec![serde_json::json!({
        "type": "redacted_thinking",
        "data": "encrypted"
    })];
    assert!(validate_required_thinking(true, &content).is_ok());
}
```

- [ ] **Step 5: 运行测试并确认失败**

Run: `cargo test required_thinking -- --nocapture`

Expected: FAIL，`validate_required_thinking` 不存在。

- [ ] **Step 6: 实现非流式 thinking 校验**

在 `src/anthropic/handlers.rs` 加入：

```rust
fn validate_required_thinking(
    thinking_enabled: bool,
    content: &[serde_json::Value],
) -> Result<(), &'static str> {
    if !thinking_enabled {
        return Ok(());
    }
    let has_reasoning = content.iter().any(|block| {
        matches!(
            block.get("type").and_then(serde_json::Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    });
    if has_reasoning {
        Ok(())
    } else {
        Err("client requested thinking but upstream produced no thinking content")
    }
}
```

在 content 归一化后、成功响应构造前调用。失败返回 HTTP 502：

```rust
Json(ErrorResponse::new("upstream_thinking_protocol_error", message))
```

- [ ] **Step 7: 编写流式 thinking 缺失测试**

在 `src/anthropic/stream.rs` 测试模块加入：

```rust
#[test]
fn enabled_thinking_stream_ends_with_error_when_only_plain_text_arrives() {
    let mut ctx = StreamContext::new_with_thinking(
        "claude-opus-4.8",
        10,
        true,
        HashMap::new(),
        HashSet::new(),
    );
    let mut events = ctx.generate_initial_events();
    events.extend(ctx.process_assistant_response("plain text"));
    events.extend(ctx.generate_final_events());

    assert!(events.iter().any(|event| {
        event.event == "error"
            && event.data["error"]["type"] == "upstream_thinking_protocol_error"
    }));
    assert!(!events.iter().any(|event| event.event == "message_stop"));
}
```

- [ ] **Step 8: 实现流式 reasoning 观察标记**

在 `StreamContext` 加入：

```rust
saw_reasoning_output: bool,
```

构造时设为 `false`。在原生 reasoning text、redacted reasoning 或成功开始 `<thinking>` block 时设为 `true`。在 `generate_final_events` 的协议检查区加入：

```rust
if self.thinking_enabled
    && !self.saw_reasoning_output
    && self.tool_json_error.is_none()
    && self.terminal_protocol_error.is_none()
{
    self.terminal_protocol_error = Some(
        "client requested thinking but upstream produced no thinking content".to_string(),
    );
    self.terminal_protocol_error_type = Some("upstream_thinking_protocol_error");
}
```

- [ ] **Step 9: 运行 thinking 测试**

Run: `cargo test thinking -- --nocapture`

Expected: 全部 PASS；旧模型 XML 解析测试仍通过。

- [ ] **Step 10: 创建本地提交**

```powershell
git add -- src/anthropic/converter.rs src/anthropic/handlers.rs src/anthropic/stream.rs
git commit -m "fix(thinking): 保证请求返回推理内容"
```

## Task 4：增加账号级动态模型可用性校验

**Files:**
- Create: `src/kiro/model_capabilities.rs`
- Modify: `src/kiro/mod.rs`
- Modify: `src/kiro/provider.rs:159-232,986-1138`
- Modify: `src/anthropic/handlers.rs:340-390`
- Test: `src/kiro/model_capabilities.rs`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 编写缓存隔离和过期测试**

创建 `src/kiro/model_capabilities.rs`，先写测试：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_is_isolated_by_credential() {
        let mut cache = ModelAvailabilityCache::new(Duration::from_secs(300));
        let now = Instant::now();
        cache.insert(1, ["claude-opus-4.8".to_string()], now);
        cache.insert(2, ["claude-sonnet-4.5".to_string()], now);

        assert_eq!(cache.lookup(1, "claude-opus-4.8", now), Some(true));
        assert_eq!(cache.lookup(2, "claude-opus-4.8", now), Some(false));
    }

    #[test]
    fn expired_entry_returns_unknown() {
        let mut cache = ModelAvailabilityCache::new(Duration::from_secs(60));
        let now = Instant::now();
        cache.insert(1, ["claude-opus-4.8".to_string()], now);
        assert_eq!(
            cache.lookup(1, "claude-opus-4.8", now + Duration::from_secs(61)),
            None
        );
    }
}
```

- [ ] **Step 2: 运行测试并确认因类型不存在而失败**

Run: `cargo test kiro::model_capabilities::tests -- --nocapture`

Expected: FAIL，模块或类型不存在。

- [ ] **Step 3: 实现缓存类型**

在同一文件测试前加入：

```rust
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelAvailability {
    Available,
    Missing,
    Unknown,
}

struct CachedModels {
    fetched_at: Instant,
    model_ids: HashSet<String>,
}

pub struct ModelAvailabilityCache {
    ttl: Duration,
    entries: HashMap<u64, CachedModels>,
}

impl ModelAvailabilityCache {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            entries: HashMap::new(),
        }
    }

    pub fn lookup(&self, credential_id: u64, model: &str, now: Instant) -> Option<bool> {
        let entry = self.entries.get(&credential_id)?;
        if now.duration_since(entry.fetched_at) > self.ttl {
            return None;
        }
        Some(entry.model_ids.contains(model))
    }

    pub fn insert(
        &mut self,
        credential_id: u64,
        model_ids: impl IntoIterator<Item = String>,
        now: Instant,
    ) {
        self.entries.insert(
            credential_id,
            CachedModels {
                fetched_at: now,
                model_ids: model_ids.into_iter().collect(),
            },
        );
    }
}
```

在 `src/kiro/mod.rs` 加入：

```rust
pub mod model_capabilities;
```

- [ ] **Step 4: 运行缓存测试并确认通过**

Run: `cargo test kiro::model_capabilities::tests -- --nocapture`

Expected: 2 tests PASS。

- [ ] **Step 5: 编写 provider 可用性结果测试**

在 `src/kiro/model_capabilities.rs` 测试模块增加：

```rust
#[test]
fn cached_lookup_maps_to_public_availability() {
    let mut cache = ModelAvailabilityCache::new(Duration::from_secs(300));
    let now = Instant::now();
    cache.insert(7, ["claude-opus-4.8".to_string()], now);

    assert_eq!(cache.availability(7, "claude-opus-4.8", now), ModelAvailability::Available);
    assert_eq!(cache.availability(7, "claude-sonnet-4.5", now), ModelAvailability::Missing);
    assert_eq!(cache.availability(8, "claude-opus-4.8", now), ModelAvailability::Unknown);
}
```

- [ ] **Step 6: 实现 availability 方法**

```rust
pub fn availability(
    &self,
    credential_id: u64,
    model: &str,
    now: Instant,
) -> ModelAvailability {
    match self.lookup(credential_id, model, now) {
        Some(true) => ModelAvailability::Available,
        Some(false) => ModelAvailability::Missing,
        None => ModelAvailability::Unknown,
    }
}
```

- [ ] **Step 7: 在 Provider 中接入查询和故障转移**

给 `KiroProvider` 增加：

```rust
model_availability: Mutex<crate::kiro::model_capabilities::ModelAvailabilityCache>,
```

构造时初始化：

```rust
model_availability: Mutex::new(ModelAvailabilityCache::new(Duration::from_secs(300))),
```

加入方法：

```rust
async fn model_availability_for(
    &self,
    credential_id: u64,
    model: &str,
) -> ModelAvailability {
    let now = Instant::now();
    if let Some(hit) = self.model_availability.lock().lookup(credential_id, model, now) {
        return if hit {
            ModelAvailability::Available
        } else {
            ModelAvailability::Missing
        };
    }

    match self.token_manager.get_available_models_for(credential_id).await {
        Ok(response) => {
            let ids: Vec<String> = response
                .models
                .into_iter()
                .map(|entry| entry.model_id)
                .collect();
            let available = ids.iter().any(|id| id == model);
            self.model_availability.lock().insert(credential_id, ids, now);
            if available {
                ModelAvailability::Available
            } else {
                ModelAvailability::Missing
            }
        }
        Err(error) => {
            tracing::warn!(
                credential_id,
                model,
                error = %error,
                "模型列表查询失败，按未知能力继续当前请求"
            );
            ModelAvailability::Unknown
        }
    }
}
```

在 `call_api_with_retry` 增加 `model_incompatible_ids: HashSet<u64>`。每轮获取凭据前把它与 `request_throttled_ids` 合并后传入 `acquire_context_excluding`。取得 `ctx` 后、发送 API 前执行：

```rust
let mut excluded_ids = request_throttled_ids.clone();
excluded_ids.extend(model_incompatible_ids.iter().copied());
let mut ctx = match self
    .token_manager
    .acquire_context_excluding(model.as_deref(), group, &excluded_ids)
    .await
{
    Ok(context) => context,
    Err(error) => {
        Self::emit_attempt(
            sink,
            attempt,
            0,
            "",
            None,
            outcome::UNKNOWN,
            Some(&error.to_string()),
            attempt_start,
        );
        last_error = Some(error);
        continue;
    }
};
```

模型可用性检查为：

```rust
if let Some(model) = model.as_deref() {
    if self.model_availability_for(ctx.id, model).await == ModelAvailability::Missing {
        tracing::warn!(credential_id = ctx.id, model, "当前凭据不提供目标模型，切换凭据");
        model_incompatible_ids.insert(ctx.id);
        last_error = Some(anyhow::anyhow!(
            "MODEL_NOT_AVAILABLE: credential #{} does not provide {}",
            ctx.id,
            model
        ));
        if model_incompatible_ids.len() >= total_credentials {
            anyhow::bail!(
                "MODEL_NOT_AVAILABLE: requested model is unavailable for configured credentials"
            );
        }
        continue;
    }
}
```

- [ ] **Step 8: 让 Anthropic 层把模型不可用映射成 400**

在 `classify_provider_error` 最前加入：

```rust
if text.contains("MODEL_NOT_AVAILABLE") {
    return ClassifiedProviderError {
        http_status: StatusCode::BAD_REQUEST,
        error_type: "invalid_request_error",
        public_message: "The requested model is not available for the configured upstream account.",
    };
}
```

并加入测试：

```rust
#[test]
fn unavailable_model_maps_to_anthropic_400() {
    let classified = classify_provider_error(&anyhow::anyhow!(
        "MODEL_NOT_AVAILABLE: requested model is unavailable"
    ));
    assert_eq!(classified.http_status, StatusCode::BAD_REQUEST);
    assert_eq!(classified.error_type, "invalid_request_error");
}
```

- [ ] **Step 9: 运行模型能力测试**

Run: `cargo test model_capabilities -- --nocapture`

Run: `cargo test unavailable_model_ -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 10: 创建本地提交**

```powershell
git add -- src/kiro/model_capabilities.rs src/kiro/mod.rs src/kiro/provider.rs src/anthropic/handlers.rs
git commit -m "fix(model): 按账号校验可用模型"
```

## Task 5：抽取 PDF 预处理入口并验证完整转换链路

**Files:**
- Modify: `src/anthropic/handlers.rs:692-807,1988-2049`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 编写 PDF 请求到 Kiro wire 的失败测试**

在 `src/anthropic/handlers.rs` 测试模块加入确定性的最小文本 PDF：

```rust
const PDF_CANARY_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvUmVzb3VyY2VzIDw8IC9Gb250IDw8IC9GMSA0IDAgUiA+PiA+PiAvQ29udGVudHMgNSAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iago1IDAgb2JqCjw8IC9MZW5ndGggNTQgPj4Kc3RyZWFtCkJUIC9GMSAxMiBUZiA3MiA3MjAgVGQgKFBERi1DT01QQVRJQklMSVRZLVRPS0VOKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzExIDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDE1CiUlRU9GCg==";
```

然后加入：

```rust
#[tokio::test]
async fn prepare_request_carries_pdf_canary_into_kiro_wire_in_order() {
    let mut request: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 128,
        "messages": [{
            "role": "user",
            "content": [
                {"type": "text", "text": "before"},
                {"type": "document", "source": {
                    "type": "base64",
                    "media_type": "application/pdf",
                    "data": PDF_CANARY_B64
                }},
                {"type": "text", "text": "after"}
            ]
        }]
    }))
    .unwrap();

    let converted = prepare_request(&mut request, ToolCompatibilityMode::ClaudeCode)
        .await
        .unwrap();
    let content = &converted
        .conversation_state
        .current_message
        .user_input_message
        .content;
    let before = content.find("before").unwrap();
    let canary = content.find("PDF-COMPATIBILITY-TOKEN").unwrap();
    let after = content.find("after").unwrap();
    assert!(before < canary && canary < after);
}
```

- [ ] **Step 2: 运行测试并确认 helper 不存在**

Run: `cargo test anthropic::handlers::tests::prepare_request_carries_pdf_ -- --nocapture`

Expected: FAIL，`prepare_request` 不存在。

- [ ] **Step 3: 实现共享预处理入口**

在 `src/anthropic/handlers.rs` 加入：

```rust
#[derive(Debug)]
enum PrepareRequestError {
    Document(super::document::DocumentError),
    Conversion(super::converter::ConversionError),
}

async fn prepare_request(
    payload: &mut MessagesRequest,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Result<super::converter::ConversionResult, PrepareRequestError> {
    super::document::expand_pdf_documents(payload)
        .await
        .map_err(PrepareRequestError::Document)?;
    super::converter::convert_request_with_mode(payload, mode)
        .map_err(PrepareRequestError::Conversion)
}

fn conversion_error_parts(error: &ConversionError) -> (&'static str, String) {
    match error {
        ConversionError::UnsupportedModel(model) => {
            ("invalid_request_error", format!("模型不支持: {model}"))
        }
        ConversionError::EmptyMessages => {
            ("invalid_request_error", "消息列表为空".to_string())
        }
        ConversionError::UnsupportedToolMapping(reason) => (
            "invalid_request_error",
            format!("工具映射不支持: {reason}"),
        ),
        ConversionError::InvalidToolChoice(reason) => (
            "invalid_request_error",
            format!("工具选择无效: {reason}"),
        ),
    }
}
```

标准 `/v1/messages` 和 `/cc/v1/messages` 两条路径都改成：

```rust
let conversion_result = match prepare_request(&mut payload, state.tool_compatibility_mode).await {
    Ok(result) => result,
    Err(PrepareRequestError::Document(error)) => {
        tracing::warn!(error = %error, "Anthropic document preprocessing failed");
        hook.record(0, 0, 0, 0, 0, 0.0, "error");
        return map_document_error(error);
    }
    Err(PrepareRequestError::Conversion(error)) => {
        let (error_type, message) = conversion_error_parts(&error);
        tracing::warn!(error = %error, "Anthropic request conversion failed");
        hook.record(0, 0, 0, 0, 0, 0.0, "error");
        return (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new(error_type, message)),
        )
            .into_response();
    }
};
```

- [ ] **Step 4: 增加 PDF 错误不会调用转换器的测试**

```rust
#[tokio::test]
async fn prepare_request_rejects_invalid_pdf_before_conversion() {
    let mut request: MessagesRequest = serde_json::from_value(serde_json::json!({
        "model": "claude-opus-4-8",
        "max_tokens": 128,
        "messages": [{
            "role": "user",
            "content": [{"type": "document", "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": "not-base64"
            }}]
        }]
    }))
    .unwrap();

    assert!(matches!(
        prepare_request(&mut request, ToolCompatibilityMode::ClaudeCode).await,
        Err(PrepareRequestError::Document(_))
    ));
}
```

- [ ] **Step 5: 运行 PDF 和转换回归测试**

Run: `cargo test anthropic::document::tests -- --nocapture`

Run: `cargo test prepare_request_ -- --nocapture`

Run: `cargo test document_ -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 6: 创建本地提交**

```powershell
git add -- src/anthropic/handlers.rs
git commit -m "test(pdf): 覆盖文档完整转换链路"
```

## Task 6：实现本地黑盒兼容性探针 CLI

**Files:**
- Create: `src/bin/anthropic_probe.rs`
- Test: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 编写 CLI 参数和响应判定失败测试**

创建 `src/bin/anthropic_probe.rs`，先加入：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_args_requires_base_url_and_model() {
        let args = parse_args_from([
            "anthropic_probe",
            "--base-url",
            "http://127.0.0.1:8080",
            "--model",
            "claude-opus-4-8",
        ])
        .unwrap();
        assert_eq!(args.base_url, "http://127.0.0.1:8080");
        assert_eq!(args.model, "claude-opus-4-8");
        assert_eq!(args.parallel, 16);
    }

    #[test]
    fn classify_thinking_requires_reasoning_block() {
        let response = serde_json::json!({
            "content": [{"type": "text", "text": "plain"}]
        });
        assert_eq!(
            classify_thinking(&response),
            ProbeResult::Fail("response contains no thinking block".into())
        );
    }

    #[test]
    fn classify_required_tool_checks_name() {
        let response = serde_json::json!({
            "content": [{
                "type": "tool_use",
                "name": "probe_echo",
                "input": {"value": "x"}
            }],
            "stop_reason": "tool_use"
        });
        assert_eq!(classify_required_tool(&response, "probe_echo"), ProbeResult::Pass);
    }
}
```

- [ ] **Step 2: 运行测试并确认失败**

Run: `cargo test --bin anthropic_probe -- --nocapture`

Expected: FAIL，参数和判定函数不存在。

- [ ] **Step 3: 实现参数、结果和 HTTP 基础函数**

在测试前加入：

```rust
use std::path::PathBuf;
use base64::Engine as _;
use futures::future::join_all;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug)]
struct Args {
    base_url: String,
    model: String,
    pdf: Option<PathBuf>,
    parallel: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeResult {
    Pass,
    Fail(String),
    Skip(String),
}

fn parse_args_from<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into).skip(1);
    let mut base_url = None;
    let mut model = None;
    let mut pdf = None;
    let mut parallel = 16usize;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--base-url" => base_url = iter.next(),
            "--model" => model = iter.next(),
            "--pdf" => pdf = iter.next().map(PathBuf::from),
            "--parallel" => {
                parallel = iter
                    .next()
                    .ok_or("--parallel requires a value")?
                    .parse()
                    .map_err(|_| "--parallel must be a positive integer")?;
                if parallel == 0 {
                    return Err("--parallel must be a positive integer".into());
                }
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        base_url: base_url.ok_or("--base-url is required")?,
        model: model.ok_or("--model is required")?,
        pdf,
        parallel,
    })
}

fn classify_thinking(response: &Value) -> ProbeResult {
    let has = response["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            matches!(block["type"].as_str(), Some("thinking" | "redacted_thinking"))
        })
    });
    if has {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail("response contains no thinking block".into())
    }
}

fn classify_required_tool(response: &Value, name: &str) -> ProbeResult {
    let has = response["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["type"] == "tool_use" && block["name"].as_str() == Some(name)
        })
    });
    if has && response["stop_reason"] == "tool_use" {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!("response did not call required tool {name}"))
    }
}

async fn post_message(
    client: &reqwest::Client,
    args: &Args,
    api_key: &str,
    body: Value,
) -> Result<Value, String> {
    let response = client
        .post(format!("{}/v1/messages", args.base_url.trim_end_matches('/')))
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let value: Value = response.json().await.map_err(|error| error.to_string())?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(format!("HTTP {}: {}", status.as_u16(), value))
    }
}
```

- [ ] **Step 4: 实现四类非流式探针和并发 Canary**

加入 `thinking_probe`、`tool_probe`、`pdf_probe` 和 `parallel_canary_probe`：

```rust
async fn thinking_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    match post_message(client, args, key, json!({
        "model": args.model,
        "max_tokens": 256,
        "thinking": {"type": "enabled", "budget_tokens": 1024},
        "messages": [{"role": "user", "content": "Reply with a short answer."}]
    })).await {
        Ok(value) => classify_thinking(&value),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn tool_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let name = "probe_echo";
    match post_message(client, args, key, json!({
        "model": args.model,
        "max_tokens": 256,
        "messages": [{"role": "user", "content": "Call the provided tool with value local-check."}],
        "tools": [{
            "name": name,
            "description": "Return the provided value.",
            "input_schema": {
                "type": "object",
                "properties": {"value": {"type": "string"}},
                "required": ["value"]
            }
        }],
        "tool_choice": {"type": "tool", "name": name}
    })).await {
        Ok(value) => classify_required_tool(&value, name),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn pdf_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let Some(path) = &args.pdf else {
        return ProbeResult::Skip("--pdf was not provided".into());
    };
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) => return ProbeResult::Fail(error.to_string()),
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    match post_message(client, args, key, json!({
        "model": args.model,
        "max_tokens": 256,
        "messages": [{"role": "user", "content": [
            {"type": "document", "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": encoded
            }},
            {"type": "text", "text": "Return the exact verification token printed in the document."}
        ]}]
    })).await {
        Ok(value) if value["content"].as_array().is_some_and(|blocks| !blocks.is_empty()) => {
            ProbeResult::Pass
        }
        Ok(_) => ProbeResult::Fail("PDF request returned empty content".into()),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn parallel_canary_probe(
    client: &reqwest::Client,
    args: &Args,
    key: &str,
) -> ProbeResult {
    let jobs = (0..args.parallel).map(|_| {
        let client = client.clone();
        let key = key.to_string();
        let base_url = args.base_url.clone();
        let model = args.model.clone();
        async move {
            let canary = format!("CANARY_{}", Uuid::new_v4().simple());
            let local_args = Args { base_url, model, pdf: None, parallel: 1 };
            let response = post_message(&client, &local_args, &key, json!({
                "model": local_args.model,
                "max_tokens": 64,
                "system": format!("Reply with exactly {canary} and no other text."),
                "messages": [{"role": "user", "content": "Follow the system instruction."}]
            })).await?;
            let text = response["content"].as_array()
                .into_iter().flatten()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>();
            Ok::<_, String>((canary, text))
        }
    });
    let results = join_all(jobs).await;
    for result in results {
        match result {
            Ok((canary, text)) if text.trim() == canary => {}
            Ok((canary, text)) => return ProbeResult::Fail(format!(
                "canary mismatch: expected {canary}, got {text:?}"
            )),
            Err(error) => return ProbeResult::Fail(error),
        }
    }
    ProbeResult::Pass
}
```

- [ ] **Step 5: 实现 main 并保证 Key 只从环境读取**

```rust
#[tokio::main]
async fn main() {
    let args = match parse_args_from(std::env::args()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!("ANTHROPIC_API_KEY is required");
            std::process::exit(2);
        }
    };
    let client = reqwest::Client::new();
    let results = [
        ("thinking", thinking_probe(&client, &args, &api_key).await),
        ("tool_choice", tool_probe(&client, &args, &api_key).await),
        ("pdf", pdf_probe(&client, &args, &api_key).await),
        ("parallel_canary", parallel_canary_probe(&client, &args, &api_key).await),
    ];
    let mut failed = false;
    for (name, result) in results {
        println!("{name}: {result:?}");
        failed |= matches!(result, ProbeResult::Fail(_));
    }
    if failed {
        std::process::exit(1);
    }
}
```

- [ ] **Step 6: 运行二进制单元测试**

Run: `cargo test --bin anthropic_probe -- --nocapture`

Expected: 3 tests PASS。

- [ ] **Step 7: 检查帮助错误不会输出环境 Key**

Run: `cargo run --bin anthropic_probe -- --model claude-opus-4-8`

Expected: exit 2，输出 `--base-url is required`，不包含 `ANTHROPIC_API_KEY` 的值。

- [ ] **Step 8: 创建本地提交**

```powershell
git add -- src/bin/anthropic_probe.rs
git commit -m "feat(probe): 增加本地兼容性探针"
```

## Task 7：增加 SSE 探针、使用文档和完整验证

**Files:**
- Modify: `src/bin/anthropic_probe.rs`
- Modify: `README.md`
- Test: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 编写 SSE 事件判定失败测试**

在探针测试模块加入：

```rust
#[test]
fn classify_sse_requires_ordered_terminal_events() {
    let events = vec![
        json!({"type": "message_start"}),
        json!({"type": "content_block_start", "content_block": {"type": "text"}}),
        json!({"type": "content_block_stop"}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
        json!({"type": "message_stop"}),
    ];
    assert_eq!(classify_sse(&events), ProbeResult::Pass);
    assert!(matches!(classify_sse(&events[..4]), ProbeResult::Fail(_)));
}
```

- [ ] **Step 2: 运行测试并确认失败**

Run: `cargo test --bin anthropic_probe classify_sse -- --nocapture`

Expected: FAIL，`classify_sse` 不存在。

- [ ] **Step 3: 实现 SSE 解析和顺序校验**

加入：

```rust
fn classify_sse(events: &[Value]) -> ProbeResult {
    let start = events.iter().position(|event| event["type"] == "message_start");
    let delta = events.iter().rposition(|event| event["type"] == "message_delta");
    let stop = events.iter().rposition(|event| event["type"] == "message_stop");
    match (start, delta, stop) {
        (Some(start), Some(delta), Some(stop)) if start < delta && delta < stop => ProbeResult::Pass,
        _ => ProbeResult::Fail("SSE event order is incomplete or invalid".into()),
    }
}

async fn stream_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    use futures::StreamExt;

    let response = match client
        .post(format!("{}/v1/messages", args.base_url.trim_end_matches('/')))
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": args.model,
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => return ProbeResult::Fail(format!("HTTP {}", response.status())),
        Err(error) => return ProbeResult::Fail(error.to_string()),
    };

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return ProbeResult::Fail(error.to_string()),
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer.drain(..pos + 2);
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(value) = serde_json::from_str::<Value>(data) {
                        events.push(value);
                    }
                }
            }
        }
    }
    classify_sse(&events)
}
```

把 `stream_probe` 加入 `main` 的结果列表。

- [ ] **Step 4: 运行探针测试和编译检查**

Run: `cargo test --bin anthropic_probe -- --nocapture`

Expected: 4 tests PASS。

Run: `cargo check --bin anthropic_probe`

Expected: exit 0。

- [ ] **Step 5: 在 README 增加本地运行说明**

加入以下章节：

````markdown
### 本地 Anthropic 兼容性探针

先启动本服务，再用临时客户端 Key 运行：

```powershell
$env:ANTHROPIC_API_KEY = "临时客户端Key"
cargo run --bin anthropic_probe -- `
  --base-url http://127.0.0.1:8080 `
  --model claude-opus-4-8 `
  --pdf D:\path\to\text-based.pdf `
  --parallel 16
Remove-Item Env:ANTHROPIC_API_KEY
```

探针覆盖 thinking、强制工具调用、文本型 PDF、并发 Canary 和 SSE 顺序。它验证兼容层行为，不证明服务是 Anthropic 官方直连，也不保证第三方检测平台的固定分数。扫描版 PDF 暂不支持。
````

- [ ] **Step 6: 执行新功能的聚焦验证**

Run: `cargo test --bin anthropic_probe -- --nocapture`

Expected: 全部 PASS。

Run: `cargo test anthropic:: -- --nocapture`

Run: `cargo test model_capabilities -- --nocapture`

Expected: 全部 PASS。

- [ ] **Step 7: 执行完整验证**

Run: `cargo fmt --check`

Expected: exit 0。

Run: `cargo test`

Expected: 0 failed。

Run: `cargo check --all-features`

Expected: exit 0；允许记录已有、与本次文件无关的 warning，但本次新增文件不得产生 warning。

Run: `bun run build`

Workdir: `admin-ui`

Expected: exit 0。

Run: `git diff --check`

Expected: 无输出，exit 0。

- [ ] **Step 8: 检查提交范围并创建最终本地提交**

```powershell
git status --short
git diff --stat
git add -- src/bin/anthropic_probe.rs README.md
git diff --cached --stat
git diff --cached --check
git commit -m "docs(probe): 补充本地兼容测试说明"
```

- [ ] **Step 9: 最终审计**

Run: `rg -n -i "ztest|01KX|CANARY_[0-9A-F]|I'm Kiro|You are Claude" src README.md`

Expected: 生产代码中不存在站点 ID、固定 Canary、身份伪造提示；允许 README 使用通用术语说明限制，探针只通过 UUID 运行时生成 Canary。

Run: `git status --short`

Expected: 没有本任务遗留的未提交文件；本地运行配置、凭证和数据库文件保持未跟踪或被忽略。

---

## 计划自检结果

- 设计目标中的 thinking、system/Canary、`tool_choice`、PDF、SSE 和动态模型可用性均有对应任务。
- 所有生产改动都先有明确失败测试和预期失败原因。
- `ToolChoicePolicy` 在 converter、handler 和 stream 中使用同一类型名；工具名验证统一以客户端可见名称为准。
- 动态模型查询失败采用 `Unknown` 并保守继续；明确缺失才切换凭据或返回 400。
- 计划不修改 Token 双轨计量，不识别检测站点，不伪造模型身份，不实现 OCR。
