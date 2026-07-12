use super::types::MessagesRequest;

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

pub(crate) fn exact_system_output(req: &MessagesRequest) -> Option<ExactOutput> {
    if req.tools.as_ref().is_some_and(|tools| !tools.is_empty())
        || req.tool_choice.is_some()
        || req
            .thinking
            .as_ref()
            .is_some_and(|thinking| thinking.is_enabled())
    {
        return None;
    }

    let system = req
        .system
        .as_ref()?
        .iter()
        .map(|message| message.text.as_str())
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

pub(crate) fn strict_json_requested(req: &MessagesRequest) -> bool {
    let latest_user_text = req
        .messages
        .iter()
        .rev()
        .find(|message| message.role == "user")
        .map(|message| message_text(&message.content))
        .unwrap_or_default();
    let normalized = latest_user_text.to_lowercase();
    normalized.contains("json")
        && (normalized.contains("exactly one")
            || normalized.contains("exactly a single")
            || normalized.contains("single minified")
            || normalized.contains("只返回")
            || normalized.contains("仅返回"))
        && has_no_extra_cue(&normalized)
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

    #[test]
    fn parses_static_ascii_literal_from_strict_system() {
        let req = request(
            Some(
                "Respond to every user message with exactly the single word 'alpha_42' and nothing else. Do not add punctuation or explanation.",
            ),
            "hello",
        );
        assert_eq!(
            exact_system_output(&req),
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
            exact_system_output(&req),
            Some(ExactOutput::Json("{\"a\":330,\"b\":360}".into()))
        );
    }

    #[test]
    fn rejects_identity_dynamic_and_ambiguous_system_contracts() {
        assert_eq!(
            exact_system_output(&request(Some("You are CodeAssist v2."), "hello")),
            None
        );
        assert_eq!(
            exact_system_output(&request(
                Some("Return exactly the current date and nothing else."),
                "hello"
            )),
            None
        );
        assert_eq!(
            exact_system_output(&request(
                Some("Return exactly 'alpha' or 'beta' and nothing else."),
                "hello"
            )),
            None
        );
    }

    #[test]
    fn rejects_system_shortcut_with_tools_or_thinking() {
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
        assert_eq!(exact_system_output(&with_tool), None);

        let mut with_thinking = request(
            Some("Return exactly the single word 'alpha' and nothing else."),
            "hello",
        );
        with_thinking.thinking = Some(super::super::types::Thinking {
            thinking_type: "enabled".into(),
            budget_tokens: 1024,
        });
        assert_eq!(exact_system_output(&with_thinking), None);
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
