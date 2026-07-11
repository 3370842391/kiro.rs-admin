# API Conformance and Request Isolation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate cross-request conversation reuse and preserve client-provided system/history content without synthetic assistant replies.

**Architecture:** Keep the existing Anthropic-to-Kiro converter boundary, but make upstream conversation identifiers request-scoped. Normalize the trailing run of user messages into the current Kiro message, preserve system text without the global chunking policy, and retain only client-supplied history.

**Tech Stack:** Rust, serde/serde_json, uuid, built-in Rust tests, Cargo, Bun/Vite.

---

## File Map

- Modify: `src/anthropic/converter.rs`
  - Generate request-scoped conversation identifiers.
  - Preserve system text without global policy injection.
  - Normalize trailing user messages into `currentMessage`.
  - Remove synthetic assistant history entries.
  - Host focused unit/regression tests in the existing `#[cfg(test)]` module.
- No production configuration, Admin UI source, credential, trace, or runtime data files are changed.

### Task 1: Isolate Upstream Conversation IDs

**Files:**
- Modify: `src/anthropic/converter.rs:626-659`
- Modify: `src/anthropic/converter.rs:734-742`
- Test: `src/anthropic/converter.rs:3074-3187`

- [ ] **Step 1: Replace the metadata-reuse test with a failing isolation test**

Delete the `extract_session_id` unit tests and replace `test_convert_request_with_session_metadata` with:

```rust
#[test]
fn test_same_metadata_gets_distinct_conversation_ids() {
    use super::super::types::Metadata;

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.metadata = Some(Metadata {
        user_id: Some(
            "user_account__session_a0662283-7fd3-4399-a7eb-52b9a717ae88".to_string(),
        ),
    });

    let first = convert_request(&req).unwrap();
    let second = convert_request(&req).unwrap();
    let first_id = &first.conversation_state.conversation_id;
    let second_id = &second.conversation_state.conversation_id;

    assert_ne!(first_id, second_id);
    assert_ne!(first_id, "a0662283-7fd3-4399-a7eb-52b9a717ae88");
    assert!(Uuid::parse_str(first_id).is_ok());
    assert!(Uuid::parse_str(second_id).is_ok());
}
```

- [ ] **Step 2: Run the isolation test and verify it fails**

Run:

```text
cargo test anthropic::converter::tests::test_same_metadata_gets_distinct_conversation_ids -- --exact
```

Expected: FAIL because both conversions currently reuse the UUID extracted from `metadata.user_id`.

- [ ] **Step 3: Generate a fresh conversation ID for every conversion**

Delete `extract_session_id` and `is_valid_uuid`. Replace the metadata-derived block in `convert_request_with_mode` with:

```rust
// Anthropic/OpenAI compatibility requests carry their complete history.
// Reusing metadata.user_id as upstream state makes parallel requests race.
let conversation_id = Uuid::new_v4().to_string();
let agent_continuation_id = Uuid::new_v4().to_string();
```

Keep `metadata.user_id` in the request type for logging and compatibility; only remove its use as Kiro state.

- [ ] **Step 4: Run the focused converter tests**

Run:

```text
cargo test anthropic::converter::tests::test_same_metadata_gets_distinct_conversation_ids -- --exact
cargo test anthropic::converter::tests::test_convert_request_without_metadata -- --exact
```

Expected: both tests PASS and both IDs parse as UUIDs.

- [ ] **Step 5: Commit request isolation**

```text
git add src/anthropic/converter.rs
git commit -m "fix: isolate upstream conversations per request"
```

### Task 2: Preserve System Content Without Global Injection

**Files:**
- Modify: `src/anthropic/converter.rs:218-223`
- Modify: `src/anthropic/converter.rs:1631-1716`
- Test: `src/anthropic/converter.rs` existing test module

- [ ] **Step 1: Write failing system fidelity tests**

Add:

