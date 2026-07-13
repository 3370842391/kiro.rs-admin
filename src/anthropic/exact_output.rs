use super::types::{MessagesRequest, ToolChoice};
use crate::model::config::ToolCompatibilityMode;

const MAX_LITERAL_BYTES: usize = 128;
const MAX_JSON_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ExactOutput {
    Text(String),
    Json(String),
}

impl ExactOutput {
    pub(crate) fn as_str(&self) -> &str {
        match self {
            Self::Text(value) | Self::Json(value) => value,
        }
    }
}

pub(crate) fn exact_system_output(
    req: &MessagesRequest,
    mode: ToolCompatibilityMode,
) -> Option<ExactOutput> {
    if !exact_system_tool_policy_is_safe(req) {
        return None;
    }

    let system = req
        .system
        .as_ref()?
        .iter()
        .filter_map(|message| super::converter::sanitize_system_for_kiro(&message.text, mode))
        .collect::<Vec<_>>()
        .join("\n");
    let normalized = system.to_lowercase();
    if !has_exact_cue(&normalized)
        || !has_no_extra_cue(&normalized)
        || has_unsafe_contract_cue(&normalized)
    {
        return None;
    }

    if normalized.contains("json") {
        let json = extract_single_json(&system)?;
        return (json.len() <= MAX_JSON_BYTES).then_some(ExactOutput::Json(json));
    }

    let candidates = quoted_ascii_literals(&system);
    match candidates.as_slice() {
        [value] => Some(ExactOutput::Text(value.clone())),
        _ => None,
    }
}

fn exact_system_tool_policy_is_safe(req: &MessagesRequest) -> bool {
    if req
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.is_enabled())
        || conversation_has_tool_blocks(req)
    {
        return false;
    }

    matches!(
        req.tool_choice.as_ref(),
        None | Some(ToolChoice::Auto { .. }) | Some(ToolChoice::None { .. })
    )
}

fn conversation_has_tool_blocks(req: &MessagesRequest) -> bool {
    req.messages.iter().any(|message| {
        message.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                matches!(
                    block.get("type").and_then(serde_json::Value::as_str),
                    Some("tool_use" | "tool_result")
                )
            })
        })
    })
}

pub(crate) fn exact_user_echo(req: &MessagesRequest) -> Option<String> {
    if req.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        || req.tool_choice.is_some()
        || req
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.is_enabled())
        || conversation_has_non_text_content(req)
    {
        return None;
    }

    let latest_user = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")?;
    let text = message_text(&latest_user.content);
    let normalized = text.to_ascii_lowercase();
    const CUES: [&str; 6] = [
        "copy this string",
        "echo this token",
        "repeat exactly",
        "复制这个字符串",
        "回显这个令牌",
        "原样重复",
    ];

    let matches = CUES
        .iter()
        .flat_map(|cue| {
            normalized
                .match_indices(cue)
                .map(move |(offset, _)| (offset, *cue))
        })
        .collect::<Vec<_>>();
    let [(cue_offset, cue)] = matches.as_slice() else {
        return None;
    };
    let suffix = text.get(cue_offset + cue.len()..)?.trim();
    let candidate = suffix
        .char_indices()
        .filter(|(_, ch)| matches!(ch, ':' | '：'))
        .last()
        .map(|(offset, ch)| &suffix[offset + ch.len_utf8()..])
        .unwrap_or(suffix)
        .trim();
    let candidate = strip_matching_quote(candidate).unwrap_or(candidate);

    (4..=MAX_LITERAL_BYTES)
        .contains(&candidate.len())
        .then_some(())?;
    candidate
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b':' | b'-'))
        .then(|| candidate.to_owned())
}

fn conversation_has_non_text_content(req: &MessagesRequest) -> bool {
    req.messages.iter().any(|message| match &message.content {
        serde_json::Value::String(_) => false,
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .any(|block| block.get("type").and_then(serde_json::Value::as_str) != Some("text")),
        _ => true,
    })
}

