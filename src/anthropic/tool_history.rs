use std::collections::{HashMap, HashSet};

use crate::kiro::model::requests::{conversation::Message, tool::ToolResult};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolIdNormalization {
    pub(crate) rewritten_ids: HashMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolHistoryError {
    DuplicateToolUseId(String),
    DuplicateToolResultId(String),
    AmbiguousNormalizedId(String),
    OrphanToolResultId(String),
}

impl std::fmt::Display for ToolHistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateToolUseId(id) => write!(formatter, "duplicate tool_use id: {id:?}"),
            Self::DuplicateToolResultId(id) => {
                write!(formatter, "duplicate tool_result id: {id:?}")
            }
            Self::AmbiguousNormalizedId(id) => {
                write!(formatter, "ambiguous normalized tool id: {id:?}")
            }
            Self::OrphanToolResultId(id) => {
                write!(
                    formatter,
                    "tool_result references unknown tool_use id: {id:?}"
                )
            }
        }
    }
}

impl std::error::Error for ToolHistoryError {}

pub(crate) fn is_upstream_safe_tool_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn normalized_tool_id(id: &str) -> String {
    if is_upstream_safe_tool_id(id) {
        return id.to_owned();
    }

    let digest = Sha256::digest(id.as_bytes());
    format!("tooluse_{}", hex::encode(&digest[..20]))
}

pub(crate) fn normalize_tool_history_ids(
    history: &mut [Message],
    current_results: &mut [ToolResult],
) -> Result<ToolIdNormalization, ToolHistoryError> {
    let mut original_to_normalized = HashMap::new();
    let mut normalized_to_original = HashMap::new();
    let mut rewritten_ids = HashMap::new();
    let mut outstanding_tool_uses = HashSet::new();
    let mut seen_results = HashSet::new();

    for message in history.iter() {
        match message {
            Message::Assistant(message) => {
                let Some(tool_uses) = &message.assistant_response_message.tool_uses else {
                    continue;
                };

                for tool_use in tool_uses {
                    let original = tool_use.tool_use_id.clone();
                    if original_to_normalized.contains_key(&original) {
                        return Err(ToolHistoryError::DuplicateToolUseId(original));
                    }

                    let normalized = normalized_tool_id(&original);
                    if let Some(owner) = normalized_to_original.get(&normalized) {
                        if owner != &original {
                            return Err(ToolHistoryError::AmbiguousNormalizedId(normalized));
                        }
                    }

                    normalized_to_original.insert(normalized.clone(), original.clone());
                    original_to_normalized.insert(original.clone(), normalized.clone());
                    outstanding_tool_uses.insert(original.clone());
                    if original != normalized {
                        rewritten_ids.insert(original, normalized);
                    }
                }
            }
            Message::User(message) => {
                for result in &message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    validate_result_id(
                        &result.tool_use_id,
                        &original_to_normalized,
                        &mut outstanding_tool_uses,
                        &mut seen_results,
                    )?;
                }
            }
        }
    }

    for result in current_results.iter() {
        validate_result_id(
            &result.tool_use_id,
            &original_to_normalized,
            &mut outstanding_tool_uses,
            &mut seen_results,
        )?;
    }

    for message in history.iter_mut() {
        match message {
            Message::Assistant(message) => {
                if let Some(tool_uses) = &mut message.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        tool_use.tool_use_id =
                            original_to_normalized[&tool_use.tool_use_id].clone();
                    }
                }
            }
            Message::User(message) => {
                for result in &mut message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    result.tool_use_id = original_to_normalized[&result.tool_use_id].clone();
                }
            }
        }
    }
    for result in current_results {
        result.tool_use_id = original_to_normalized[&result.tool_use_id].clone();
    }

    Ok(ToolIdNormalization { rewritten_ids })
}