```rust
#[test]
fn test_system_content_is_preserved_without_internal_policy() {
    use super::super::types::SystemMessage;

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.system = Some(vec![SystemMessage {
        text: "Reply with nonce-7 only.".to_string(),
        cache_control: None,
    }]);

    let result = convert_request(&req).unwrap();
    assert_eq!(result.conversation_state.history.len(), 1);

    let Message::User(system) = &result.conversation_state.history[0] else {
        panic!("system content must be represented as Kiro user history");
    };
    assert_eq!(system.user_input_message.content, "Reply with nonce-7 only.");
}

#[test]
fn test_thinking_prefix_appears_once_before_system() {
    use super::super::types::{SystemMessage, Thinking};

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.thinking = Some(Thinking {
        thinking_type: "enabled".to_string(),
        budget_tokens: 2048,
    });
    req.system = Some(vec![SystemMessage {
        text: "Keep the answer exact.".to_string(),
        cache_control: None,
    }]);

    let result = convert_request(&req).unwrap();
    let Message::User(system) = &result.conversation_state.history[0] else {
        panic!("expected system history");
    };
    let content = &system.user_input_message.content;

    assert!(content.starts_with(
        "<thinking_mode>enabled</thinking_mode><max_thinking_length>2048</max_thinking_length>\n"
    ));
    assert_eq!(content.matches("<thinking_mode>").count(), 1);
    assert!(content.ends_with("Keep the answer exact."));
}
```

- [ ] **Step 2: Run the fidelity tests and verify they fail**

Run:

```text
cargo test anthropic::converter::tests::test_system_content_is_preserved_without_internal_policy -- --exact
cargo test anthropic::converter::tests::test_thinking_prefix_appears_once_before_system -- --exact
```

Expected: FAIL because the converter appends `SYSTEM_CHUNKED_POLICY` and adds a synthetic assistant acknowledgement.

- [ ] **Step 3: Remove global policy and synthetic acknowledgement**

Delete `SYSTEM_CHUNKED_POLICY`. In `build_history`, retain the existing ordered join and thinking-prefix logic, but do not append any internal policy:

```rust
let final_content = if let Some(ref prefix) = thinking_prefix {
    if !has_thinking_tags(&system_content) {
        format!("{}\n{}", prefix, system_content)
    } else {
        system_content
    }
} else {
    system_content
};

history.push(Message::User(HistoryUserMessage::new(
    final_content,
    model_id,
)));
```

For thinking without client system, add only the thinking user history message. Remove both `HistoryAssistantMessage::new("I will follow these instructions.")` calls. Keep the existing Write/Edit/Bash tool description suffixes because they scope size limits to the relevant tools.

- [ ] **Step 4: Run focused tests**

Run:

```text
cargo test anthropic::converter::tests::test_system_content_is_preserved_without_internal_policy -- --exact
cargo test anthropic::converter::tests::test_thinking_prefix_appears_once_before_system -- --exact
```

Expected: both tests PASS; system history contains no synthetic assistant entry.

- [ ] **Step 5: Commit system fidelity**

```text
git add src/anthropic/converter.rs
git commit -m "fix: preserve client system instructions"
```

### Task 3: Merge the Trailing User Run Into Current Message

**Files:**
- Modify: `src/anthropic/converter.rs:715-829`
- Modify: `src/anthropic/converter.rs:1659-1768`
- Test: `src/anthropic/converter.rs` existing test module

- [ ] **Step 1: Write failing history normalization tests**

Add:

```rust
#[test]
fn test_consecutive_trailing_users_merge_into_current_message() {
    use super::super::types::Message as AnthropicMessage;

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.messages = vec![
        AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("first"),
        },
        AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("second"),
        },
    ];

    let result = convert_request(&req).unwrap();
    assert!(result.conversation_state.history.is_empty());
    assert_eq!(
        result
            .conversation_state
            .current_message
            .user_input_message
            .content,
        "first\nsecond"
    );
}

#[test]
fn test_multiturn_history_contains_no_synthetic_ok() {
    use super::super::types::Message as AnthropicMessage;

    let mut req = minimal_request_with_output_config("claude-sonnet-4.5");
    req.output_config = None;
    req.messages = vec![
        AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("question"),
        },
        AnthropicMessage {
            role: "assistant".to_string(),
            content: serde_json::json!("answer"),
        },
        AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("follow up A"),
        },
        AnthropicMessage {
            role: "user".to_string(),
            content: serde_json::json!("follow up B"),
        },
    ];

    let result = convert_request(&req).unwrap();
    assert_eq!(result.conversation_state.history.len(), 2);
    assert_eq!(
        result
            .conversation_state
            .current_message
            .user_input_message
            .content,
        "follow up A\nfollow up B"
    );
    assert!(!result.conversation_state.history.iter().any(|message| {
        matches!(
            message,
            Message::Assistant(assistant)
                if assistant.assistant_response_message.content == "OK"
        )
    }));
}
```

