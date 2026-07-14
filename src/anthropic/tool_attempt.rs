use super::stream::{SseEvent, ToolJsonAccumulatorError, ToolJsonAccumulatorError::IncompleteJson};
use crate::kiro::model::events::Event;
use std::collections::HashMap;
use std::future::Future;

const MAX_UPSTREAM_ERROR_MESSAGE_CHARS: usize = 512;

fn bounded_upstream_error_message(message: &str) -> String {
    message
        .chars()
        .take(MAX_UPSTREAM_ERROR_MESSAGE_CHARS)
        .collect()
}

/// 实时 SSE 的试运行缓冲。
///
/// `message_start` 和空 content delta 会暂存在内存中；首个非空文本、thinking、
/// redacted thinking 或完整工具块出现时，缓冲与该事件一起原子提交。提交之后任何失败
/// 都不允许透明重试，防止重复文本或重复工具执行。
#[derive(Debug, Default)]
pub(crate) struct ProbationBuffer {
    pending: Vec<SseEvent>,
    committed: bool,
    tool_forwarded: bool,
    pending_tools: HashMap<i64, bool>,
}

impl ProbationBuffer {
    pub(crate) fn push(&mut self, event: SseEvent) -> Vec<SseEvent> {
        if self.committed {
            return vec![event];
        }

        let complete_tool = self.observe_tool_event(&event);
        let semantic_output = complete_tool || event_has_semantic_output(&event);
        self.tool_forwarded |= complete_tool;
        self.pending.push(event);

        if semantic_output {
            self.committed = true;
            return std::mem::take(&mut self.pending);
        }
        Vec::new()
    }

    pub(crate) fn push_all(&mut self, events: Vec<SseEvent>) -> Vec<SseEvent> {
        let mut output = Vec::new();
        for event in events {
            output.extend(self.push(event));
        }
        output
    }

    #[cfg(test)]
    pub(crate) fn committed(&self) -> bool {
        self.committed
    }

    #[cfg(test)]
    pub(crate) fn tool_forwarded(&self) -> bool {
        self.tool_forwarded
    }

    pub(crate) fn take_pending(&mut self) -> Vec<SseEvent> {
        std::mem::take(&mut self.pending)
    }

    fn observe_tool_event(&mut self, event: &SseEvent) -> bool {
        let Some(index) = event.data.get("index").and_then(serde_json::Value::as_i64) else {
            return false;
        };
        if event.event == "content_block_start"
            && event
                .data
                .pointer("/content_block/type")
                .and_then(serde_json::Value::as_str)
                == Some("tool_use")
        {
            self.pending_tools.insert(index, false);
            return false;
        }
        if event.event == "content_block_delta"
            && event
                .data
                .pointer("/delta/type")
                .and_then(serde_json::Value::as_str)
                == Some("input_json_delta")
        {
            if let Some(has_json) = self.pending_tools.get_mut(&index) {
                *has_json = true;
            }
            return false;
        }
        if event.event == "content_block_stop" {
            return self.pending_tools.remove(&index).unwrap_or(false);
        }
        false
    }

    /// 若失败仍处于首轮未提交窗口，丢弃该轮缓冲并允许重试一次。
    #[cfg(test)]
    pub(crate) fn prepare_retry(
        &mut self,
        attempt_index: u8,
        error: &ToolJsonAccumulatorError,
    ) -> bool {
        self.prepare_attempt_retry(
            attempt_index,
            AttemptTermination::Eof,
            Some(AttemptFailure::IncompleteToolJson(error.clone())),
        )
    }

    pub(crate) fn prepare_attempt_retry(
        &mut self,
        attempt_index: u8,
        termination: AttemptTermination,
        failure: Option<AttemptFailure>,
    ) -> bool {
        if self.should_retry_attempt(attempt_index, termination, failure) {
            self.pending.clear();
            self.pending_tools.clear();
            return true;
        }
        false
    }

