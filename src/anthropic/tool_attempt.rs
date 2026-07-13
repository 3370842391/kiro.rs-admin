use super::stream::{ToolJsonAccumulatorError, ToolJsonAccumulatorError::IncompleteJson};
use std::future::Future;

/// 单次上游工具生成 attempt 的提交状态。
///
/// 只有第一次、尚未向客户端提交任何语义内容或工具调用、并且终态为 EOF 半截 JSON
/// 时才能透明重试。非法 JSON 与已经提交的输出都必须原样失败，防止重复执行工具。
#[derive(Debug, Clone)]
pub(crate) struct ToolAttemptState {
    pub attempt_index: u8,
    pub terminal_error: Option<ToolJsonAccumulatorError>,
    pub semantic_output_started: bool,
    pub tool_forwarded: bool,
}

impl ToolAttemptState {
    pub(crate) fn should_retry(&self) -> bool {
        self.attempt_index == 0
            && !self.semantic_output_started
            && !self.tool_forwarded
            && matches!(self.terminal_error, Some(IncompleteJson { .. }))
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
    use crate::anthropic::stream::ToolJsonAccumulatorError;
    use std::collections::VecDeque;

    fn incomplete() -> ToolJsonAccumulatorError {
        ToolJsonAccumulatorError::IncompleteJson {
            tool_use_id: "tool_1".to_string(),
            name: "fs_write".to_string(),
            bytes: 56,
        }
    }

    #[test]
    fn retries_only_first_incomplete_uncommitted_attempt() {
        let retryable = ToolAttemptState {
            attempt_index: 0,
            terminal_error: Some(incomplete()),
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
            terminal_error: Some(ToolJsonAccumulatorError::InvalidJson {
                tool_use_id: "tool_1".to_string(),
                name: "fs_write".to_string(),
                message: "expected value".to_string(),
            }),
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
                terminal_error: Some(incomplete()),
                semantic_output_started: false,
                tool_forwarded: false,
            },
            ToolAttemptState {
                attempt_index: 1,
                terminal_error: None,
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

        assert!(state.terminal_error.is_none());
        assert_eq!(attempt_count, 2);
    }

    #[tokio::test]
    async fn run_with_single_retry_stops_after_invalid_first_attempt() {
        let invalid = ToolAttemptState {
            attempt_index: 0,
            terminal_error: Some(ToolJsonAccumulatorError::InvalidJson {
                tool_use_id: "tool_1".to_string(),
                name: "fs_write".to_string(),
                message: "expected value".to_string(),
            }),
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
}