- [ ] **Step 2: Run the normalization tests and verify they fail**

Run:

```text
cargo test anthropic::converter::tests::test_consecutive_trailing_users_merge_into_current_message -- --exact
cargo test anthropic::converter::tests::test_multiturn_history_contains_no_synthetic_ok -- --exact
```

Expected: FAIL because only the final user message becomes `currentMessage` and the prior user is paired with a generated `OK`.

- [ ] **Step 3: Add a focused current-user merger**

Add beside `process_message_content`:

```rust
fn process_current_user_messages(
    messages: &[super::types::Message],
) -> Result<(String, Vec<KiroImage>, Vec<ToolResult>), ConversionError> {
    let mut content_parts = Vec::new();
    let mut images = Vec::new();
    let mut tool_results = Vec::new();

    for message in messages {
        let (text, message_images, message_tool_results) =
            process_message_content(&message.content)?;
        if !text.is_empty() {
            content_parts.push(text);
        }
        images.extend(message_images);
        tool_results.extend(message_tool_results);
    }

    Ok((content_parts.join("\n"), images, tool_results))
}
```

- [ ] **Step 4: Split history from the trailing user run**

After prefill normalization, compute the trailing run and use it for the current message:

```rust
let current_start = messages
    .iter()
    .rposition(|message| message.role == "assistant")
    .map_or(0, |index| index + 1);
let history_messages = &messages[..current_start];
let current_messages = &messages[current_start..];

let (text_content, images, tool_results) =
    process_current_user_messages(current_messages)?;
```

Pass `history_messages` into `build_history`. Change `build_history` to iterate every message it receives instead of subtracting one:

```rust
for msg in messages {
    if msg.role == "user" {
        if !assistant_buffer.is_empty() {
            let merged = merge_assistant_messages(&assistant_buffer, tool_name_map, mode)?;
            history.push(Message::Assistant(merged));
            assistant_buffer.clear();
        }
        user_buffer.push(msg);
    } else if msg.role == "assistant" {
        if !user_buffer.is_empty() {
            let merged_user = merge_user_messages(&user_buffer, model_id, &mut image_dedup)?;
            history.push(Message::User(merged_user));
            user_buffer.clear();
        }
        assistant_buffer.push(msg);
    }
}
```

Flush the buffers normally, but delete the generated `HistoryAssistantMessage::new("OK")` entry. The split guarantees normal history ends at the last assistant, while every trailing user message is represented in `currentMessage`.

- [ ] **Step 5: Run converter regression tests**

Run:

```text
cargo test anthropic::converter::tests::test_consecutive_trailing_users_merge_into_current_message -- --exact
cargo test anthropic::converter::tests::test_multiturn_history_contains_no_synthetic_ok -- --exact
cargo test anthropic::converter::tests
```

Expected: focused tests PASS and all existing converter tests PASS.

- [ ] **Step 6: Commit history normalization**

```text
git add src/anthropic/converter.rs
git commit -m "fix: normalize current user message history"
```

### Task 4: Verify the Complete Change Set

**Files:**
- Verify: `src/anthropic/converter.rs`
- Verify: repository-wide Rust and Admin UI build

- [ ] **Step 1: Format the Rust source**

Run:

```text
cargo fmt
cargo fmt --check
```

Expected: formatter exits successfully and the check reports no diff.

- [ ] **Step 2: Run the complete Rust test suite**

Run:

```text
cargo test
```

Expected: all tests PASS with zero failures.

- [ ] **Step 3: Build the Admin UI**

Run from `admin-ui`:

```text
bun run build
```

Expected: TypeScript and Vite build complete successfully. Generated `admin-ui/dist` remains ignored.

- [ ] **Step 4: Check repository hygiene**

Run:

```text
git diff --check
git status --short
```

Expected: no whitespace errors; only intentional source or plan progress changes are present.

- [ ] **Step 5: Review the final diff for detector-specific behavior**

Run:

```text
git diff HEAD~3 -- src/anthropic/converter.rs
rg -n -i "ztest|canary|nonce|probe|detector" src
```

Expected: the diff contains only general request isolation and conversion fixes; no Ztest/probe-specific production branch exists.

- [ ] **Step 6: Commit formatting only if it changed tracked files**

```text
git add src/anthropic/converter.rs
git commit -m "style: format converter isolation changes"
```

Skip this commit when `cargo fmt` produces no additional tracked diff.