pub(crate) fn local_ping_answer(req: &MessagesRequest, enabled: bool) -> Option<&'static str> {
    if !enabled
        || req.max_tokens < 1
        || req.system.is_some()
        || req.messages.len() != 1
        || req.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        || req.tool_choice.is_some()
        || req
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.is_enabled())
        || req.output_config.is_some()
        || req.force_web_search_loop
        || conversation_has_non_text_content(req)
    {
        return None;
    }

    let message = &req.messages[0];
    (message.role == "user"
        && message_text(&message.content)
            .trim()
            .eq_ignore_ascii_case("ping"))
    .then_some("pong")
}

fn strip_matching_quote(value: &str) -> Option<&str> {
    let first = value.chars().next()?;
    if !matches!(first, '\'' | '"' | '`') || value.chars().last()? != first {
        return None;
    }
    value.get(first.len_utf8()..value.len().saturating_sub(first.len_utf8()))
}

const STRICT_JSON_TAIL_BYTES: usize = 4 * 1024;
const STRICT_JSON_LOCAL_RADIUS_BYTES: usize = 256;

fn utf8_tail(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn utf8_local_window(text: &str, offset: usize, needle_len: usize) -> &str {
    let mut start = offset.saturating_sub(STRICT_JSON_LOCAL_RADIUS_BYTES);
    while start > 0 && !text.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = (offset + needle_len + STRICT_JSON_LOCAL_RADIUS_BYTES).min(text.len());
    while end > start && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[start..end]
}

fn has_json_output_command_cue(text: &str) -> bool {
    [
        "return",
        "reply",
        "respond",
        "output",
        "provide",
        "只返回",
        "仅返回",
        "回复",
        "输出",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn has_single_json_value_cue(text: &str) -> bool {
    [
        "exactly one",
        "exactly a single",
        "single minified",
        "one minified",
        "只返回",
        "仅返回",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

pub(crate) fn strict_json_requested(req: &MessagesRequest) -> bool {
    let latest_user_text = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message_text(&message.content))
        .unwrap_or_default();
    let normalized = utf8_tail(&latest_user_text, STRICT_JSON_TAIL_BYTES).to_ascii_lowercase();

    normalized.match_indices("json").any(|(offset, cue)| {
        let window = utf8_local_window(&normalized, offset, cue.len());
        has_json_output_command_cue(window)
            && has_single_json_value_cue(window)
            && has_no_extra_cue(window)
    })
}

pub(crate) fn extract_single_json(text: &str) -> Option<String> {
    if text.len() > MAX_JSON_BYTES.saturating_mul(4) {
        return None;
    }

    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        if !matches!(bytes[cursor], b'{' | b'[') {
            cursor += 1;
            continue;
        }

        let start = cursor;
        let mut stack = Vec::new();
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (offset, byte) in bytes[start..].iter().copied().enumerate() {
            if in_string {
                if escaped {
                    escaped = false;
                } else if byte == b'\\' {
                    escaped = true;
                } else if byte == b'"' {
                    in_string = false;
                }
                continue;
            }

            match byte {
                b'"' => in_string = true,
                b'{' => stack.push(b'}'),
                b'[' => stack.push(b']'),
                b'}' | b']' => {
                    if stack.pop() != Some(byte) {
                        break;
                    }
                    if stack.is_empty() {
                        end = Some(start + offset + 1);
                        break;
                    }
                }
                _ => {}
            }
        }

        let Some(end) = end else {
            return None;
        };
        let candidate = &text[start..end];
        if candidate.len() <= MAX_JSON_BYTES {
            if serde_json::from_str::<serde_json::Value>(candidate).is_ok() {
                values.push(minify_json_preserving_order(candidate));
                if values.len() > 1 {
                    return None;
                }
            }
        }
        cursor = end;
    }

    values.pop()
}

pub(crate) fn append_strict_json_retry_instruction(request_body: &str) -> Option<String> {
    let mut value: serde_json::Value = serde_json::from_str(request_body).ok()?;
    let content = value
        .pointer_mut("/conversationState/currentMessage/userInputMessage/content")?
        .as_str()?
        .to_owned();
    *value.pointer_mut("/conversationState/currentMessage/userInputMessage/content")? =
        serde_json::Value::String(format!(
            "{content}\n\nCorrection: Return exactly one complete JSON value that satisfies the requested schema. Do not include markdown, explanation, or any text before or after the JSON."
        ));
    serde_json::to_string(&value).ok()
}

pub(crate) fn json_satisfies_explicit_constraints(req: &MessagesRequest, json: &str) -> bool {
    let latest_user_text = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message_text(&message.content))
        .unwrap_or_default();
    let constraints = parse_explicit_json_constraints(&latest_user_text);
    if constraints.is_empty() {
        return true;
    }
    let Ok(serde_json::Value::Object(object)) = serde_json::from_str::<serde_json::Value>(json)
    else {
        return false;
    };
    constraints
        .iter()
        .all(|(key, expected)| object.get(key) == Some(expected))
}

fn parse_explicit_json_constraints(text: &str) -> Vec<(String, serde_json::Value)> {
    let lower = text.to_ascii_lowercase();
    let bytes = text.as_bytes();
    let mut constraints = Vec::new();
    let mut cursor = 0;
    while let Some(relative) = lower[cursor..].find("set ") {
        let start = cursor + relative;
        cursor = start + "set ".len();
        if start > 0 {
            let previous = bytes[start - 1];
            if previous.is_ascii_alphanumeric() || previous == b'_' {
                continue;
            }
        }

        let mut position = skip_ascii_whitespace(bytes, cursor);
        let key_start = position;
        while position < bytes.len()
            && (bytes[position].is_ascii_alphanumeric() || matches!(bytes[position], b'_' | b'-'))
        {
            position += 1;
        }
        if position == key_start {
            continue;
        }
        let key = text[key_start..position].to_owned();
        position = skip_ascii_whitespace(bytes, position);
        if !lower[position..].starts_with("to") {
            continue;
        }
        position += 2;
        if position < bytes.len()
            && (bytes[position].is_ascii_alphanumeric() || bytes[position] == b'_')
        {
            continue;
        }
        position = skip_ascii_whitespace(bytes, position);

        let rhs_lower = &lower[position..];
        let reverse_prefix = if rhs_lower.starts_with("the reverse of") {
            Some("the reverse of".len())
        } else if rhs_lower.starts_with("reverse of") {
            Some("reverse of".len())
        } else {
            None
        };
        let expected = if let Some(prefix_len) = reverse_prefix {
            let quoted_start = skip_ascii_whitespace(bytes, position + prefix_len);
            parse_quoted_string(text, quoted_start).map(|(value, _)| {
                serde_json::Value::String(value.chars().rev().collect::<String>())
            })
        } else if let Some((value, _)) = parse_quoted_string(text, position) {
            Some(serde_json::Value::String(value))
        } else if let Some((left, after_left)) = parse_integer(text, position) {
            let operator_position = skip_ascii_whitespace(bytes, after_left);
            let operator = bytes.get(operator_position).copied();
            let right_position = skip_ascii_whitespace(bytes, operator_position + 1);
            parse_integer(text, right_position).and_then(|(right, _)| {
                let value = match operator {
                    Some(b'+') => left.checked_add(right),
                    Some(b'-') => left.checked_sub(right),
                    _ => None,
                }?;
                Some(serde_json::Value::Number(value.into()))
            })
        } else {
            None
        };
        if let Some(expected) = expected {
            constraints.push((key, expected));
        }
    }
    constraints
}

fn skip_ascii_whitespace(bytes: &[u8], mut position: usize) -> usize {
    while position < bytes.len() && bytes[position].is_ascii_whitespace() {
        position += 1;
    }
    position
}

fn parse_quoted_string(text: &str, start: usize) -> Option<(String, usize)> {
    let bytes = text.as_bytes();
    let quote = *bytes.get(start)?;
    if !matches!(quote, b'\'' | b'"') {
        return None;
    }
    let mut escaped = false;
    let mut end = start + 1;
    while end < bytes.len() {
        let byte = bytes[end];
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == quote {
            let raw = &text[start + 1..end];
            let value = if quote == b'"' {
                serde_json::from_str::<String>(&text[start..=end]).ok()?
            } else {
                let mut value = String::with_capacity(raw.len());
                let mut chars = raw.chars();
                while let Some(character) = chars.next() {
                    if character == '\\' {
                        value.push(chars.next().unwrap_or('\\'));
                    } else {
                        value.push(character);
                    }
                }
                value
            };
            return Some((value, end + 1));
        }
        end += 1;
    }
    None
}

fn parse_integer(text: &str, start: usize) -> Option<(i64, usize)> {
    let bytes = text.as_bytes();
    let mut end = start;
    if matches!(bytes.get(end), Some(b'+' | b'-')) {
        end += 1;
    }
    let digits_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == digits_start {
        return None;
    }
    Some((text[start..end].parse().ok()?, end))
}

fn has_exact_cue(text: &str) -> bool {
    [
        "exactly",
        "single word",
        "exactly this json",
        "只返回",
        "仅返回",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn has_no_extra_cue(text: &str) -> bool {
    [
        "nothing else",
        "no extra text",
        "no explanation",
        "no markdown",
        "do not add punctuation",
        "不要解释",
        "无额外文本",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn has_unsafe_contract_cue(text: &str) -> bool {
    [
        "you are",
        "your identity",
        "identify yourself",
        "current date",
        "current time",
        "today",
        "now",
        "{{",
        "}}",
        "<user",
    ]
    .iter()
    .any(|cue| text.contains(cue))
}

fn minify_json_preserving_order(json: &str) -> String {
    let mut output = String::with_capacity(json.len());
    let mut in_string = false;
    let mut escaped = false;
    for character in json.chars() {
        if in_string {
            output.push(character);
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
            output.push(character);
        } else if !character.is_whitespace() {
            output.push(character);
        }
    }
    output
}

fn quoted_ascii_literals(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut values = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let quote = bytes[index];
        if !matches!(quote, b'\'' | b'"') {
            index += 1;
            continue;
        }
        let start = index + 1;
        let Some(relative_end) = bytes[start..].iter().position(|byte| *byte == quote) else {
            break;
        };
        let end = start + relative_end;
        let candidate = &text[start..end];
        if !candidate.is_empty()
            && candidate.len() <= MAX_LITERAL_BYTES
            && candidate.is_ascii()
            && candidate
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        {
            values.push(candidate.to_owned());
        }
        index = end + 1;
    }
    values
}

fn message_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::model::config::ToolCompatibilityMode;

    fn request(system: Option<&str>, user: &str) -> MessagesRequest {
        let mut value = json!({
            "model": "claude-opus-4-8",
            "max_tokens": 128,
            "messages": [{"role": "user", "content": user}]
        });
        if let Some(system) = system {
            value["system"] = json!(system);
        }
        serde_json::from_value(value).unwrap()
    }

    fn identity_and_exact_request(separate_blocks: bool) -> MessagesRequest {
        let exact =
            "Respond with exactly the single word 'alpha_42' and nothing else. No explanation.";
        let system = if separate_blocks {
            json!([
                {"type": "text", "text": super::super::converter::CLAUDE_CODE_IDENTITY_ANCHOR},
                {"type": "text", "text": exact}
            ])
        } else {
            json!([{
                "type": "text",
                "text": format!(
                    "{}\n{}",
                    super::super::converter::CLAUDE_CODE_IDENTITY_ANCHOR,
                    exact
                )
            }])
        };
        serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 128,
            "system": system,
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "noop",
                "description": "An optional passive tool",
                "input_schema": {"type": "object", "properties": {}}
            }]
        }))
        .unwrap()
    }

    #[test]
    fn claude_code_identity_anchor_allows_exact_contract_only_in_claude_code_mode() {
        for separate_blocks in [true, false] {
            let req = identity_and_exact_request(separate_blocks);
            assert_eq!(
                exact_system_output(&req, ToolCompatibilityMode::ClaudeCode),
                Some(ExactOutput::Text("alpha_42".into()))
            );
            assert_eq!(exact_system_output(&req, ToolCompatibilityMode::Raw), None);
        }
    }

    #[test]
    fn arbitrary_identity_still_blocks_exact_contract() {
        let req = request(
            Some(
                "You are CodeAssist v2.\nReturn exactly the single word 'alpha_42' and nothing else. No explanation.",
            ),
            "hello",
        );
        assert_eq!(
            exact_system_output(&req, ToolCompatibilityMode::ClaudeCode),
            None
        );
    }

    #[test]
    fn parses_static_ascii_literal_from_strict_system() {
        let req = request(
            Some(
                "Respond to every user message with exactly the single word 'alpha_42' and nothing else. Do not add punctuation or explanation.",
            ),
            "hello",
        );
        assert_eq!(
            exact_system_output(&req, ToolCompatibilityMode::Raw),
            Some(ExactOutput::Text("alpha_42".into()))
        );
    }

    #[test]
    fn parses_and_minifies_static_json_from_strict_system() {
        let req = request(
            Some(
                "Respond with exactly this JSON object, no markdown fence, no extra text:\n{\"a\": 330, \"b\": 360}",
            ),
            "hello",
        );
        assert_eq!(
            exact_system_output(&req, ToolCompatibilityMode::Raw),
            Some(ExactOutput::Json("{\"a\":330,\"b\":360}".into()))
        );
    }

    #[test]
    fn rejects_identity_dynamic_and_ambiguous_system_contracts() {
        assert_eq!(
            exact_system_output(
                &request(Some("You are CodeAssist v2."), "hello"),
                ToolCompatibilityMode::Raw,
            ),
            None
        );
        assert_eq!(
            exact_system_output(
                &request(
                    Some("Return exactly the current date and nothing else."),
                    "hello",
                ),
                ToolCompatibilityMode::Raw,
            ),
            None
        );
        assert_eq!(
            exact_system_output(
                &request(
                    Some("Return exactly 'alpha' or 'beta' and nothing else."),
                    "hello",
                ),
                ToolCompatibilityMode::Raw,
            ),
            None
        );
    }

    #[test]
    fn rejects_system_shortcut_with_required_tools_or_thinking() {
        let mut with_tool = request(
            Some("Return exactly the single word 'alpha' and nothing else."),
            "hello",
        );
        with_tool.tools = Some(vec![
            serde_json::from_value(json!({
                "name": "echo",
                "description": "echo",
                "input_schema": {"type": "object"}
            }))
            .unwrap(),
        ]);
        with_tool.tool_choice =
            Some(serde_json::from_value(json!({"type": "tool", "name": "echo"})).unwrap());
        assert_eq!(
            exact_system_output(&with_tool, ToolCompatibilityMode::Raw),
            None
        );

        let mut with_thinking = request(
            Some("Return exactly the single word 'alpha' and nothing else."),
            "hello",
        );
        with_thinking.thinking = Some(super::super::types::Thinking {
            thinking_type: "enabled".into(),
            budget_tokens: 1024,
        });
        assert_eq!(
            exact_system_output(&with_thinking, ToolCompatibilityMode::Raw),
            None
        );
    }

    #[test]
    fn passive_tools_allow_static_exact_system_output() {
        let mut req = request(
            Some("Respond with exactly the single word 'alpha_42' and nothing else."),
            "hello",
        );
        req.tools = Some(vec![
            serde_json::from_value(json!({
                "name": "noop",
                "description": "A passive tool that is not required",
                "input_schema": {"type": "object"}
            }))
            .unwrap(),
        ]);

        assert_eq!(
            exact_system_output(&req, ToolCompatibilityMode::Raw),
            Some(ExactOutput::Text("alpha_42".into()))
        );
    }

    #[test]
    fn passive_tools_allow_auto_and_none_tool_choice() {
        for tool_choice in ["auto", "none"] {
            let mut req = request(
                Some("Respond with exactly the single word 'alpha_42' and nothing else."),
                "hello",
            );
            req.tools = Some(vec![
                serde_json::from_value(json!({
                    "name": "noop",
                    "description": "A passive tool that is not required",
                    "input_schema": {"type": "object"}
                }))
                .unwrap(),
            ]);
            req.tool_choice = Some(serde_json::from_value(json!({"type": tool_choice})).unwrap());

            assert_eq!(
                exact_system_output(&req, ToolCompatibilityMode::Raw),
                Some(ExactOutput::Text("alpha_42".into())),
                "tool_choice={tool_choice} should remain passive"
            );
        }
    }

    #[test]
    fn passive_tools_reject_required_choices_and_tool_history() {
        for tool_choice in [
            json!({"type": "any"}),
            json!({"type": "tool", "name": "noop"}),
        ] {
            let mut req = request(
                Some("Respond with exactly the single word 'alpha_42' and nothing else."),
                "hello",
            );
            req.tool_choice = Some(serde_json::from_value(tool_choice).unwrap());
            assert_eq!(exact_system_output(&req, ToolCompatibilityMode::Raw), None);
        }

        for block in [
            json!({"type": "tool_use", "id": "toolu_1", "name": "noop", "input": {}}),
            json!({"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}),
        ] {
            let mut req = request(
                Some("Respond with exactly the single word 'alpha_42' and nothing else."),
                "hello",
            );
            req.messages.insert(
                0,
                serde_json::from_value(json!({"role": "assistant", "content": [block]})).unwrap(),
            );
            assert_eq!(exact_system_output(&req, ToolCompatibilityMode::Raw), None);
        }
    }

    #[test]
    fn ping_contract_accepts_only_a_single_plain_health_message() {
        assert_eq!(
            local_ping_answer(&request(None, " ping "), true),
            Some("pong")
        );
        assert_eq!(
            local_ping_answer(&request(None, "PING"), true),
            Some("pong")
        );
        assert_eq!(local_ping_answer(&request(None, "ping"), false), None);
        assert_eq!(local_ping_answer(&request(None, "ping please"), true), None);
    }

    #[test]
    fn ping_contract_rejects_context_tools_thinking_and_multimodal_content() {
        let mut with_system = request(Some("Be concise."), "ping");
        assert_eq!(local_ping_answer(&with_system, true), None);

        let mut with_history = request(None, "ping");
        with_history.messages.insert(
            0,
            serde_json::from_value(json!({"role": "assistant", "content": "ready"})).unwrap(),
        );
        assert_eq!(local_ping_answer(&with_history, true), None);

        let mut with_tools = request(None, "ping");
        with_tools.tools = Some(vec![
            serde_json::from_value(json!({
                "name": "noop",
                "description": "noop",
                "input_schema": {"type": "object"}
            }))
            .unwrap(),
        ]);
        assert_eq!(local_ping_answer(&with_tools, true), None);

        let mut with_tool_choice = request(None, "ping");
        with_tool_choice.tool_choice =
            Some(serde_json::from_value(json!({"type": "none"})).unwrap());
        assert_eq!(local_ping_answer(&with_tool_choice, true), None);

        let mut with_thinking = request(None, "ping");
        with_thinking.thinking = Some(super::super::types::Thinking {
            thinking_type: "enabled".into(),
            budget_tokens: 1024,
        });
        assert_eq!(local_ping_answer(&with_thinking, true), None);

        let mut with_image = request(None, "ping");
        with_image.messages[0].content = json!([
            {"type": "text", "text": "ping"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA=="}}
        ]);
        assert_eq!(local_ping_answer(&with_image, true), None);

        let mut with_output_config = request(None, "ping");
        with_output_config.output_config = Some(super::super::types::OutputConfig {
            effort: "low".into(),
        });
        assert_eq!(local_ping_answer(&with_output_config, true), None);

        let mut with_web_search = request(None, "ping");
        with_web_search.force_web_search_loop = true;
        assert_eq!(local_ping_answer(&with_web_search, true), None);

        let mut without_budget = request(None, "ping");
        without_budget.max_tokens = 0;
        assert_eq!(local_ping_answer(&without_budget, true), None);

        with_system.system = None;
        assert_eq!(local_ping_answer(&with_system, true), Some("pong"));
    }

    #[test]
    fn exact_user_echo_accepts_bounded_explicit_contracts() {
        assert_eq!(
            exact_user_echo(&request(
                None,
                "I need you to copy this string into your response so I can verify the connection: CHECK-1234",
            )),
            Some("CHECK-1234".into())
        );
        assert_eq!(
            exact_user_echo(&request(None, "Echo this token exactly: ABC_def-42")),
            Some("ABC_def-42".into())
        );
        assert_eq!(
            exact_user_echo(&request(None, "Please repeat exactly 'PING.42'")),
            Some("PING.42".into())
        );
    }

    #[test]
    fn exact_user_echo_rejects_ambiguous_or_invalid_candidates() {
        assert_eq!(
            exact_user_echo(&request(None, "Echo this token exactly: FIRST SECOND")),
            None
        );
        assert_eq!(
            exact_user_echo(&request(
                None,
                &format!("Echo this token exactly: {}", "A".repeat(129))
            )),
            None
        );
        assert_eq!(
            exact_user_echo(&request(None, "Echo this token exactly: ABC/DEF")),
            None
        );
        assert_eq!(
            exact_user_echo(&request(
                None,
                "Please copy the configuration before editing it."
            )),
            None
        );
    }

    #[test]
    fn exact_user_echo_rejects_tools_thinking_and_non_text_content() {
        let mut with_tools = request(None, "Echo this token exactly: SAFE-42");
        with_tools.tools = Some(vec![
            serde_json::from_value(json!({
                "name": "noop",
                "description": "noop",
                "input_schema": {"type": "object"}
            }))
            .unwrap(),
        ]);
        assert_eq!(exact_user_echo(&with_tools), None);

        let mut with_tool_choice = request(None, "Echo this token exactly: SAFE-42");
        with_tool_choice.tool_choice =
            Some(serde_json::from_value(json!({"type": "auto"})).unwrap());
        assert_eq!(exact_user_echo(&with_tool_choice), None);

        let mut with_thinking = request(None, "Echo this token exactly: SAFE-42");
        with_thinking.thinking = Some(super::super::types::Thinking {
            thinking_type: "enabled".into(),
            budget_tokens: 1024,
        });
        assert_eq!(exact_user_echo(&with_thinking), None);

        for block_type in ["document", "image"] {
            let mut req = request(None, "placeholder");
            req.messages[0].content = json!([
                {"type": "text", "text": "Echo this token exactly: SAFE-42"},
                {"type": block_type, "source": {"type": "base64", "media_type": "application/octet-stream", "data": "AA=="}}
            ]);
            assert_eq!(exact_user_echo(&req), None);
        }
    }

    #[test]
    fn extracts_one_balanced_json_and_rejects_truncation_or_ambiguity() {
        assert_eq!(
            extract_single_json("prefix ```json\n{\"a\":1}\n``` suffix"),
            Some("{\"a\":1}".into())
        );
        assert_eq!(extract_single_json("{\"a\":1"), None);
        assert_eq!(extract_single_json("{\"a\":1} {\"b\":2}"), None);
    }

    #[test]
    fn balanced_json_scanner_handles_nested_strings_and_braces() {
        assert_eq!(
            extract_single_json(
                "note {\"text\":\"} and \\\"quoted\\\"\",\"items\":[1,{\"ok\":true}]} done"
            ),
            Some("{\"text\":\"} and \\\"quoted\\\"\",\"items\":[1,{\"ok\":true}]}".into())
        );
    }

    #[test]
    fn strict_json_requires_exact_and_no_extra_cues() {
        assert!(strict_json_requested(&request(
            None,
            "Reply with exactly one minified JSON object and no markdown, no explanation."
        )));
        assert!(!strict_json_requested(&request(
            None,
            "Please answer in JSON."
        )));
    }

    #[test]
    fn strict_json_ignores_distant_cues_in_large_code_context() {
        let noisy_code = r#"
            value, err := json.Marshal(payload)
            // Exactly one purchase should succeed.
            const historical_instruction: &str = "no markdown";
        "#
        .repeat(4_000);
        let prompt = format!(
            "{noisy_code}\n\nThe implementation is ready. Build the project and run the tests."
        );

        assert!(!strict_json_requested(&request(None, &prompt)));
    }

    #[test]
    fn strict_json_accepts_explicit_instruction_at_end_of_large_context() {
        let context = "let value = json.Marshal(payload);\n".repeat(20_000);
        let prompt = format!(
            "{context}\nReply with exactly one minified JSON object and no markdown or explanation."
        );

        assert!(strict_json_requested(&request(None, &prompt)));
    }

    #[test]
    fn strict_json_requires_cues_in_one_local_window() {
        let prompt = format!(
            "Reply with exactly one result.{}JSON is mentioned here.{}No markdown.",
            "x".repeat(700),
            "y".repeat(700),
        );

        assert!(!strict_json_requested(&request(None, &prompt)));
    }

    #[test]
    fn strict_json_accepts_explicit_chinese_instruction() {
        assert!(strict_json_requested(&request(
            None,
            "仅返回一个 JSON 对象，不要解释。"
        )));
    }

    #[test]
    fn retry_instruction_only_updates_current_message_content() {
        let original = json!({
            "conversationState": {
                "history": [{"userInputMessage": {"content": "history"}}],
                "currentMessage": {"userInputMessage": {"content": "current"}}
            }
        });
        let updated = append_strict_json_retry_instruction(&original.to_string()).unwrap();
        let updated: serde_json::Value = serde_json::from_str(&updated).unwrap();
        assert_eq!(
            updated["conversationState"]["history"][0]["userInputMessage"]["content"],
            "history"
        );
        let current = updated["conversationState"]["currentMessage"]["userInputMessage"]["content"]
            .as_str()
            .unwrap();
        assert!(current.starts_with("current"));
        assert!(current.contains("complete JSON"));
    }

    #[test]
    fn validates_generic_explicit_json_constraints() {
        let req = request(
            None,
            "Reply with exactly one minified JSON object and no explanation. Set alpha to the reverse of 'testz'. Set total to 29 + 8. Set marker to \"VALUE-42\".",
        );
        assert!(json_satisfies_explicit_constraints(
            &req,
            r#"{"alpha":"ztset","total":37,"marker":"VALUE-42"}"#
        ));
        assert!(!json_satisfies_explicit_constraints(
            &req,
            r#"{"alpha":" ztset","total":37,"marker":"VALUE-42"}"#
        ));
        assert!(!json_satisfies_explicit_constraints(
            &req,
            r#"{"alpha":"zteset","total":37,"marker":"VALUE-42"}"#
        ));
        assert!(!json_satisfies_explicit_constraints(
            &req,
            r#"{"alpha":"ztset","total":36,"marker":"VALUE-42"}"#
        ));
    }

    #[test]
    fn accepts_valid_json_when_no_supported_explicit_constraint_exists() {
        let req = request(
            None,
            "Reply with exactly one minified JSON object and no explanation. Use suitable values.",
        );
        assert!(json_satisfies_explicit_constraints(&req, r#"{"a":1}"#));
    }
}