    pub(crate) fn should_retry_attempt(
        &self,
        attempt_index: u8,
        termination: AttemptTermination,
        failure: Option<AttemptFailure>,
    ) -> bool {
        let state = ToolAttemptState {
            attempt_index,
            termination,
            failure,
            semantic_output_started: self.committed,
            tool_forwarded: self.tool_forwarded,
        };
        state.should_retry()
    }
}

fn event_has_semantic_output(event: &SseEvent) -> bool {
    if event.event == "content_block_start" {
        return event
            .data
            .pointer("/content_block/type")
            .and_then(serde_json::Value::as_str)
            == Some("redacted_thinking");
    }
    if event.event != "content_block_delta" {
        return false;
    }
    match event
        .data
        .pointer("/delta/type")
        .and_then(serde_json::Value::as_str)
    {
        Some("text_delta") => event
            .data
            .pointer("/delta/text")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|text| !text.is_empty()),
        Some("thinking_delta") => event
            .data
            .pointer("/delta/thinking")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|thinking| !thinking.is_empty()),
        // 工具参数 delta 只有在同一 tool_use 块 stop 后才算完整；单个 partial_json
        // 绝不能开启提交窗口，否则 EOF 半截会被误认为已向客户端提交。
        Some("input_json_delta") => false,
        _ => false,
    }
}

/// 上游 attempt 的读取终止方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptTermination {
    Eof,
    ReadError(String),
    IdleTimeout,
    ClientClosed,
}

/// 上游 attempt 在正常收尾后得到的语义失败分类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AttemptFailure {
    IncompleteToolJson(ToolJsonAccumulatorError),
    InvalidToolSchema {
        failure: super::tool_schema::ToolSchemaFailure,
    },
    EmptyResponse,
    ContextWindowExceeded,
    UpstreamError {
        error_type: String,
        message: String,
    },
}

impl AttemptFailure {
    /// 映射为稳定的客户端错误；显式上游异常不回显其正文。
    pub(crate) fn public_error(&self) -> (&'static str, String) {
        match self {
            Self::IncompleteToolJson(error) => (error.error_type(), error.message()),
            Self::InvalidToolSchema { failure } => {
                ("upstream_tool_schema_error", failure.public_message())
            }
            Self::EmptyResponse => (
                "upstream_empty_response",
                "Upstream returned no assistant content after one retry".to_string(),
            ),
            Self::ContextWindowExceeded => (
                "upstream_context_window_exceeded",
                "Upstream context window was exceeded".to_string(),
            ),
            Self::UpstreamError { error_type, .. } => {
                let safe_type = error_type
                    .chars()
                    .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.'))
                    .take(64)
                    .collect::<String>();
                let safe_type = if safe_type.is_empty() {
                    "unknown upstream error"
                } else {
                    safe_type.as_str()
                };
                (
                    "upstream_protocol_error",
                    format!("Upstream reported {safe_type}"),
                )
            }
        }
    }
}

/// 从 AWS event-stream 中累积本轮 attempt 的语义与显式失败信号。
#[derive(Debug, Default)]
pub(crate) struct AttemptObservation {
    saw_frame: bool,
    semantic_output_started: bool,
    context_window_exceeded: bool,
    upstream_error: Option<(String, String)>,
}

impl AttemptObservation {
    pub(crate) fn observe(&mut self, event: &Event) {
        self.saw_frame = true;
        match event {
            Event::AssistantResponse(response) => {
                self.semantic_output_started |= !response.content.is_empty();
            }
            Event::ReasoningContent(reasoning) => {
                self.semantic_output_started |= reasoning
                    .text
                    .as_deref()
                    .is_some_and(|text| !text.is_empty())
                    || reasoning
                        .redacted_content
                        .as_deref()
                        .is_some_and(|content| !content.is_empty());
            }
            Event::ContextUsage(context_usage) => {
                self.context_window_exceeded |= context_usage.context_usage_percentage >= 100.0;
            }
            Event::Error {
                error_code,
                error_message,
            } => {
                self.upstream_error.get_or_insert_with(|| {
                    (
                        error_code.clone(),
                        bounded_upstream_error_message(error_message),
                    )
                });
            }
            Event::Exception {
                exception_type,
                message,
            } => {
                if exception_type != "ContentLengthExceededException" {
                    self.upstream_error.get_or_insert_with(|| {
                        (
                            exception_type.clone(),
                            bounded_upstream_error_message(message),
                        )
                    });
                }
            }
            _ => {}
        }
    }

