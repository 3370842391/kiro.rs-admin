use std::collections::{HashMap, HashSet};

use crate::kiro::model::requests::{conversation::Message, tool::ToolResult};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ToolIdNormalization {
    pub(crate) rewritten_ids: HashMap<String, String>,
    /// 同一 id 且 name/input 完全一致的重复 tool_use，丢弃后语义无损。
    pub(crate) deduplicated_tool_uses: usize,
    /// 同一 id 但 name/input 不同的 tool_use。只保留首个，丢弃其余——有损，需要告警。
    pub(crate) dropped_conflicting_tool_uses: usize,
    /// 找不到对应 tool_use 的孤立 tool_result（含"结果早于调用"的乱序）。
    pub(crate) dropped_orphan_results: usize,
    /// 对应 tool_use 已被别的 tool_result 消费掉的重复结果。
    pub(crate) dropped_duplicate_results: usize,
}

impl ToolIdNormalization {
    /// 本次归一化是否丢弃过内容（用于决定日志级别）。
    pub(crate) fn dropped_anything(&self) -> bool {
        self.dropped_conflicting_tool_uses > 0
            || self.dropped_orphan_results > 0
            || self.dropped_duplicate_results > 0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ToolHistoryError {
    /// 两个不同的原始 id 归一化后撞成同一个 Kiro id。
    ///
    /// 这是**唯一**保留的硬失败：继续下去会把两次不同的工具调用合并成一次，
    /// 静默串改对话语义，比拒绝请求更糟。归一化用 SHA256 前 20 字节，
    /// 真实碰撞概率可忽略，实际只会在上游同时发来原始 id 和它的归一化形式时出现。
    AmbiguousNormalizedId(String),
}

impl std::fmt::Display for ToolHistoryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AmbiguousNormalizedId(id) => {
                write!(formatter, "ambiguous normalized tool id: {id:?}")
            }
        }
    }
}

impl std::error::Error for ToolHistoryError {}

/// 单条 tool_result 的处置方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultDisposition {
    Keep,
    /// 引用了不存在（或尚未出现）的 tool_use。
    DropOrphan,
    /// 对应的 tool_use 已经被前一条 tool_result 消费。
    DropDuplicate,
}

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