fn validate_result_id(
    id: &str,
    original_to_normalized: &HashMap<String, String>,
    outstanding_tool_uses: &mut HashSet<String>,
    seen_results: &mut HashSet<String>,
) -> Result<(), ToolHistoryError> {
    if !original_to_normalized.contains_key(id) {
        return Err(ToolHistoryError::OrphanToolResultId(id.to_owned()));
    }
    if !seen_results.insert(id.to_owned()) {
        return Err(ToolHistoryError::DuplicateToolResultId(id.to_owned()));
    }
    if !outstanding_tool_uses.remove(id) {
        return Err(ToolHistoryError::DuplicateToolResultId(id.to_owned()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::kiro::model::requests::{
        conversation::{
            AssistantMessage, HistoryAssistantMessage, HistoryUserMessage, Message,
            UserInputMessageContext, UserMessage,
        },
        tool::{ToolResult, ToolUseEntry},
    };

    use super::{
        ToolHistoryError, is_upstream_safe_tool_id, normalize_tool_history_ids, normalized_tool_id,
    };

    fn assistant_with_tool_uses(ids: &[&str]) -> Message {
        Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: AssistantMessage::new("calling tool").with_tool_uses(
                ids.iter()
                    .map(|id| ToolUseEntry::new(*id, "get_weather"))
                    .collect(),
            ),
        })
    }

    fn user_with_tool_results(ids: &[&str]) -> Message {
        Message::User(HistoryUserMessage {
            user_input_message: UserMessage::new("tool result", "claude-sonnet-4").with_context(
                UserInputMessageContext::new().with_tool_results(
                    ids.iter()
                        .map(|id| ToolResult::success(*id, "ok"))
                        .collect(),
                ),
            ),
        })
    }

    fn tool_use_id(message: &Message, index: usize) -> &str {
        let Message::Assistant(message) = message else {
            panic!("expected assistant message")
        };
        &message
            .assistant_response_message
            .tool_uses
            .as_ref()
            .expect("tool uses")[index]
            .tool_use_id
    }

    fn historical_result_id(message: &Message, index: usize) -> &str {
        let Message::User(message) = message else {
            panic!("expected user message")
        };
        &message
            .user_input_message
            .user_input_message_context
            .tool_results[index]
            .tool_use_id
    }

    #[test]
    fn validates_upstream_tool_id_contract() {
        assert!(is_upstream_safe_tool_id("tooluse_abc-123"));
        assert!(!is_upstream_safe_tool_id("functions.AskUserQuestion:1"));
        assert!(!is_upstream_safe_tool_id("tool/get_weather/1"));
        assert!(!is_upstream_safe_tool_id(""));
        assert!(!is_upstream_safe_tool_id(&"a".repeat(65)));
        assert!(!is_upstream_safe_tool_id("tooluse_工具"));
    }

    #[test]
    fn normalizes_colon_id_for_historical_pair() {
        let original = "functions.AskUserQuestion:1";
        let mut history = vec![
            assistant_with_tool_uses(&[original]),
            user_with_tool_results(&[original]),
        ];
        let mut current = vec![];

        let report = normalize_tool_history_ids(&mut history, &mut current).unwrap();

        let normalized = tool_use_id(&history[0], 0);
        assert!(normalized.starts_with("tooluse_"));
        assert_eq!(normalized.len(), 48);
        assert_eq!(historical_result_id(&history[1], 0), normalized);
        assert_eq!(
            report.rewritten_ids.get(original).map(String::as_str),
            Some(normalized)
        );
    }

    #[test]
    fn normalizes_slash_id_for_current_result_pair() {
        let original = "tool/get_weather/1";
        let mut history = vec![assistant_with_tool_uses(&[original])];
        let mut current = vec![ToolResult::success(original, "sunny")];

        normalize_tool_history_ids(&mut history, &mut current).unwrap();

        assert_eq!(current[0].tool_use_id, tool_use_id(&history[0], 0));
        assert!(is_upstream_safe_tool_id(&current[0].tool_use_id));
    }

    #[test]
    fn normalizes_empty_and_overlong_ids() {
        let overlong = "x".repeat(65);
        let mut history = vec![assistant_with_tool_uses(&["", &overlong])];
        let mut current = vec![
            ToolResult::success("", "empty"),
            ToolResult::success(&overlong, "long"),
        ];

        normalize_tool_history_ids(&mut history, &mut current).unwrap();

        assert!(is_upstream_safe_tool_id(tool_use_id(&history[0], 0)));
        assert!(is_upstream_safe_tool_id(tool_use_id(&history[0], 1)));
        assert_eq!(tool_use_id(&history[0], 0), current[0].tool_use_id);
        assert_eq!(tool_use_id(&history[0], 1), current[1].tool_use_id);
    }

    #[test]
    fn leaves_safe_ids_unchanged() {
        let original = "tooluse_abc-123";
        let mut history = vec![assistant_with_tool_uses(&[original])];
        let mut current = vec![ToolResult::success(original, "ok")];

        let report = normalize_tool_history_ids(&mut history, &mut current).unwrap();

        assert_eq!(tool_use_id(&history[0], 0), original);
        assert_eq!(current[0].tool_use_id, original);
        assert!(report.rewritten_ids.is_empty());
    }

    #[test]
    fn different_invalid_ids_do_not_collide() {
        let mut history = vec![assistant_with_tool_uses(&["a:b", "a.b"])];
        let mut current = vec![
            ToolResult::success("a:b", "colon"),
            ToolResult::success("a.b", "dot"),
        ];

        normalize_tool_history_ids(&mut history, &mut current).unwrap();

        assert_ne!(tool_use_id(&history[0], 0), tool_use_id(&history[0], 1));
        assert_eq!(tool_use_id(&history[0], 0), current[0].tool_use_id);
        assert_eq!(tool_use_id(&history[0], 1), current[1].tool_use_id);
    }

    #[test]
    fn rejects_normalized_id_collision_with_existing_safe_id() {
        let invalid = "a:b";
        let normalized = normalized_tool_id(invalid);
        let mut history = vec![assistant_with_tool_uses(&[invalid, &normalized])];

        let error = normalize_tool_history_ids(&mut history, &mut []).unwrap_err();

        assert_eq!(error, ToolHistoryError::AmbiguousNormalizedId(normalized));
    }

    #[test]
    fn rejects_duplicate_tool_use_ids() {
        let mut history = vec![assistant_with_tool_uses(&["a:b", "a:b"])];
        let mut current = vec![ToolResult::success("a:b", "ok")];

        let error = normalize_tool_history_ids(&mut history, &mut current).unwrap_err();

        assert_eq!(error, ToolHistoryError::DuplicateToolUseId("a:b".into()));
    }

    #[test]
    fn rejects_duplicate_tool_result_ids_across_history_and_current_message() {
        let mut history = vec![
            assistant_with_tool_uses(&["a:b"]),
            user_with_tool_results(&["a:b"]),
        ];
        let mut current = vec![ToolResult::success("a:b", "duplicate")];

        let error = normalize_tool_history_ids(&mut history, &mut current).unwrap_err();

        assert_eq!(error, ToolHistoryError::DuplicateToolResultId("a:b".into()));
    }

    #[test]
    fn rejects_orphaned_historical_and_current_results() {
        let mut historical = vec![user_with_tool_results(&["missing.history:1"])];
        let error = normalize_tool_history_ids(&mut historical, &mut []).unwrap_err();
        assert_eq!(
            error,
            ToolHistoryError::OrphanToolResultId("missing.history:1".into())
        );

        let mut current_history = vec![assistant_with_tool_uses(&["known:1"])];
        let error = normalize_tool_history_ids(
            &mut current_history,
            &mut [ToolResult::success("missing.current:1", "orphan")],
        )
        .unwrap_err();
        assert_eq!(
            error,
            ToolHistoryError::OrphanToolResultId("missing.current:1".into())
        );
    }

    #[test]
    fn rejects_tool_result_that_precedes_its_tool_use() {
        let mut history = vec![
            user_with_tool_results(&["future:1"]),
            assistant_with_tool_uses(&["future:1"]),
        ];

        let error = normalize_tool_history_ids(&mut history, &mut []).unwrap_err();

        assert_eq!(
            error,
            ToolHistoryError::OrphanToolResultId("future:1".into())
        );
    }
}