    pub(crate) fn saw_frame(&self) -> bool {
        self.saw_frame
    }

    pub(crate) fn failure(
        &self,
        tool_json_error: Option<ToolJsonAccumulatorError>,
        tool_semantic_output: bool,
    ) -> Option<AttemptFailure> {
        if let Some(error) = tool_json_error {
            return Some(AttemptFailure::IncompleteToolJson(error));
        }
        if self.context_window_exceeded {
            return Some(AttemptFailure::ContextWindowExceeded);
        }
        if let Some((error_type, message)) = &self.upstream_error {
            return Some(AttemptFailure::UpstreamError {
                error_type: error_type.clone(),
                message: message.clone(),
            });
        }
        if !self.semantic_output_started && !tool_semantic_output {
            return Some(AttemptFailure::EmptyResponse);
        }
        None
    }
}

/// 单次上游工具生成 attempt 的提交状态。
///
/// 只有第一次、尚未向客户端提交任何语义内容或工具调用、并且正常 EOF 后得到纯空响应
/// 、半截工具 JSON或未交付的 Schema 错误时才能透明重试。非法 JSON 与已经提交的输出都
/// 必须原样失败，防止重复执行工具。
#[derive(Debug, Clone)]
pub(crate) struct ToolAttemptState {
    pub attempt_index: u8,
    pub termination: AttemptTermination,
    pub failure: Option<AttemptFailure>,
    pub semantic_output_started: bool,
    pub tool_forwarded: bool,
}

impl ToolAttemptState {
    pub(crate) fn should_retry(&self) -> bool {
        let retryable_failure = match &self.failure {
            Some(AttemptFailure::EmptyResponse)
            | Some(AttemptFailure::IncompleteToolJson(IncompleteJson { .. })) => true,
            Some(AttemptFailure::InvalidToolSchema { failure }) => {
                failure.can_retry_with_description()
            }
            _ => false,
        };
        self.attempt_index == 0
            && self.termination == AttemptTermination::Eof
            && !self.semantic_output_started
            && !self.tool_forwarded
            && retryable_failure
    }
}