/// 归一化工具 id，并把**可修复**的历史损坏就地修掉而不是拒绝整轮请求。
///
/// 客户端（尤其经 NewAPI 之类做过 OpenAI ↔ Anthropic 格式转换的链路）经常送来
/// 配对已经断掉的历史：tool_result 找不到 tool_use、同一个 id 出现两次、结果排在
/// 调用前面。早期实现对这些一律 `Err` → 整轮 400。问题是客户端会带着**同一份**坏历史
/// 无限重试，每次都被拒，会话就永久卡死，自己再也恢复不了。
///
/// 现在的策略：坏的部分丢掉，好的部分照常跑。丢弃动作与下游 `validate_tool_pairing`
/// 对孤立 tool_use 的处理对称——那边早就是"移除并继续"，这边不该更严格。
///
/// 只有 [`ToolHistoryError::AmbiguousNormalizedId`] 仍然硬失败，因为它会静默合并
/// 两次不同的调用，属于改坏语义而不是丢内容。
pub(crate) fn normalize_tool_history_ids(
    history: &mut [Message],
    current_results: &mut Vec<ToolResult>,
) -> Result<ToolIdNormalization, ToolHistoryError> {
    let plan = plan_tool_use_dedup(history);
    let mut original_to_normalized = HashMap::new();
    let mut normalized_to_original = HashMap::new();
    let mut rewritten_ids = HashMap::new();
    let mut outstanding_tool_uses = HashSet::new();
    let mut seen_results = HashSet::new();
    // 每条 history 消息里需要丢弃的 tool_result 下标。
    let mut result_drops: Vec<Vec<usize>> = vec![Vec::new(); history.len()];
    let mut dropped_orphan_results = 0usize;
    let mut dropped_duplicate_results = 0usize;

    for (message_index, message) in history.iter().enumerate() {
        match message {
            Message::Assistant(message) => {
                let Some(tool_uses) = &message.assistant_response_message.tool_uses else {
                    continue;
                };

                for (tool_index, tool_use) in tool_uses.iter().enumerate() {
                    if plan.drops[message_index].contains(&tool_index) {
                        continue;
                    }
                    let original = tool_use.tool_use_id.clone();
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
                for (result_index, result) in message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                    .iter()
                    .enumerate()
                {
                    match classify_result_id(
                        &result.tool_use_id,
                        &original_to_normalized,
                        &mut outstanding_tool_uses,
                        &mut seen_results,
                    ) {
                        ResultDisposition::Keep => {}
                        ResultDisposition::DropOrphan => {
                            dropped_orphan_results += 1;
                            result_drops[message_index].push(result_index);
                        }
                        ResultDisposition::DropDuplicate => {
                            dropped_duplicate_results += 1;
                            result_drops[message_index].push(result_index);
                        }
                    }
                }
            }
        }
    }

    // 当前消息的 tool_result 同样过滤。下游 `validate_tool_pairing` 也会兜一层，
    // 但这里必须先剔除，否则后面的 id 重写会索引到不存在的键。
    current_results.retain(|result| {
        match classify_result_id(
            &result.tool_use_id,
            &original_to_normalized,
            &mut outstanding_tool_uses,
            &mut seen_results,
        ) {
            ResultDisposition::Keep => true,
            ResultDisposition::DropOrphan => {
                dropped_orphan_results += 1;
                false
            }
            ResultDisposition::DropDuplicate => {
                dropped_duplicate_results += 1;
                false
            }
        }
    });

    // 先落地所有删除，再统一重写 id，避免下标错位。
    for (message_index, message) in history.iter_mut().enumerate() {
        match message {
            Message::Assistant(message) => {
                let duplicates = &plan.drops[message_index];
                if duplicates.is_empty() {
                    continue;
                }
                let Some(tool_uses) = &mut message.assistant_response_message.tool_uses else {
                    continue;
                };
                let mut index = 0;
                tool_uses.retain(|_| {
                    let keep = !duplicates.contains(&index);
                    index += 1;
                    keep
                });
            }
            Message::User(message) => {
                let drops = &result_drops[message_index];
                if drops.is_empty() {
                    continue;
                }
                let mut index = 0;
                message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                    .retain(|_| {
                        let keep = !drops.contains(&index);
                        index += 1;
                        keep
                    });
            }
        }
    }

    // 走到这里，history 与 current_results 中残留的 id 一定都在映射表里。
    // 仍用 `get` 而不是索引：宁可漏改一个 id 让下游配对检查兜住，也不要 panic 掉整个请求。
    for message in history.iter_mut() {
        match message {
            Message::Assistant(message) => {
                if let Some(tool_uses) = &mut message.assistant_response_message.tool_uses {
                    for tool_use in tool_uses {
                        if let Some(normalized) = original_to_normalized.get(&tool_use.tool_use_id)
                        {
                            tool_use.tool_use_id = normalized.clone();
                        }
                    }
                }
            }
            Message::User(message) => {
                for result in &mut message
                    .user_input_message
                    .user_input_message_context
                    .tool_results
                {
                    if let Some(normalized) = original_to_normalized.get(&result.tool_use_id) {
                        result.tool_use_id = normalized.clone();
                    }
                }
            }
        }
    }
    for result in current_results.iter_mut() {
        if let Some(normalized) = original_to_normalized.get(&result.tool_use_id) {
            result.tool_use_id = normalized.clone();
        }
    }

    Ok(ToolIdNormalization {
        rewritten_ids,
        deduplicated_tool_uses: plan.identical,
        dropped_conflicting_tool_uses: plan.conflicting,
        dropped_orphan_results,
        dropped_duplicate_results,
    })
}

struct ToolUseDedupPlan {
    /// 每条消息里要丢弃的 tool_use 下标。
    drops: Vec<Vec<usize>>,
    /// name/input 完全一致的重复数（无损）。
    identical: usize,
    /// 同 id 但 name/input 不同的重复数（有损）。
    conflicting: usize,
}

/// 规划 tool_use 去重：**整段历史范围内**同一个 id 只保留首次出现。
///
/// 早期实现只在单条消息内去重，且遇到"同 id 不同内容"就整轮报错。跨消息重复
/// （客户端重放了一段历史）同样会报错。两种情况现在都统一成"首个胜出"：
/// 后面的重复被丢掉，它们的 tool_result 随之变成孤立项，由上面的逻辑一并清掉。
fn plan_tool_use_dedup(history: &[Message]) -> ToolUseDedupPlan {
    let mut drops = Vec::with_capacity(history.len());
    let mut seen = HashMap::<&str, (&str, &serde_json::Value)>::new();
    let mut identical = 0usize;
    let mut conflicting = 0usize;

    for message in history {
        let Message::Assistant(message) = message else {
            drops.push(Vec::new());
            continue;
        };
        let Some(tool_uses) = &message.assistant_response_message.tool_uses else {
            drops.push(Vec::new());
            continue;
        };
        let mut message_drops = Vec::new();
        for (index, tool_use) in tool_uses.iter().enumerate() {
            match seen.get(tool_use.tool_use_id.as_str()) {
                Some(&(name, input)) => {
                    if name == tool_use.name.as_str() && input == &tool_use.input {
                        identical += 1;
                    } else {
                        conflicting += 1;
                    }
                    message_drops.push(index);
                }
                None => {
                    seen.insert(
                        tool_use.tool_use_id.as_str(),
                        (tool_use.name.as_str(), &tool_use.input),
                    );
                }
            }
        }
        drops.push(message_drops);
    }

    ToolUseDedupPlan {
        drops,
        identical,
        conflicting,
    }
}

fn classify_result_id(
    id: &str,
    original_to_normalized: &HashMap<String, String>,
    outstanding_tool_uses: &mut HashSet<String>,
    seen_results: &mut HashSet<String>,
) -> ResultDisposition {
    if !original_to_normalized.contains_key(id) {
        return ResultDisposition::DropOrphan;
    }
    if !seen_results.insert(id.to_owned()) {
        return ResultDisposition::DropDuplicate;
    }
    if !outstanding_tool_uses.remove(id) {
        return ResultDisposition::DropDuplicate;
    }
    ResultDisposition::Keep
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

        let error = normalize_tool_history_ids(&mut history, &mut Vec::new()).unwrap_err();

        assert_eq!(error, ToolHistoryError::AmbiguousNormalizedId(normalized));
    }

    #[test]
    fn drops_same_message_duplicate_id_with_different_name_or_input() {
        for second in [
            ToolUseEntry::new("duplicate:1", "other_tool")
                .with_input(serde_json::json!({"city": "Paris"})),
            ToolUseEntry::new("duplicate:1", "get_weather")
                .with_input(serde_json::json!({"city": "London"})),
        ] {
            let first = ToolUseEntry::new("duplicate:1", "get_weather")
                .with_input(serde_json::json!({"city": "Paris"}));
            let mut history = vec![Message::Assistant(HistoryAssistantMessage {
                assistant_response_message: AssistantMessage::new("calling tool")
                    .with_tool_uses(vec![first, second]),
            })];

            let report = normalize_tool_history_ids(&mut history, &mut Vec::new())
                .expect("conflicting duplicate must not fail the whole turn");

            assert_eq!(report.dropped_conflicting_tool_uses, 1);
            assert_eq!(report.deduplicated_tool_uses, 0);
            let Message::Assistant(message) = &history[0] else {
                panic!("expected assistant message");
            };
            let tool_uses = message
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("tool uses");
            assert_eq!(tool_uses.len(), 1, "first occurrence wins");
            assert_eq!(tool_uses[0].name, "get_weather");
            assert_eq!(tool_uses[0].input, serde_json::json!({"city": "Paris"}));
        }
    }

    #[test]
    fn deduplicates_identical_tool_uses_within_one_assistant_message() {
        let tool_use = ToolUseEntry::new("duplicate:1", "get_weather")
            .with_input(serde_json::json!({"city": "Paris"}));
        let mut history = vec![Message::Assistant(HistoryAssistantMessage {
            assistant_response_message: AssistantMessage::new("calling tool")
                .with_tool_uses(vec![tool_use.clone(), tool_use]),
        })];
        let mut current = vec![ToolResult::success("duplicate:1", "sunny")];

        let report = normalize_tool_history_ids(&mut history, &mut current)
            .expect("identical duplicate should be repaired");

        let Message::Assistant(message) = &history[0] else {
            panic!("expected assistant message");
        };
        assert_eq!(
            message
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("tool uses")
                .len(),
            1
        );
        assert_eq!(report.deduplicated_tool_uses, 1);
        assert!(report.rewritten_ids.contains_key("duplicate:1"));
        assert_eq!(current[0].tool_use_id, tool_use_id(&history[0], 0));
    }

    #[test]
    fn drops_identical_tool_use_id_reused_across_assistant_messages() {
        let mut history = vec![
            assistant_with_tool_uses(&["duplicate:1"]),
            assistant_with_tool_uses(&["duplicate:1"]),
        ];

        let report = normalize_tool_history_ids(&mut history, &mut Vec::new())
            .expect("cross-message duplicate must not fail the whole turn");

        assert_eq!(report.deduplicated_tool_uses, 1);
        let Message::Assistant(second) = &history[1] else {
            panic!("expected assistant message");
        };
        assert!(
            second
                .assistant_response_message
                .tool_uses
                .as_ref()
                .expect("tool uses")
                .is_empty(),
            "later duplicate is dropped, first occurrence kept"
        );
    }

    #[test]
    fn drops_duplicate_tool_result_ids_across_history_and_current_message() {
        let mut history = vec![
            assistant_with_tool_uses(&["a:b"]),
            user_with_tool_results(&["a:b"]),
        ];
        let mut current = vec![ToolResult::success("a:b", "duplicate")];

        let report = normalize_tool_history_ids(&mut history, &mut current)
            .expect("duplicate result must not fail the whole turn");

        assert_eq!(report.dropped_duplicate_results, 1);
        assert!(current.is_empty(), "the second result is dropped");
        // 历史里那条先到的结果保留，配对仍然完整。
        assert_eq!(
            historical_result_id(&history[1], 0),
            tool_use_id(&history[0], 0)
        );
    }

    #[test]
    fn drops_orphaned_historical_and_current_results() {
        let mut historical = vec![user_with_tool_results(&["missing.history:1"])];
        let report = normalize_tool_history_ids(&mut historical, &mut Vec::new())
            .expect("orphan result must not fail the whole turn");
        assert_eq!(report.dropped_orphan_results, 1);
        let Message::User(message) = &historical[0] else {
            panic!("expected user message");
        };
        assert!(
            message
                .user_input_message
                .user_input_message_context
                .tool_results
                .is_empty()
        );

        let mut current_history = vec![assistant_with_tool_uses(&["known:1"])];
        let mut current = vec![ToolResult::success("missing.current:1", "orphan")];
        let report = normalize_tool_history_ids(&mut current_history, &mut current)
            .expect("orphan result must not fail the whole turn");
        assert_eq!(report.dropped_orphan_results, 1);
        assert!(current.is_empty());
    }

    #[test]
    fn drops_tool_result_that_precedes_its_tool_use() {
        let mut history = vec![
            user_with_tool_results(&["future:1"]),
            assistant_with_tool_uses(&["future:1"]),
        ];

        let report = normalize_tool_history_ids(&mut history, &mut Vec::new())
            .expect("out-of-order result must not fail the whole turn");

        assert_eq!(report.dropped_orphan_results, 1);
        let Message::User(message) = &history[0] else {
            panic!("expected user message");
        };
        assert!(
            message
                .user_input_message
                .user_input_message_context
                .tool_results
                .is_empty(),
            "the early result is dropped; the tool_use itself survives for downstream pairing"
        );
    }

    /// 线上最高频的一类：经 OpenAI ↔ Anthropic 转换后 tool_use 丢失，只剩 tool_result。
    /// 过去整轮 400，客户端带着同一份坏历史无限重试，会话永久卡死。
    #[test]
    fn keeps_healthy_pairs_while_dropping_broken_ones() {
        let mut history = vec![
            assistant_with_tool_uses(&["good:1"]),
            user_with_tool_results(&["good:1", "call_lost_in_translation"]),
        ];
        let mut current = vec![];

        let report = normalize_tool_history_ids(&mut history, &mut current)
            .expect("a broken pair must not take the healthy ones down with it");

        assert_eq!(report.dropped_orphan_results, 1);
        let Message::User(message) = &history[1] else {
            panic!("expected user message");
        };
        let results = &message
            .user_input_message
            .user_input_message_context
            .tool_results;
        assert_eq!(results.len(), 1, "the healthy pair survives");
        assert_eq!(results[0].tool_use_id, tool_use_id(&history[0], 0));
    }
}