/// 执行一次上游 attempt，并且只在第一次结果满足 [`ToolAttemptState::should_retry`]
/// 时再执行一次。第二次结果无条件返回，避免无界重试。
pub(crate) async fn run_with_single_retry<T, E, Collect, CollectFuture, StateOf>(
    mut collect: Collect,
    state_of: StateOf,
) -> Result<(T, u8), E>
where
    Collect: FnMut(u8) -> CollectFuture,
    CollectFuture: Future<Output = Result<T, E>>,
    StateOf: Fn(&T, u8) -> ToolAttemptState,
{
    for attempt_index in 0_u8..=1 {
        let value = collect(attempt_index).await?;
        let state = state_of(&value, attempt_index);
        if state.should_retry() {
            continue;
        }
        return Ok((value, attempt_index + 1));
    }
    unreachable!("第二次 attempt 不允许继续重试")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::stream::{SseEvent, ToolJsonAccumulatorError};
    use crate::kiro::model::events::{AssistantResponseEvent, Event, ToolUseEvent};
    use serde_json::json;
    use std::collections::VecDeque;

    fn incomplete() -> ToolJsonAccumulatorError {
        ToolJsonAccumulatorError::IncompleteJson {
            tool_use_id: "tool_1".to_string(),
            name: "fs_write".to_string(),
            bytes: 56,
        }
    }

    fn empty_attempt() -> ToolAttemptState {
        ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::EmptyResponse),
            semantic_output_started: false,
            tool_forwarded: false,
        }
    }

    fn schema_failure() -> super::super::tool_schema::ToolSchemaFailure {
        super::super::tool_schema::ToolSchemaFailure::from_error_and_input(
            super::super::tool_schema::ToolSchemaError {
                tool_name: "get_weather".to_string(),
                violations: vec![
                    super::super::tool_schema::ToolInputViolation::MissingRequired(
                        "$.city".to_string(),
                    ),
                ],
            },
            &serde_json::json!({}),
        )
    }

    #[test]
    fn invalid_tool_schema_retries_only_before_semantic_output_and_only_once() {
        let retryable = ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::InvalidToolSchema {
                failure: schema_failure(),
            }),
            semantic_output_started: false,
            tool_forwarded: false,
        };
        assert!(retryable.should_retry());

        let second = ToolAttemptState {
            attempt_index: 1,
            ..retryable.clone()
        };
        assert!(!second.should_retry());

        let after_text = ToolAttemptState {
            semantic_output_started: true,
            ..retryable
        };
        assert!(!after_text.should_retry());

        let undeclared = ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::InvalidToolSchema {
                failure: super::super::tool_schema::ToolSchemaFailure::from_error_and_input(
                    super::super::tool_schema::ToolSchemaError {
                        tool_name: "undeclared".to_string(),
                        violations: vec![
                            super::super::tool_schema::ToolInputViolation::UndeclaredTool,
                        ],
                    },
                    &serde_json::json!({}),
                ),
            }),
            semantic_output_started: false,
            tool_forwarded: false,
        };
        assert!(!undeclared.should_retry());
    }

    #[test]
    fn retries_only_first_normal_eof_empty_or_incomplete_attempt() {
        let empty = empty_attempt();
        assert!(empty.should_retry());
        assert!(
            ToolAttemptState {
                failure: Some(AttemptFailure::IncompleteToolJson(incomplete())),
                ..empty.clone()
            }
            .should_retry()
        );

        assert!(
            !ToolAttemptState {
                attempt_index: 1,
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                termination: AttemptTermination::ReadError("connection reset".into()),
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                termination: AttemptTermination::IdleTimeout,
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                termination: AttemptTermination::ClientClosed,
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                semantic_output_started: true,
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                tool_forwarded: true,
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                failure: Some(AttemptFailure::ContextWindowExceeded),
                ..empty.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                failure: Some(AttemptFailure::UpstreamError {
                    error_type: "ValidationException".into(),
                    message: "invalid request".into(),
                }),
                ..empty
            }
            .should_retry()
        );
    }

    #[test]
    fn attempt_failures_use_stable_public_errors_without_upstream_body() {
        let (error_type, message) = AttemptFailure::EmptyResponse.public_error();
        assert_eq!(error_type, "upstream_empty_response");
        assert_eq!(
            message,
            "Upstream returned no assistant content after one retry"
        );

        let sensitive = "request body: secret customer document";
        let (error_type, message) = AttemptFailure::UpstreamError {
            error_type: "ValidationException".into(),
            message: sensitive.into(),
        }
        .public_error();
        assert_eq!(error_type, "upstream_protocol_error");
        assert!(message.contains("ValidationException"));
        assert!(!message.contains(sensitive));
        assert!(!message.contains("secret customer document"));
    }

    #[test]
    fn content_length_exception_keeps_existing_max_tokens_semantics() {
        let mut observation = AttemptObservation::default();
        let mut response = AssistantResponseEvent::default();
        response.content = "partial output".into();
        observation.observe(&Event::AssistantResponse(response));
        observation.observe(&Event::Exception {
            exception_type: "ContentLengthExceededException".into(),
            message: "output limit reached".into(),
        });

        assert_eq!(observation.failure(None, false), None);
    }

    #[test]
    fn upstream_error_detail_is_bounded_before_storage() {
        let mut observation = AttemptObservation::default();
        observation.observe(&Event::Exception {
            exception_type: "ModelError".into(),
            message: "敏".repeat(1000),
        });

        let Some(AttemptFailure::UpstreamError { message, .. }) = observation.failure(None, false)
        else {
            panic!("expected upstream failure");
        };
        assert_eq!(message.chars().count(), 512);
        assert!(message.is_char_boundary(message.len()));
    }

    #[test]
    fn retries_only_first_incomplete_uncommitted_attempt() {
        let retryable = ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::IncompleteToolJson(incomplete())),
            semantic_output_started: false,
            tool_forwarded: false,
        };

        assert!(retryable.should_retry());
        assert!(
            !ToolAttemptState {
                attempt_index: 1,
                ..retryable.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                semantic_output_started: true,
                ..retryable.clone()
            }
            .should_retry()
        );
        assert!(
            !ToolAttemptState {
                tool_forwarded: true,
                ..retryable
            }
            .should_retry()
        );
    }

    #[test]
    fn invalid_json_is_never_retryable() {
        let state = ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::IncompleteToolJson(
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: "tool_1".to_string(),
                    name: "fs_write".to_string(),
                    message: "expected value".to_string(),
                },
            )),
            semantic_output_started: false,
            tool_forwarded: false,
        };
        assert!(!state.should_retry());
    }

    #[tokio::test]
    async fn run_with_single_retry_returns_second_attempt_after_retryable_first() {
        let mut attempts = VecDeque::from([
            ToolAttemptState {
                attempt_index: 0,
                termination: AttemptTermination::Eof,
                failure: Some(AttemptFailure::IncompleteToolJson(incomplete())),
                semantic_output_started: false,
                tool_forwarded: false,
            },
            ToolAttemptState {
                attempt_index: 1,
                termination: AttemptTermination::Eof,
                failure: None,
                semantic_output_started: false,
                tool_forwarded: false,
            },
        ]);

        let (state, attempt_count) = run_with_single_retry(
            |_| std::future::ready(Ok::<_, ()>(attempts.pop_front().unwrap())),
            |state, _| state.clone(),
        )
        .await
        .unwrap();

        assert!(state.failure.is_none());
        assert_eq!(attempt_count, 2);
    }

    #[tokio::test]
    async fn second_schema_failure_is_terminal_and_never_retried_a_third_time() {
        let calls = std::cell::Cell::new(0_u8);

        let (state, attempt_count) = run_with_single_retry(
            |attempt_index| {
                calls.set(calls.get() + 1);
                std::future::ready(Ok::<_, ()>(ToolAttemptState {
                    attempt_index,
                    termination: AttemptTermination::Eof,
                    failure: Some(AttemptFailure::InvalidToolSchema {
                        failure: schema_failure(),
                    }),
                    semantic_output_started: false,
                    tool_forwarded: false,
                }))
            },
            |state, _| state.clone(),
        )
        .await
        .unwrap();

        assert_eq!(calls.get(), 2);
        assert_eq!(attempt_count, 2);
        let failure = state.failure.expect("second schema failure");
        assert_eq!(failure.public_error().0, "upstream_tool_schema_error");
    }

    #[tokio::test]
    async fn run_with_single_retry_stops_after_invalid_first_attempt() {
        let invalid = ToolAttemptState {
            attempt_index: 0,
            termination: AttemptTermination::Eof,
            failure: Some(AttemptFailure::IncompleteToolJson(
                ToolJsonAccumulatorError::InvalidJson {
                    tool_use_id: "tool_1".to_string(),
                    name: "fs_write".to_string(),
                    message: "expected value".to_string(),
                },
            )),
            semantic_output_started: false,
            tool_forwarded: false,
        };
        let calls = std::cell::Cell::new(0_u8);

        let (_, attempt_count) = run_with_single_retry(
            |_| {
                calls.set(calls.get() + 1);
                std::future::ready(Ok::<_, ()>(invalid.clone()))
            },
            |state, _| state.clone(),
        )
        .await
        .unwrap();

        assert_eq!(calls.get(), 1);
        assert_eq!(attempt_count, 1);
    }

    fn message_start() -> SseEvent {
        SseEvent::new("message_start", json!({"type": "message_start"}))
    }

    fn text_delta(text: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({"delta": {"type": "text_delta", "text": text}}),
        )
    }

    fn tool_start(id: &str) -> SseEvent {
        SseEvent::new(
            "content_block_start",
            json!({
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": id,
                    "name": "fs_write",
                    "input": {}
                }
            }),
        )
    }

    fn tool_delta(index: i32, json: &str) -> SseEvent {
        SseEvent::new(
            "content_block_delta",
            json!({
                "index": index,
                "delta": {"type": "input_json_delta", "partial_json": json}
            }),
        )
    }

    fn block_stop(index: i32) -> SseEvent {
        SseEvent::new(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": index}),
        )
    }

    fn error_event() -> SseEvent {
        SseEvent::new(
            "error",
            json!({
                "type": "error",
                "error": {"type": "upstream_tool_json_error", "message": "incomplete"}
            }),
        )
    }

    fn kiro_tool(id: &str, input: &str, stop: bool) -> Event {
        Event::ToolUse(ToolUseEvent {
            name: "custom_tool".to_string(),
            tool_use_id: id.to_string(),
            input: input.to_string(),
            stop,
        })
    }

    fn stream_context() -> crate::anthropic::stream::StreamContext {
        crate::anthropic::stream::StreamContext::new_with_thinking(
            "claude-test",
            10,
            false,
            std::collections::HashMap::new(),
            std::collections::HashSet::new(),
        )
    }

    #[test]
    fn realtime_probation_discards_first_incomplete_attempt_before_commit() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());

        assert!(buffer.prepare_retry(0, &incomplete()));
        assert!(buffer.take_pending().is_empty());
        assert!(!buffer.committed());
    }

    #[test]
    fn realtime_retry_clears_partial_tool_tracking() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());
        assert!(buffer.push(tool_start("tool_1")).is_empty());
        assert!(buffer.push(tool_delta(0, r#"{"path":"/tmp"#)).is_empty());

        assert!(buffer.prepare_retry(0, &incomplete()));
        assert!(buffer.push(block_stop(0)).is_empty());
        assert!(!buffer.committed());
        assert!(!buffer.tool_forwarded());
    }

    #[test]
    fn realtime_probation_commits_after_first_text_and_never_retries() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());

        let committed = buffer.push(text_delta("hello"));
        assert_eq!(committed.len(), 2);
        assert_eq!(committed[0].event, "message_start");
        assert_eq!(committed[1].data["delta"]["text"], "hello");
        assert!(buffer.committed());
        assert!(!buffer.prepare_retry(0, &incomplete()));
    }

    #[test]
    fn realtime_probation_commits_complete_tool_and_never_retries() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());

        assert!(buffer.push(tool_start("tool_1")).is_empty());
        assert!(
            buffer
                .push(tool_delta(0, r#"{"path":"/tmp/a"}"#))
                .is_empty()
        );
        let committed = buffer.push(block_stop(0));
        assert_eq!(committed.len(), 4);
        assert!(buffer.tool_forwarded());
        assert!(!buffer.prepare_retry(0, &incomplete()));
    }

    #[test]
    fn realtime_probation_does_not_commit_partial_delta_or_retry_invalid_json() {
        let mut partial = ProbationBuffer::default();
        assert!(partial.push(message_start()).is_empty());
        assert!(partial.push(tool_delta(0, r#"{"path":"/tmp"#)).is_empty());
        assert!(!partial.committed(), "partial_json 本身不得算语义提交");
        let invalid = ToolJsonAccumulatorError::InvalidJson {
            tool_use_id: "tool_1".to_string(),
            name: "fs_write".to_string(),
            message: "expected value".to_string(),
        };
        let mut invalid_attempt = ProbationBuffer::default();
        assert!(invalid_attempt.push(message_start()).is_empty());
        assert!(!invalid_attempt.prepare_retry(0, &invalid));
    }

    #[test]
    fn realtime_empty_deltas_are_not_semantic_but_redacted_thinking_is() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());
        assert!(buffer.push(text_delta("")).is_empty());
        assert!(
            buffer
                .push(SseEvent::new(
                    "content_block_delta",
                    json!({"delta": {"type": "thinking_delta", "thinking": ""}}),
                ))
                .is_empty()
        );
        assert!(!buffer.committed());

        let visible = buffer.push(SseEvent::new(
            "content_block_start",
            json!({
                "index": 0,
                "content_block": {"type": "redacted_thinking", "data": "ciphertext"}
            }),
        ));
        assert_eq!(visible.len(), 4);
        assert!(buffer.committed());
    }

    #[test]
    fn realtime_second_incomplete_flushes_standard_error_without_success_terminal() {
        let mut second = ProbationBuffer::default();
        assert!(second.push(message_start()).is_empty());
        assert!(second.push(error_event()).is_empty());
        assert!(!second.prepare_retry(1, &incomplete()));

        let visible = second.take_pending();
        assert_eq!(
            visible
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
        assert!(visible.iter().any(|event| event.event == "error"));
        assert!(!visible.iter().any(|event| event.event == "message_delta"));
        assert!(!visible.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn buffered_retry_discards_first_attempt_and_exposes_only_second_result() {
        let mut first = ProbationBuffer::default();
        assert!(first.push(message_start()).is_empty());
        assert!(first.push(error_event()).is_empty());
        assert!(first.prepare_retry(0, &incomplete()));

        let mut second = ProbationBuffer::default();
        assert!(second.push(message_start()).is_empty());
        let visible = second.push(text_delta("recovered"));
        assert_eq!(
            visible
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
        assert_eq!(visible.last().unwrap().data["delta"]["text"], "recovered");
    }

    #[test]
    fn early_stream_comments_do_not_commit_probation() {
        let mut buffer = ProbationBuffer::default();
        assert!(buffer.push(message_start()).is_empty());
        assert!(!buffer.committed(), "握手 comment/ping 不进入语义事件缓冲");
        assert!(buffer.prepare_retry(0, &incomplete()));
    }

    #[test]
    fn realtime_converter_first_incomplete_second_complete_exposes_one_message_start() {
        let mut first_ctx = stream_context();
        let mut first = ProbationBuffer::default();
        assert!(
            first
                .push_all(first_ctx.generate_initial_events())
                .is_empty()
        );
        assert!(
            first
                .push_all(first_ctx.process_kiro_event(&kiro_tool(
                    "tool_1",
                    r#"{"path":"/tmp"#,
                    false,
                )))
                .is_empty()
        );
        assert!(first.push_all(first_ctx.generate_final_events()).is_empty());
        let first_error = first_ctx.terminal_tool_json_error().unwrap();
        assert!(first.prepare_retry(0, first_error));

        let mut second_ctx = stream_context();
        let mut second = ProbationBuffer::default();
        let mut visible = second.push_all(second_ctx.generate_initial_events());
        visible.extend(second.push_all(second_ctx.process_kiro_event(&kiro_tool(
            "tool_2",
            r#"{"path":"/tmp/a"}"#,
            true,
        ))));
        visible.extend(second.push_all(second_ctx.generate_final_events()));
        visible.extend(second.take_pending());

        assert!(second_ctx.terminal_tool_json_error().is_none());
        assert_eq!(
            visible
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
        assert!(visible.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
        assert!(visible.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn realtime_converter_text_before_incomplete_never_retries() {
        let mut ctx = stream_context();
        let mut probation = ProbationBuffer::default();
        assert!(probation.push_all(ctx.generate_initial_events()).is_empty());
        let mut response = AssistantResponseEvent::default();
        response.content = "already visible".to_string();
        assert!(
            !probation
                .push_all(ctx.process_kiro_event(&Event::AssistantResponse(response)))
                .is_empty()
        );
        assert!(
            probation
                .push_all(ctx.process_kiro_event(&kiro_tool("tool_1", r#"{"path":"/tmp"#, false,)))
                .is_empty()
        );
        let _ = probation.push_all(ctx.generate_final_events());
        assert!(!probation.prepare_retry(0, ctx.terminal_tool_json_error().unwrap()));
    }

    #[test]
    fn realtime_converter_complete_tool_before_second_incomplete_never_retries() {
        let mut ctx = stream_context();
        let mut probation = ProbationBuffer::default();
        assert!(probation.push_all(ctx.generate_initial_events()).is_empty());
        assert!(
            !probation
                .push_all(ctx.process_kiro_event(&kiro_tool(
                    "tool_1",
                    r#"{"path":"/tmp/a"}"#,
                    true,
                )))
                .is_empty()
        );
        assert!(
            probation
                .push_all(ctx.process_kiro_event(&kiro_tool("tool_2", r#"{"path":"/tmp"#, false,)))
                .is_empty()
        );
        let _ = probation.push_all(ctx.generate_final_events());
        assert!(!probation.prepare_retry(0, ctx.terminal_tool_json_error().unwrap()));
    }

    #[test]
    fn realtime_converter_second_incomplete_emits_error_without_success_terminal() {
        let mut ctx = stream_context();
        let mut probation = ProbationBuffer::default();
        assert!(probation.push_all(ctx.generate_initial_events()).is_empty());
        assert!(
            probation
                .push_all(ctx.process_kiro_event(&kiro_tool("tool_2", r#"{"path":"/tmp"#, false,)))
                .is_empty()
        );
        assert!(probation.push_all(ctx.generate_final_events()).is_empty());
        assert!(!probation.prepare_retry(1, ctx.terminal_tool_json_error().unwrap()));
        let visible = probation.take_pending();

        assert_eq!(
            visible
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
        assert!(visible.iter().any(|event| event.event == "error"));
        assert!(!visible.iter().any(|event| event.event == "message_delta"));
        assert!(!visible.iter().any(|event| event.event == "message_stop"));
    }

    #[test]
    fn buffered_context_first_incomplete_second_complete_uses_same_retry_gate() {
        let new_context = || {
            crate::anthropic::stream::BufferedStreamContext::new(
                "claude-test",
                10,
                false,
                std::collections::HashMap::new(),
                std::collections::HashSet::new(),
            )
        };
        let mut first_ctx = new_context();
        first_ctx.process_and_buffer(&kiro_tool("tool_1", r#"{"path":"/tmp"#, false));
        let first_events = first_ctx.finish_and_get_all_events();
        let mut first = ProbationBuffer::default();
        assert!(first.push_all(first_events).is_empty());
        assert!(first.prepare_retry(0, first_ctx.terminal_tool_json_error().unwrap()));

        let mut second_ctx = new_context();
        second_ctx.process_and_buffer(&kiro_tool("tool_2", r#"{"path":"/tmp/a"}"#, true));
        let second_events = second_ctx.finish_and_get_all_events();
        let mut second = ProbationBuffer::default();
        let mut visible = second.push_all(second_events);
        visible.extend(second.take_pending());
        assert!(second_ctx.terminal_tool_json_error().is_none());
        assert_eq!(
            visible
                .iter()
                .filter(|event| event.event == "message_start")
                .count(),
            1
        );
    }
}
