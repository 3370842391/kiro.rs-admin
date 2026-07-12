//! Anthropic API Handler 函数

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceKeySource, TraceRecord, TraceSink, outcome,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::token;
use anyhow::Error;
use axum::{
    Json as JsonExtractor,
    body::Body,
    extract::{Extension, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use chrono::Utc;
use futures::{Stream, StreamExt, stream};
use serde_json::json;
use std::time::Duration;
use tokio::time::{Instant as TokioInstant, interval};
use uuid::Uuid;

use super::converter::{ConversionError, convert_request_with_mode};
use super::middleware::{AppState, KeyContext};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, Model, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

/// 请求结束时记录用量的钩子
///
/// 在 handler 入口构造，调用 [`Self::record`] 时把当次请求的 input/output token、
/// 命中的上游凭据 ID、状态写入：
/// - `usage_log.YYYY-MM-DD.jsonl`（持久化历史）
/// - 内存聚合器（仪表盘趋势）
/// - 客户端 Key 计数（按 Key 累计）
#[derive(Clone)]
pub(crate) struct UsageRecordHook {
    pub recorder: Option<SharedRecorder>,
    pub aggregator: Option<SharedAggregator>,
    pub client_keys: Option<SharedClientKeyManager>,
    pub key_id: u64,
    pub model: String,
    pub started_at: Instant,
}

impl UsageRecordHook {
    pub fn from_state(state: &AppState, key_id: u64, model: String) -> Self {
        Self {
            recorder: state.usage_recorder.clone(),
            aggregator: state.usage_aggregator.clone(),
            client_keys: state.client_keys.clone(),
            key_id,
            model,
            started_at: Instant::now(),
        }
    }

    pub fn record(
        &self,
        credential_id: u64,
        input_tokens: i32,
        output_tokens: i32,
        cache_creation_tokens: i32,
        cache_read_tokens: i32,
        credits: f64,
        status: &str,
    ) {
        let rec = UsageRecord {
            ts: Utc::now().to_rfc3339(),
            key_id: self.key_id,
            credential_id,
            model: self.model.clone(),
            input_tokens: input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            status: status.to_string(),
        };
        if let Some(r) = &self.recorder {
            r.record(&rec);
        }
        if let Some(a) = &self.aggregator {
            a.ingest(&rec);
        }
        if status == "success" && self.key_id != 0 {
            if let Some(m) = &self.client_keys {
                m.record_usage(
                    self.key_id,
                    rec.input_tokens,
                    rec.output_tokens,
                    rec.cache_creation_tokens,
                    rec.cache_read_tokens,
                    rec.credits,
                );
            }
        }
    }
}

/// 单次请求的链路追踪器
///
/// 在 handler 入口构造，作为 [`TraceSink`] 传入 provider；provider 在重试循环里
/// 每跳调用 [`on_attempt`](TraceSink::on_attempt) 累积一条 [`TraceAttempt`]。
/// 请求结束时调用 [`Self::finalize`] 组装 [`TraceRecord`] 并写入 SQLite。
///
/// `store` 为 None（未启用 Admin / trace）时所有方法都是空操作，零开销。
pub(crate) struct RequestTracer {
    store: Option<SharedTraceStore>,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    model: String,
    is_stream: bool,
    /// 本次请求实际下发的思考档位（low/medium/high/xhigh/max）；未启用/不支持为 None。
    reasoning_effort: Option<String>,
    /// 是否声明 1M 扩展上下文（客户端带 `anthropic-beta: context-1m-...` 头）。
    context_1m: bool,
    /// 客户端是否请求了推理（thinking 启用 或 显式 effort）；与档位独立。
    thinking: bool,
    started_at: Instant,
    /// 首个客户端可见内容事件产出时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    /// 首个 Kiro 原始 body chunk 到达时刻（仅流式标记；取第一次）
    upstream_first_byte_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Clone, Copy, Default)]
pub(crate) struct TraceUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_tokens: u64,
    pub cache_read_tokens: u64,
    pub credits: f64,
}

impl TraceUsage {
    /// 错误早退等无用量场景
    pub fn zero() -> Self {
        Self::default()
    }
}

struct RequestTraceOptions {
    key_ctx: KeyContext,
    model: String,
    is_stream: bool,
    /// 实际下发的思考档位；未启用/不支持为 None。
    reasoning_effort: Option<String>,
    /// 是否声明 1M 扩展上下文。
    context_1m: bool,
    /// 客户端是否请求了推理（thinking 启用 或 显式 effort）。
    thinking: bool,
}

impl RequestTracer {
    fn new(state: &AppState, options: RequestTraceOptions) -> Self {
        Self {
            store: state.trace_store.clone(),
            trace_id: Uuid::new_v4().to_string(),
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            model: options.model,
            is_stream: options.is_stream,
            reasoning_effort: options.reasoning_effort,
            context_1m: options.context_1m,
            thinking: options.thinking,
            started_at: Instant::now(),
            first_token_at: parking_lot::Mutex::new(None),
            upstream_first_byte_at: parking_lot::Mutex::new(None),
            attempts: parking_lot::Mutex::new(Vec::new()),
        }
    }

    /// 标记首个客户端可见内容事件产出（幂等，仅记录第一次）
    pub fn mark_first_token(&self) {
        let mut slot = self.first_token_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    /// 标记首个 Kiro 原始 body chunk 到达（幂等，仅记录第一次）
    pub fn mark_upstream_first_byte(&self) {
        let mut slot = self.upstream_first_byte_at.lock();
        if slot.is_none() {
            *slot = Some(Instant::now());
        }
    }

    fn last_http_status(&self) -> Option<u16> {
        self.attempts.lock().last().and_then(|a| a.http_status)
    }

    /// 组装并落库一条完整链路。store 为 None 时不做任何事。
    pub fn finalize(
        &self,
        final_status: &str,
        error_type: Option<&str>,
        error_message: Option<&str>,
        interrupted_after_bytes: Option<u64>,
        usage: TraceUsage,
    ) {
        let Some(store) = &self.store else { return };
        let attempts = std::mem::take(&mut *self.attempts.lock());
        // 最终凭据：最后一跳的命中凭据（成功跳即命中凭据，失败跳即最后尝试的凭据）
        let final_credential_id = attempts.last().map(|a| a.credential_id).unwrap_or(0);
        let first_token_ms = self
            .first_token_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let upstream_first_byte_ms = self
            .upstream_first_byte_at
            .lock()
            .map(|t| t.duration_since(self.started_at).as_millis() as u64);
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            model: self.model.clone(),
            is_stream: self.is_stream,
            final_status: final_status.to_string(),
            final_credential_id,
            error_type: error_type.map(|s| s.to_string()),
            error_message: error_message.map(|s| s.to_string()),
            total_attempts: attempts.len() as u32,
            duration_ms: self.started_at.elapsed().as_millis() as u64,
            interrupted_after_bytes,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_tokens: usage.cache_creation_tokens,
            cache_read_tokens: usage.cache_read_tokens,
            credits: usage.credits,
            first_token_ms,
            upstream_first_byte_ms,
            reasoning_effort: self.reasoning_effort.clone(),
            context_1m: self.context_1m,
            thinking: self.thinking,
            attempts,
        };
        store.insert(&rec);
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.attempts.lock().push(attempt);
    }
}

/// 取追踪器里最后一跳的 outcome（用于把 provider 的失败分类提升到 record.error_type）。
/// 返回 'static str（outcome 常量），无 attempt 时返回 None。
fn last_attempt_outcome(tracer: &RequestTracer) -> Option<&'static str> {
    let last = tracer.attempts.lock().last()?.outcome.clone();
    Some(match last.as_str() {
        outcome::QUOTA_EXHAUSTED => outcome::QUOTA_EXHAUSTED,
        outcome::ACCOUNT_THROTTLED => outcome::ACCOUNT_THROTTLED,
        outcome::AUTH_FAILED => outcome::AUTH_FAILED,
        outcome::TRANSIENT => outcome::TRANSIENT,
        outcome::NETWORK_ERROR => outcome::NETWORK_ERROR,
        outcome::BAD_REQUEST => outcome::BAD_REQUEST,
        _ => outcome::UNKNOWN,
    })
}

/// Image-budget warning threshold (in raw base64 chars, not decoded bytes).
/// Emits a warning when the total base64 char count of all image content in one request exceeds this threshold.
/// The threshold does not reject the request (the upstream makes the final call); it only gives operators more precise diagnostics.
const IMAGE_BUDGET_WARN_BYTES: usize = 800 * 1024;

/// Budget statistics for the image content in one inbound request.
struct ImageBudget {
    count: usize,
    total_b64_bytes: usize,
    largest_b64_bytes: usize,
}

/// Counts the total number of images in the payload and their base64 byte size.
/// Looks only at inline base64 (image source.type == "base64"), skipping url-mode images (which do not
/// go directly into a Bedrock single message body). This is a lightweight O(N) scan that does not decode base64.
fn count_image_budget(payload: &super::types::MessagesRequest) -> ImageBudget {
    let mut count = 0usize;
    let mut total = 0usize;
    let mut largest = 0usize;
    for msg in &payload.messages {
        if let serde_json::Value::Array(arr) = &msg.content {
            for item in arr {
                if item.get("type").and_then(|v| v.as_str()) != Some("image") {
                    continue;
                }
                let Some(src) = item.get("source") else {
                    continue;
                };
                if src.get("type").and_then(|v| v.as_str()) != Some("base64") {
                    continue;
                }
                let n = src
                    .get("data")
                    .and_then(|v| v.as_str())
                    .map(|s| s.len())
                    .unwrap_or(0);
                count += 1;
                total += n;
                if n > largest {
                    largest = n;
                }
            }
        }
    }
    ImageBudget {
        count,
        total_b64_bytes: total,
        largest_b64_bytes: largest,
    }
}

struct ClassifiedProviderError {
    http_status: StatusCode,
    error_type: &'static str,
    public_message: &'static str,
}

fn classify_provider_error(err: &Error) -> ClassifiedProviderError {
    let text = err.to_string();
    if text.contains("MODEL_NOT_AVAILABLE") {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "The requested model is not available for the configured upstream account.",
        };
    }
    if text.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Context window is full. Reduce conversation history, system prompt, or tools.",
        };
    }
    if text.contains("Input is too long") {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Input is too long. Reduce the size of your messages.",
        };
    }
    if crate::kiro::endpoint::default_is_client_validation_error(&text) {
        return ClassifiedProviderError {
            http_status: StatusCode::BAD_REQUEST,
            error_type: "invalid_request_error",
            public_message: "Invalid message sequence: tool_use and tool_result blocks must be correctly paired and ordered.",
        };
    }
    ClassifiedProviderError {
        http_status: StatusCode::BAD_GATEWAY,
        error_type: "api_error",
        public_message: "Upstream API request failed.",
    }
}

/// 将 KiroProvider 错误映射为 HTTP 响应。
pub(super) fn map_provider_error(err: Error) -> Response {
    let classified = classify_provider_error(&err);
    if classified.http_status.is_client_error() {
        tracing::warn!(error = %err, "上游拒绝了客户端请求");
    } else {
        tracing::error!(error = %err, "Kiro API 调用失败");
    }
    (
        classified.http_status,
        Json(ErrorResponse::new(
            classified.error_type,
            classified.public_message,
        )),
    )
        .into_response()
}

fn provider_error_sse(err: Error, upstream_status: Option<u16>) -> Bytes {
    let classified = classify_provider_error(&err);
    let mut error = json!({
        "type": classified.error_type,
        "message": classified.public_message,
    });
    if let Some(status) = upstream_status {
        error["upstream_status"] = json!(status);
    }
    SseEvent::new("error", json!({"type": "error", "error": error}))
        .to_sse_string()
        .into()
}

/// 按客户端可见输入生成 Anthropic usage；上游上下文只保留用于护栏与日志。
fn split_non_stream_usage(
    client_visible_tokens: i32,
    upstream_context_tokens: Option<i32>,
    cache_usage: &super::cache_metering::CacheUsage,
) -> (i32, i32, i32) {
    let mut usage = super::usage::InputTokenUsage::new(client_visible_tokens);
    if let Some(tokens) = upstream_context_tokens {
        usage.observe_upstream_context(tokens);
    }
    usage.split_api(cache_usage)
}

fn build_local_text_message(
    model: &str,
    answer: &str,
    input_tokens: i32,
    cache_usage: &super::cache_metering::CacheUsage,
) -> serde_json::Value {
    let (input_tokens, cache_creation_tokens, cache_read_tokens) =
        split_non_stream_usage(input_tokens, None, cache_usage);
    let output_tokens = token::count_tokens(answer).max(1) as i32;
    json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": answer}],
        "model": model,
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation_tokens,
            "cache_read_input_tokens": cache_read_tokens
        }
    })
}

fn build_local_text_stream_events(
    model: &str,
    answer: &str,
    input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
) -> Vec<SseEvent> {
    let (input_tokens, cache_creation_tokens, cache_read_tokens) =
        split_non_stream_usage(input_tokens, None, &cache_usage);
    let output_tokens = token::count_tokens(answer).max(1) as i32;
    let message_id = format!("msg_{}", Uuid::new_v4().to_string().replace('-', ""));

    vec![
        SseEvent::new(
            "message_start",
            json!({
                "type": "message_start",
                "message": {
                    "id": message_id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": model,
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {
                        "input_tokens": input_tokens,
                        "output_tokens": 0,
                        "cache_creation_input_tokens": cache_creation_tokens,
                        "cache_read_input_tokens": cache_read_tokens
                    }
                }
            }),
        ),
        SseEvent::new(
            "content_block_start",
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ),
        SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": answer}
            }),
        ),
        SseEvent::new(
            "content_block_stop",
            json!({"type": "content_block_stop", "index": 0}),
        ),
        SseEvent::new(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": output_tokens,
                    "cache_creation_input_tokens": cache_creation_tokens,
                    "cache_read_input_tokens": cache_read_tokens
                }
            }),
        ),
        SseEvent::new("message_stop", json!({"type": "message_stop"})),
    ]
}

fn local_exact_system_output(
    payload: &MessagesRequest,
) -> Option<super::exact_output::ExactOutput> {
    let output = super::exact_output::exact_system_output(payload)?;
    let output_tokens = token::count_tokens(output.as_str()).max(1) as i32;
    (output_tokens <= payload.max_tokens.max(0)).then_some(output)
}

#[cfg(test)]
fn local_exact_system_answer(payload: &MessagesRequest) -> Option<String> {
    local_exact_system_output(payload).map(|output| output.as_str().to_owned())
}

fn try_local_exact_system_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
) -> Option<Response> {
    let output = local_exact_system_output(payload)?;
    let answer = output.as_str();
    let output_tokens = token::count_tokens(answer).max(1) as i32;
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let cache_usage = state
        .cache_meter
        .as_ref()
        .map(|cache| {
            let usage = super::cache_metering::compute_cache_usage(cache, payload, hook.key_id);
            let (hr_min, hr_max) = provider.cache_hit_rate_bounds();
            usage.with_hit_rate_bounds(hr_min, hr_max)
        })
        .unwrap_or_default();
    let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
        split_non_stream_usage(input_tokens, None, &cache_usage);
    let output_kind = match &output {
        super::exact_output::ExactOutput::Text(_) => "text",
        super::exact_output::ExactOutput::Json(_) => "json",
    };

    tracing::debug!(
        output_kind,
        output_bytes = answer.len(),
        input_tokens = final_input_tokens,
        output_tokens,
        stream = payload.stream,
        "served static exact system output locally"
    );
    hook.record(
        0,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        0.0,
        "success",
    );

    if payload.stream {
        let body =
            build_local_text_stream_events(&payload.model, answer, input_tokens, cache_usage)
                .into_iter()
                .map(|event| event.to_sse_string())
                .collect::<String>();
        Some(
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONNECTION, "keep-alive")
                .body(Body::from(body))
                .unwrap(),
        )
    } else {
        Some(
            (
                StatusCode::OK,
                Json(build_local_text_message(
                    &payload.model,
                    answer,
                    input_tokens,
                    &cache_usage,
                )),
            )
                .into_response(),
        )
    }
}

fn local_document_system_is_safe_to_bypass(
    payload: &MessagesRequest,
    mode: crate::model::config::ToolCompatibilityMode,
) -> bool {
    const IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
    payload.system.as_ref().is_none_or(|blocks| {
        if mode != crate::model::config::ToolCompatibilityMode::ClaudeCode {
            return false;
        }
        blocks.iter().all(|block| {
            block
                .text
                .lines()
                .all(|line| line.trim().is_empty() || line.trim() == IDENTITY)
        })
    })
}

fn try_local_document_identifier_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    expansion: &super::document::DocumentExpansion,
    hook: &UsageRecordHook,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<Response> {
    if payload
        .tools
        .as_ref()
        .is_some_and(|tools| !tools.is_empty())
        || payload.tool_choice.is_some()
        || payload.thinking.as_ref().is_some_and(Thinking::is_enabled)
        || !local_document_system_is_safe_to_bypass(payload, mode)
    {
        return None;
    }
    let answer = expansion.deterministic_identifier_answer(payload)?;
    let output_tokens = token::count_tokens(&answer).max(1) as i32;
    if output_tokens > payload.max_tokens.max(0) {
        return None;
    }
    let input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;
    let cache_usage = state
        .cache_meter
        .as_ref()
        .map(|cache| {
            let usage = super::cache_metering::compute_cache_usage(cache, payload, hook.key_id);
            let (hr_min, hr_max) = provider.cache_hit_rate_bounds();
            usage.with_hit_rate_bounds(hr_min, hr_max)
        })
        .unwrap_or_default();
    let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
        split_non_stream_usage(input_tokens, None, &cache_usage);

    tracing::debug!(
        answer_bytes = answer.len(),
        input_tokens = final_input_tokens,
        output_tokens,
        stream = payload.stream,
        "served strict local identifier extraction for text PDF"
    );
    hook.record(
        0,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        0.0,
        "success",
    );

    if payload.stream {
        let body =
            build_local_text_stream_events(&payload.model, &answer, input_tokens, cache_usage)
                .into_iter()
                .map(|event| event.to_sse_string())
                .collect::<String>();
        Some(
            Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/event-stream")
                .header(header::CACHE_CONTROL, "no-cache")
                .header(header::CONNECTION, "keep-alive")
                .body(Body::from(body))
                .unwrap(),
        )
    } else {
        Some(
            (
                StatusCode::OK,
                Json(build_local_text_message(
                    &payload.model,
                    &answer,
                    input_tokens,
                    &cache_usage,
                )),
            )
                .into_response(),
        )
    }
}

fn available_models() -> Vec<Model> {
    let model = |id: &str, display_name: &str, owned_by: &str, max_tokens: i32| Model {
        id: id.to_string(),
        object: "model".to_string(),
        created: 1781481600,
        owned_by: owned_by.to_string(),
        display_name: display_name.to_string(),
        model_type: "chat".to_string(),
        max_tokens,
    };

    let mut models = vec![
        model("auto", "Auto", "kiro", 64000),
        model("claude-sonnet-5", "Claude Sonnet 5", "anthropic", 64000),
        model("claude-opus-4.8", "Claude Opus 4.8", "anthropic", 64000),
        model("claude-opus-4.7", "Claude Opus 4.7", "anthropic", 64000),
        model("claude-opus-4.6", "Claude Opus 4.6", "anthropic", 64000),
        model("claude-sonnet-4.6", "Claude Sonnet 4.6", "anthropic", 64000),
        model("claude-opus-4.5", "Claude Opus 4.5", "anthropic", 64000),
        model("claude-sonnet-4.5", "Claude Sonnet 4.5", "anthropic", 64000),
        model("claude-sonnet-4", "Claude Sonnet 4", "anthropic", 64000),
        model("claude-haiku-4.5", "Claude Haiku 4.5", "anthropic", 64000),
        model("deepseek-3.2", "DeepSeek v3.2", "deepseek", 64000),
        model("minimax-m2.5", "MiniMax M2.5", "minimax", 64000),
        model("minimax-m2.1", "MiniMax M2.1", "minimax", 64000),
        model("glm-5", "GLM 5", "zhipu", 64000),
        model("qwen3-coder-next", "Qwen3 Coder Next", "qwen", 64000),
        Model {
            id: "claude-fable-5".to_string(),
            object: "model".to_string(),
            created: 1781481600, // Jun 15, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-fable-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1781481600, // Jun 15, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Fable 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-5".to_string(),
            object: "model".to_string(),
            created: 1781481600, // Jun 15, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-5-thinking".to_string(),
            object: "model".to_string(),
            created: 1781481600, // Jun 15, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-8".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.8".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-8-thinking".to_string(),
            object: "model".to_string(),
            created: 1779897600, // May 28, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.8 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-7-thinking".to_string(),
            object: "model".to_string(),
            created: 1776276000, // Apr 16, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.7 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1770163200, // Feb 4, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-6-thinking".to_string(),
            object: "model".to_string(),
            created: 1771286400, // Feb 17, 2026
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.6 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-opus-4-5-20251101-thinking".to_string(),
            object: "model".to_string(),
            created: 1763942400, // Nov 24, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Opus 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-sonnet-4-5-20250929-thinking".to_string(),
            object: "model".to_string(),
            created: 1759104000, // Sep 29, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Sonnet 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
        Model {
            id: "claude-haiku-4-5-20251001-thinking".to_string(),
            object: "model".to_string(),
            created: 1760486400, // Oct 15, 2025
            owned_by: "anthropic".to_string(),
            display_name: "Claude Haiku 4.5 (Thinking)".to_string(),
            model_type: "chat".to_string(),
            max_tokens: 64000,
        },
    ];

    let mut seen = std::collections::HashSet::new();
    models.retain(|model| seen.insert(model.id.clone()));
    models
}

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models() -> impl IntoResponse {
    tracing::info!("Received GET /v1/models request");

    let models = available_models();

    Json(ModelsResponse {
        object: "list".to_string(),
        data: models,
    })
}

/// 1M 扩展上下文的 anthropic-beta 标记（与 Anthropic 官方一致）。
const BETA_CONTEXT_1M: &str = "context-1m-2025-08-07";

/// 从 `anthropic-beta` 请求头判断是否声明了 1M 扩展上下文。
/// 头值是逗号分隔的 beta token 列表，命中 `context-1m-2025-08-07` 即为真。
fn beta_has_context_1m(headers: &HeaderMap) -> bool {
    headers
        .get("anthropic-beta")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(',').any(|t| t.trim() == BETA_CONTEXT_1M))
        .unwrap_or(false)
}

/// 从已解析的 `additional_model_request_fields` 取实际下发的思考档位（effort）。
/// 未启用原生 reasoning / 模型不支持时该字段为 None，返回 None。
fn effort_from_fields(
    fields: &Option<crate::kiro::model::requests::kiro::AdditionalModelRequestFields>,
) -> Option<String> {
    fields
        .as_ref()
        .and_then(|f| f.output_config.as_ref())
        .map(|oc| oc.effort.clone())
        .filter(|e| !e.trim().is_empty())
}

/// 客户端是否请求了推理（展示用，与模型是否支持无关）。
///
/// 与 [`effort_from_fields`] 互补：effort 只在模型支持原生 reasoning 时才有值，而本判定
/// 反映「客户端意图」——thinking 启用 或 显式 `output_config.effort` 即为真。用于日志里
/// 「请求了推理但没解析出具体档位」时仍显示一个「思考」标记（对齐 Kiro-Go）。纯展示，
/// 不参与请求转换 / 上游发送 / 计费。
fn reasoning_requested(payload: &MessagesRequest) -> bool {
    payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false)
        || payload
            .output_config
            .as_ref()
            .is_some_and(|oc| !oc.effort.trim().is_empty())
}

fn map_document_error(error: super::document::DocumentError) -> Response {
    let status = if matches!(&error, super::document::DocumentError::TaskFailed(_)) {
        StatusCode::INTERNAL_SERVER_ERROR
    } else {
        StatusCode::BAD_REQUEST
    };
    let error_type = if status == StatusCode::BAD_REQUEST {
        "invalid_request_error"
    } else {
        "api_error"
    };
    (
        status,
        Json(ErrorResponse::new(error_type, error.to_string())),
    )
        .into_response()
}

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
    convert_request_with_mode(payload, mode).map_err(PrepareRequestError::Conversion)
}

/// POST /v1/messages
///
/// 创建消息（对话）
pub async fn post_messages(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    // Count the image budget on inbound to provide precise diagnostics for later context-window-full errors
    let img_stats = count_image_budget(&payload);
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        image_count = %img_stats.count,
        image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
        image_largest_b64_kb = %(img_stats.largest_b64_bytes / 1024),
        "Received POST /v1/messages request"
    );
    if img_stats.total_b64_bytes > IMAGE_BUDGET_WARN_BYTES {
        tracing::warn!(
            image_count = %img_stats.count,
            image_total_b64_kb = %(img_stats.total_b64_bytes / 1024),
            "incoming image payload is large; if upstream rejects with CONTENT_LENGTH_EXCEEDS_THRESHOLD, reduce image count or use lower-resolution screenshots"
        );
    }
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    if let Some(response) =
        try_local_exact_system_response(&state, provider.as_ref(), &payload, &hook)
    {
        return response;
    }

    let document_expansion = match super::document::expand_pdf_documents(&mut payload).await {
        Ok(expansion) => expansion,
        Err(error) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return map_document_error(error);
        }
    };
    if let Some(response) = try_local_document_identifier_response(
        &state,
        provider.as_ref(),
        &payload,
        &document_expansion,
        &hook,
        state.tool_compatibility_mode,
    ) {
        return response;
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let resp = websearch::handle_websearch_request(provider, &payload, input_tokens).await;
        // WebSearch 路径走 MCP 端点，没有 credential_id 上下文，统一记 0
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match prepare_request(&mut payload, state.tool_compatibility_mode).await
    {
        Ok(result) => result,
        Err(PrepareRequestError::Document(error)) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return map_document_error(error);
        }
        Err(PrepareRequestError::Conversion(e)) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
                ConversionError::InvalidToolChoice(reason) => {
                    ("invalid_request_error", format!("工具选择无效: {}", reason))
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 估算输入 tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;
    let tool_choice_policy = conversion_result.tool_choice_policy;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存。
    // 返回 estimate 口径的覆盖量；真实 input/cache 互斥分摊在拿到 total 真值时进行。
    let cache_usage = state
        .cache_meter
        .as_ref()
        .map(|cache| {
            let usage = super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id);
            // 注入运行时命中率整形区间（0,0 = 不整形）；随 cache_usage 带到分摊末尾。
            let (hr_min, hr_max) = provider.cache_hit_rate_bounds();
            usage.with_hit_rate_bounds(hr_min, hr_max)
        })
        .unwrap_or_default();

    if payload.stream {
        // 流式响应
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
                reasoning_effort: effort_from_fields(&kiro_request.additional_model_request_fields),
                context_1m: beta_has_context_1m(&headers),
                thinking: reasoning_requested(&payload),
            },
        ));
        handle_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
                reasoning_effort: effort_from_fields(&kiro_request.additional_model_request_fields),
                context_1m: beta_has_context_1m(&headers),
                thinking: reasoning_requested(&payload),
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    }
}

/// 处理流式请求
async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    if provider.early_stream_handshake() {
        let idle_timeout_secs = provider.stream_idle_timeout_secs();
        let stream = create_early_sse_stream(
            provider,
            request_body.to_owned(),
            model.to_owned(),
            input_tokens,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            group,
            idle_timeout_secs,
        );
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONNECTION, "keep-alive")
            .body(Body::from_stream(stream))
            .unwrap();
    }

    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            // 重试链路全部失败、未开始返回内容：error_type 取最后一跳分类
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建流处理上下文
    let mut ctx = StreamContext::new_with_constraints(
        model,
        input_tokens,
        thinking_enabled,
        provider.strict_thinking_validation(),
        tool_name_map,
        known_tool_names,
        tool_choice_policy,
    );
    ctx.cache_usage = cache_usage;
    if provider.identity_normalization() {
        ctx.enable_identity_filter();
    }

    // 生成初始事件
    let initial_events = ctx.generate_initial_events();

    // 创建 SSE 流（带 idle watchdog：上游首字节前挂死 / 中途停流超阈值主动收尾）
    let idle_timeout_secs = provider.stream_idle_timeout_secs();
    let stream = create_sse_stream(
        response,
        ctx,
        initial_events,
        hook,
        credential_id,
        tracer,
        idle_timeout_secs,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

struct EarlyStreamSetup {
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
    identity_normalization: bool,
    strict_thinking_validation: bool,
}

#[allow(clippy::too_many_arguments)]
fn create_early_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    model: String,
    input_tokens: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    idle_timeout_secs: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let tracer_for_call = tracer.clone();
    // 在 provider 被 move 进 call 之前捕获身份归一化开关。
    let identity_normalization = provider.identity_normalization();
    let strict_thinking_validation = provider.strict_thinking_validation();
    let call = async move {
        provider
            .call_api_stream(
                &request_body,
                Some(tracer_for_call.as_ref()),
                group.as_deref(),
            )
            .await
    };
    let mut setup = Some(EarlyStreamSetup {
        model,
        input_tokens,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        tool_choice_policy,
        hook,
        cache_usage,
        tracer,
        idle_timeout_secs,
        identity_normalization,
        strict_thinking_validation,
    });

    flatten_pending_call(call, move |result| {
        let setup = setup.take().expect("early stream setup consumed once");
        match result {
            Ok(call_result) => {
                let mut ctx = StreamContext::new_with_constraints(
                    setup.model,
                    setup.input_tokens,
                    setup.thinking_enabled,
                    setup.strict_thinking_validation,
                    setup.tool_name_map,
                    setup.known_tool_names,
                    setup.tool_choice_policy,
                );
                ctx.cache_usage = setup.cache_usage;
                if setup.identity_normalization {
                    ctx.enable_identity_filter();
                }
                let initial_events = ctx.generate_initial_events();
                Box::pin(create_sse_stream(
                    call_result.response,
                    ctx,
                    initial_events,
                    setup.hook,
                    call_result.credential_id,
                    setup.tracer,
                    setup.idle_timeout_secs,
                ))
            }
            Err(err) => {
                setup
                    .hook
                    .record(0, setup.input_tokens, 0, 0, 0, 0.0, "error");
                let upstream_status = setup.tracer.last_http_status();
                let error_type = last_attempt_outcome(&setup.tracer);
                let error_text = err.to_string();
                setup.tracer.finalize(
                    "error",
                    error_type,
                    Some(&error_text),
                    None,
                    TraceUsage::zero(),
                );
                Box::pin(stream::once(async move {
                    Ok(provider_error_sse(err, upstream_status))
                }))
            }
        }
    })
}

/// Ping 事件间隔（25秒）
const PING_INTERVAL_SECS: u64 = 25;

const EARLY_CONNECTED_SSE: &[u8] = b": connected\n\n";
const EARLY_PING_SSE: &[u8] = b"event: ping\ndata: {\"type\":\"ping\"}\n\n";
const EARLY_PING_INTERVAL: Duration = Duration::from_secs(1);

type BoxByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, Infallible>> + Send>>;

enum PendingCallEvent<T> {
    Comment(Bytes),
    Complete(anyhow::Result<T>),
}

impl<T> PendingCallEvent<T> {
    #[cfg(test)]
    fn comment_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Comment(bytes) => Some(bytes.as_ref()),
            Self::Complete(_) => None,
        }
    }
}

fn pending_call_stream<F, T>(future: F) -> impl Stream<Item = PendingCallEvent<T>>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
{
    struct State<F> {
        future: Pin<Box<F>>,
        heartbeat: tokio::time::Interval,
        connected_sent: bool,
        completed: bool,
    }

    let heartbeat = tokio::time::interval_at(
        TokioInstant::now() + EARLY_PING_INTERVAL,
        EARLY_PING_INTERVAL,
    );
    stream::unfold(
        State {
            future: Box::pin(future),
            heartbeat,
            connected_sent: false,
            completed: false,
        },
        |mut state| async move {
            if state.completed {
                return None;
            }
            if !state.connected_sent {
                state.connected_sent = true;
                return Some((
                    PendingCallEvent::Comment(Bytes::from_static(EARLY_CONNECTED_SSE)),
                    state,
                ));
            }
            tokio::select! {
                result = &mut state.future => {
                    state.completed = true;
                    Some((PendingCallEvent::Complete(result), state))
                }
                _ = state.heartbeat.tick() => Some((
                    PendingCallEvent::Comment(Bytes::from_static(EARLY_PING_SSE)),
                    state,
                )),
            }
        },
    )
}

fn flatten_pending_call<F, T, M>(
    future: F,
    mut on_complete: M,
) -> impl Stream<Item = Result<Bytes, Infallible>>
where
    F: Future<Output = anyhow::Result<T>> + Send + 'static,
    T: Send + 'static,
    M: FnMut(anyhow::Result<T>) -> BoxByteStream + Send + 'static,
{
    pending_call_stream(future)
        .map(move |event| -> BoxByteStream {
            match event {
                PendingCallEvent::Comment(bytes) => {
                    Box::pin(stream::once(async move { Ok(bytes) }))
                }
                PendingCallEvent::Complete(result) => on_complete(result),
            }
        })
        .flatten()
}

#[cfg(test)]
fn flatten_pending_call_for_test(
    result: anyhow::Result<BoxByteStream>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    flatten_pending_call(async move { result }, |result| match result {
        Ok(stream) => stream,
        Err(err) => Box::pin(stream::once(
            async move { Ok(provider_error_sse(err, None)) },
        )),
    })
}

#[cfg(test)]
fn early_error_test_stream(
    err: Error,
    upstream_status: Option<u16>,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    flatten_pending_call(
        async move { Err::<BoxByteStream, _>(err) },
        move |result| match result {
            Ok(stream) => stream,
            Err(err) => Box::pin(stream::once(async move {
                Ok(provider_error_sse(err, upstream_status))
            })),
        },
    )
}

/// 创建 ping 事件的 SSE 字符串
fn create_ping_sse() -> Bytes {
    Bytes::from("event: ping\ndata: {\"type\": \"ping\"}\n\n")
}

fn is_client_visible_content(event: &SseEvent) -> bool {
    if event.event == "content_block_start" {
        return event
            .data
            .pointer("/content_block/type")
            .and_then(serde_json::Value::as_str)
            == Some("tool_use");
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
            .is_some_and(|s| !s.is_empty()),
        Some("thinking_delta") => event
            .data
            .pointer("/delta/thinking")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        Some("input_json_delta") => event
            .data
            .pointer("/delta/partial_json")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty()),
        _ => false,
    }
}

fn mark_first_token_if_visible(tracer: &RequestTracer, events: &[SseEvent]) {
    if events.iter().any(is_client_visible_content) {
        tracer.mark_first_token();
    }
}

/// 创建 SSE 事件流
fn create_sse_stream(
    response: reqwest::Response,
    ctx: StreamContext,
    initial_events: Vec<SseEvent>,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    // 先发送初始事件
    let initial_stream = stream::iter(
        initial_events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    );

    // 然后处理 Kiro 响应流，同时每25秒发送 ping 保活
    let body_stream = response.bytes_stream();

    // idle watchdog 的初始截止时间：进入流处理即开始计时（覆盖首字节前挂死）。
    let idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));

    let processing_stream = stream::unfold(
        (body_stream, ctx, EventStreamDecoder::new(), false, interval(Duration::from_secs(PING_INTERVAL_SECS)), hook, credential_id, tracer, 0u64, idle_deadline),
        move |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes, mut idle_deadline)| async move {
            if finished {
                return None;
            }

            // idle watchdog：仅在 idle_timeout_secs > 0 时武装。截止时间跨 select 迭代持续，
            // 每收到一个 chunk 就顺延，故 ping 分支（每 25s 唤醒）不会重置它——只有真实数据才会。
            let idle_fut = async {
                if idle_timeout_secs == 0 {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep_until(idle_deadline).await;
                }
            };

            // 使用 select! 同时等待数据、ping 定时器与 idle watchdog
            tokio::select! {
                // 处理数据流
                chunk_result = body_stream.next() => {
                    match chunk_result {
                        Some(Ok(chunk)) => {
                            tracer.mark_upstream_first_byte();
                            sent_bytes += chunk.len() as u64;
                            // 收到真实字节：顺延 idle 截止时间
                            idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));
                            // 解码事件
                            if let Err(e) = decoder.feed(&chunk) {
                                tracing::warn!("缓冲区溢出: {}", e);
                            }

                            let mut events = Vec::new();
                            for result in decoder.decode_iter() {
                                match result {
                                    Ok(frame) => {
                                        if let Ok(event) = Event::from_frame(frame) {
                                            let sse_events = ctx.process_kiro_event(&event);
                                            events.extend(sse_events);
                                        }
                                    }
                                    Err(e) => {
                                        tracing::warn!("解码事件失败: {}", e);
                                    }
                                }
                            }
                            mark_first_token_if_visible(&tracer, &events);

                            // 转换为 SSE 字节流
                            let bytes: Vec<Result<Bytes, Infallible>> = events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();

                            Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)))
                        }
                        Some(Err(e)) => {
                            tracing::error!("读取响应流失败: {}", e);
                            // 发送最终事件并结束（记为 error）
                            let final_events = ctx.generate_final_events();
                            mark_first_token_if_visible(&tracer, &final_events);
                            record_stream_usage(&hook, &ctx, credential_id, "error");
                            // 已开始返回内容后上游断流：标记为 interrupted，带已发送字节数
                            tracer.finalize(
                                "interrupted",
                                Some(outcome::STREAM_INTERRUPTED),
                                Some(&e.to_string()),
                                Some(sent_bytes),
                                stream_trace_usage(&ctx),
                            );
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)))
                        }
                        None => {
                            // 流结束，发送最终事件（generate_final_events 内部会 finish()
                            // 累积器，据此判定是否有半截 / 非法工具调用 JSON）。
                            let final_events = ctx.generate_final_events();
                            mark_first_token_if_visible(&tracer, &final_events);
                            if let Some(message) = ctx.terminal_error_message() {
                                // 工具调用 JSON 半截 / 非法：实时流已回 200，无法改状态码，
                                // 只能记 error 并让 generate_final_events 补发的 `error` 事件透传给客户端。
                                record_stream_usage(&hook, &ctx, credential_id, "error");
                                tracer.finalize(
                                    "error",
                                    Some(outcome::BAD_REQUEST),
                                    Some(&message),
                                    None,
                                    stream_trace_usage(&ctx),
                                );
                            } else {
                                record_stream_usage(&hook, &ctx, credential_id, "success");
                                tracer.finalize(
                                    "success",
                                    None,
                                    None,
                                    None,
                                    stream_trace_usage(&ctx),
                                );
                            }
                            let bytes: Vec<Result<Bytes, Infallible>> = final_events
                                .into_iter()
                                .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                .collect();
                            Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)))
                        }
                    }
                }
                // 发送 ping 保活
                _ = ping_interval.tick() => {
                    tracing::trace!("发送 ping 保活事件");
                    let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)))
                }
                // idle watchdog：连续 idle_timeout_secs 秒没有真实字节 → 主动收尾。
                // 覆盖上游返回 200 后首字节前挂死、以及中途停流两种情况。已回 200 无法
                // 改状态码，只能发最终事件让客户端拿到一个干净的结束，而非空烧到绝对超时。
                _ = idle_fut => {
                    tracing::warn!(
                        "流式空闲超时（{}s 无字节），主动收尾。已发送 {} 字节",
                        idle_timeout_secs,
                        sent_bytes
                    );
                    let final_events = ctx.generate_final_events();
                    mark_first_token_if_visible(&tracer, &final_events);
                    record_stream_usage(&hook, &ctx, credential_id, "error");
                    tracer.finalize(
                        "interrupted",
                        Some(outcome::STREAM_INTERRUPTED),
                        Some(&format!("stream idle timeout after {}s", idle_timeout_secs)),
                        Some(sent_bytes),
                        stream_trace_usage(&ctx),
                    );
                    let bytes: Vec<Result<Bytes, Infallible>> = final_events
                        .into_iter()
                        .map(|e| Ok(Bytes::from(e.to_sse_string())))
                        .collect();
                    Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)))
                }
            }
        },
    )
    .flatten();

    initial_stream.chain(processing_stream)
}

/// 从 StreamContext 提取最终用量并写入 hook
fn record_stream_usage(
    hook: &UsageRecordHook,
    ctx: &StreamContext,
    credential_id: u64,
    status: &str,
) {
    // 互斥分摊后的 (input, cache_creation, cache_read)，与 trace 上报口径一致。
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    hook.record(
        credential_id,
        input,
        ctx.output_tokens,
        cache_creation,
        cache_read,
        ctx.credits,
        status,
    );
}

/// 从 StreamContext 提取用量，转成 trace 行用量（与 record_stream_usage 同源）
fn stream_trace_usage(ctx: &StreamContext) -> TraceUsage {
    let (input, cache_creation, cache_read) = ctx.resolved_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: ctx.output_tokens.max(0) as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        credits: if ctx.credits.is_finite() && ctx.credits > 0.0 {
            ctx.credits
        } else {
            0.0
        },
    }
}

use super::converter::get_context_window_size;

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    input_tokens: i32,
    thinking_enabled: bool,
    require_thinking: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 读取响应体
    let body_bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(e) => {
            tracing::error!("读取响应体失败: {}", e);
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    // 解析事件流
    let mut decoder = EventStreamDecoder::new();
    if let Err(e) = decoder.feed(&body_bytes) {
        tracing::warn!("缓冲区溢出: {}", e);
    }

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature: Option<String> = None;
    let mut native_redacted_thinking: Vec<String> = Vec::new();
    let mut tool_uses: Vec<serde_json::Value> = Vec::new();
    let mut upstream_signalled_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    // Kiro 整体上下文占用（包含客户端不可见的 foundational prompt）。
    let mut upstream_context_tokens: Option<i32> = None;
    // meteringEvent 上报的 credit 计费量（上游真实下发）；
    // input/cache_* 的互斥分摊在拿到 total 真值后由 cache_usage 完成。
    let mut credits: f64 = 0.0;

    // 工具调用参数 JSON 累积器：按 tool_use_id 缓冲分片，stop 时整体解析。
    // 半截 / 非法 JSON 显式暴露为错误（返回 502），不再静默回退 {} 或丢弃。
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::new();
    let mut tool_json_error: Option<super::stream::ToolJsonAccumulatorError> = None;

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => {
                if let Ok(event) = Event::from_frame(frame) {
                    match event {
                        Event::AssistantResponse(resp) => {
                            text_content.push_str(&resp.content);
                        }
                        Event::ReasoningContent(reasoning) => {
                            if let Some(text) = reasoning.text
                                && !text.is_empty()
                            {
                                native_thinking.push_str(&text);
                            }
                            if let Some(signature) = reasoning.signature
                                && !signature.is_empty()
                            {
                                native_thinking_signature = Some(signature);
                            }
                            if let Some(redacted) = reasoning.redacted_content
                                && !redacted.is_empty()
                            {
                                native_redacted_thinking.push(redacted);
                            }
                        }
                        Event::ToolUse(tool_use) => {
                            upstream_signalled_tool_use = true;
                            tracing::debug!(
                                tool_id = %tool_use.tool_use_id,
                                tool_name = %tool_use.name,
                                stop = tool_use.stop,
                                input_bytes = tool_use.input.len(),
                                "received upstream non-stream tool_use fragment"
                            );
                            match tool_accumulator.push(&tool_use, &tool_name_map) {
                                Ok(Some(completed)) => {
                                    tracing::debug!(
                                        tool_id = %completed.id,
                                        tool_name = %completed.name,
                                        input_bytes = completed.input.to_string().len(),
                                        "collected completed non-stream tool_use block"
                                    );
                                    tool_uses.push(completed.to_anthropic_block());
                                }
                                Ok(None) => {}
                                Err(e) => {
                                    tracing::error!("{}", e);
                                    tool_json_error = Some(e);
                                }
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            // 从上下文使用百分比计算实际的 input_tokens
                            let window_size = get_context_window_size(model);
                            let actual_input_tokens =
                                (context_usage.context_usage_percentage * (window_size as f64)
                                    / 100.0) as i32;
                            upstream_context_tokens = Some(actual_input_tokens);
                            // 上下文使用量达到 100% 时，设置 stop_reason 为 model_context_window_exceeded
                            if context_usage.context_usage_percentage >= 100.0 {
                                stop_reason = "model_context_window_exceeded".to_string();
                            }
                            tracing::debug!(
                                client_visible_tokens = input_tokens,
                                upstream_context_tokens = actual_input_tokens,
                                context_usage_percentage = context_usage.context_usage_percentage,
                                "received upstream context usage"
                            );
                        }
                        Event::Metering(metering) => {
                            // 上游只下发 credit；token / cache 字段不存在
                            credits += metering.usage;
                            tracing::debug!("metering credits +{:.6}", metering.usage);
                        }
                        Event::Exception { exception_type, .. } => {
                            if exception_type == "ContentLengthExceededException" {
                                stop_reason = "max_tokens".to_string();
                            }
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                tracing::warn!("解码事件失败: {}", e);
            }
        }
    }

    // 收尾：对未收到 stop=true 的残留缓冲区分处理——
    //   · 空入参（无参工具）→ 按 {} 打捞成完整 tool_use 加入响应；
    //   · 半截 JSON（上游写参数途中截断）→ 记为 IncompleteJson，返回 502。
    // 已有错误则保持不变。
    if tool_json_error.is_none() {
        let (salvaged, incomplete) = tool_accumulator.finish(&tool_name_map);
        for completed in salvaged {
            tracing::warn!(
                "上游在无参工具 {} ({}) 未发 stop=true 即断流，按 {{}} 打捞",
                completed.name,
                completed.id
            );
            tool_uses.push(completed.to_anthropic_block());
        }
        if let Some(e) = incomplete {
            tracing::error!("{}", e);
            tool_json_error = Some(e);
        }
    }

    // 工具调用 JSON 半截 / 非法：非流式路径尚未发送任何字节，直接回 502，
    // 明确暴露上游问题，而不是把无法解析的参数当成完整调用返回。
    if let Some(err) = tool_json_error {
        let message = err.message();
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_tool_json_error", message)),
        )
            .into_response();
    }

    // 剥离混入文本的字面 <tool_use> XML 泄漏（非流式：整段文本已就绪，一次性剥离）。
    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);

    // 身份归一化：把 Kiro 网关注入的品牌自述改写回 Claude（底层就是真实 Claude 模型）。
    let text_content = if provider.identity_normalization() {
        super::identity::normalize_identity_text(&text_content)
    } else {
        text_content
    };

    // 先保留原有 thinking 解析，再把可恢复的 <invoke> 和原生工具事件归一化。
    let base_content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    let content = super::stream::normalize_non_stream_content_blocks(
        base_content,
        tool_uses,
        &known_tool_names,
        &tool_name_map,
    );
    let content = normalize_required_tool_content(content, &tool_choice_policy);
    let strict_thinking_validation = provider.strict_thinking_validation();
    if require_thinking && !strict_thinking_validation && !content_has_reasoning(&content) {
        tracing::warn!(
            model,
            credential_id,
            "客户端请求了 thinking，但 Kiro 未返回 reasoning；保留有效正文或工具调用"
        );
    }
    if let Err(message) =
        validate_required_thinking(require_thinking, strict_thinking_validation, &content)
    {
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new(
                "upstream_thinking_protocol_error",
                message,
            )),
        )
            .into_response();
    }
    if let Err(message) = validate_tool_choice_content(&tool_choice_policy, &content) {
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_tool_choice_error", message)),
        )
            .into_response();
    }
    let has_output_tool_use = content
        .iter()
        .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"));
    let emitted_tool_names = content
        .iter()
        .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("name").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>();
    if let Err(message) = apply_tool_stop_reason(
        &mut stop_reason,
        upstream_signalled_tool_use,
        has_output_tool_use,
    ) {
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_tool_protocol_error", message)),
        )
            .into_response();
    }
    tracing::debug!(
        emitted_tool_names = ?emitted_tool_names,
        upstream_signalled_tool_use,
        stop_reason = %stop_reason,
        "finalized non-stream Anthropic tool event state"
    );
    if let Err(message) = validate_non_stream_content(&content) {
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(message),
            None,
            TraceUsage::zero(),
        );
        return (
            StatusCode::BAD_GATEWAY,
            Json(ErrorResponse::new("upstream_empty_response", message)),
        )
            .into_response();
    }

    // 估算输出 tokens（上游不下发 token，全部走估算）
    let output_tokens = token::estimate_output_tokens(&content);

    // API 只上报客户端可见输入；上游上下文占用不覆盖该口径。
    let (final_input_tokens, cache_creation_tokens, cache_read_tokens) =
        split_non_stream_usage(input_tokens, upstream_context_tokens, &cache_usage);

    // 构建 Anthropic 响应
    let response_body = json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "content": content,
        "model": model,
        "stop_reason": stop_reason,
        "stop_sequence": null,
        "usage": {
            "input_tokens": final_input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": cache_creation_tokens,
            "cache_read_input_tokens": cache_read_tokens
        }
    });

    hook.record(
        credential_id,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        credits,
        "success",
    );
    tracer.finalize(
        "success",
        None,
        None,
        None,
        TraceUsage {
            input_tokens: final_input_tokens.max(0) as u64,
            output_tokens: output_tokens.max(0) as u64,
            cache_creation_tokens: cache_creation_tokens.max(0) as u64,
            cache_read_tokens: cache_read_tokens.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
        },
    );
    (StatusCode::OK, Json(response_body)).into_response()
}

fn apply_tool_stop_reason(
    stop_reason: &mut String,
    upstream_signalled_tool_use: bool,
    has_output_tool_use: bool,
) -> Result<(), &'static str> {
    if upstream_signalled_tool_use && !has_output_tool_use {
        return Err("upstream ended with tool_use but produced no valid tool_use content block");
    }
    if has_output_tool_use {
        *stop_reason = "tool_use".to_string();
    }
    Ok(())
}

fn validate_non_stream_content(content: &[serde_json::Value]) -> Result<(), &'static str> {
    if content.is_empty() {
        Err("upstream returned no assistant content")
    } else {
        Ok(())
    }
}

fn normalize_required_tool_content(
    content: Vec<serde_json::Value>,
    policy: &super::converter::ToolChoicePolicy,
) -> Vec<serde_json::Value> {
    let has_tool = content
        .iter()
        .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"));
    if policy.is_required() && has_tool {
        content
            .into_iter()
            .filter(|block| block.get("type").and_then(serde_json::Value::as_str) != Some("text"))
            .collect()
    } else {
        content
    }
}

fn validate_required_thinking(
    thinking_enabled: bool,
    strict_validation: bool,
    content: &[serde_json::Value],
) -> Result<(), &'static str> {
    if !thinking_enabled || !strict_validation {
        return Ok(());
    }
    if content_has_reasoning(content) {
        Ok(())
    } else {
        Err("client requested thinking but upstream produced no thinking content")
    }
}

fn content_has_reasoning(content: &[serde_json::Value]) -> bool {
    content.iter().any(|block| {
        matches!(
            block.get("type").and_then(serde_json::Value::as_str),
            Some("thinking" | "redacted_thinking")
        )
    })
}

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

    let disable_parallel = match policy {
        ToolChoicePolicy::Auto {
            disable_parallel_tool_use,
        }
        | ToolChoicePolicy::RequiredAny {
            disable_parallel_tool_use,
        }
        | ToolChoicePolicy::RequiredSpecific {
            disable_parallel_tool_use,
            ..
        } => *disable_parallel_tool_use,
        ToolChoicePolicy::Disabled => false,
    };
    if disable_parallel && tool_names.len() > 1 {
        return Err(
            "client disabled parallel tool use but upstream produced multiple tool calls".into(),
        );
    }

    match policy {
        ToolChoicePolicy::Auto { .. } => Ok(()),
        ToolChoicePolicy::Disabled if tool_names.is_empty() => Ok(()),
        ToolChoicePolicy::Disabled => {
            Err("client disabled tool calls but upstream produced one".into())
        }
        ToolChoicePolicy::RequiredAny { .. } if tool_names.is_empty() => {
            Err("client required a tool call but upstream produced none".into())
        }
        ToolChoicePolicy::RequiredAny { .. } => Ok(()),
        ToolChoicePolicy::RequiredSpecific { name, .. }
            if tool_names.iter().any(|actual| *actual == name) =>
        {
            Ok(())
        }
        ToolChoicePolicy::RequiredSpecific { name, .. } => Err(format!(
            "client required tool {name} but upstream did not produce it"
        )),
    }
}

fn build_non_stream_content(
    thinking_enabled: bool,
    text_content: String,
    native_thinking: String,
    native_thinking_signature: Option<String>,
    native_redacted_thinking: Vec<String>,
) -> Vec<serde_json::Value> {
    let mut content = Vec::new();
    let has_native_thinking = !native_thinking.is_empty();

    if thinking_enabled {
        if has_native_thinking {
            content.push(json!({
                "type": "thinking",
                "thinking": native_thinking.clone(),
                "signature": native_thinking_signature
                    .unwrap_or_else(|| super::stream::THINKING_SIGNATURE_PLACEHOLDER.to_string()),
            }));
        } else {
            // 从完整文本中提取 thinking 块，兼容旧的 <thinking> 文本路径。
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);

            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text,
                    "signature": super::stream::THINKING_SIGNATURE_PLACEHOLDER,
                }));
            }

            if !remaining_text.is_empty() {
                content.push(json!({
                    "type": "text",
                    "text": remaining_text
                }));
            }
        }

        for redacted in native_redacted_thinking {
            content.push(json!({
                "type": "redacted_thinking",
                "data": redacted
            }));
        }

        if has_native_thinking && !text_content.is_empty() {
            content.push(json!({
                "type": "text",
                "text": text_content
            }));
        }
    } else if !text_content.is_empty() {
        content.push(json!({
            "type": "text",
            "text": text_content
        }));
    } else if has_native_thinking {
        content.push(json!({
            "type": "text",
            "text": native_thinking
        }));
    }
    content
}

/// 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
///
/// - Opus 4.6：覆写为 adaptive 类型
/// - 其他模型：覆写为 enabled 类型
/// - budget_tokens 固定为 20000
fn override_thinking_from_model_name(payload: &mut MessagesRequest) {
    let model_lower = payload.model.to_lowercase();
    if !model_lower.contains("thinking") {
        return;
    }

    let is_opus_4_6 = model_lower.contains("opus")
        && (model_lower.contains("4-6") || model_lower.contains("4.6"));

    let thinking_type = if is_opus_4_6 { "adaptive" } else { "enabled" };

    tracing::info!(
        model = %payload.model,
        thinking_type = thinking_type,
        "模型名包含 thinking 后缀，覆写 thinking 配置"
    );

    payload.thinking = Some(Thinking {
        thinking_type: thinking_type.to_string(),
        budget_tokens: 20000,
    });

    if is_opus_4_6 {
        payload.output_config = Some(OutputConfig {
            effort: "high".to_string(),
        });
    }
}

/// POST /v1/messages/count_tokens
///
/// 计算消息的 token 数量
pub async fn count_tokens(
    Extension(_key_ctx): Extension<KeyContext>,
    JsonExtractor(payload): JsonExtractor<CountTokensRequest>,
) -> impl IntoResponse {
    tracing::info!(
        model = %payload.model,
        message_count = %payload.messages.len(),
        "Received POST /v1/messages/count_tokens request"
    );

    let total_tokens = token::count_all_tokens(
        payload.model,
        payload.system,
        payload.messages,
        payload.tools,
    ) as i32;

    Json(CountTokensResponse {
        input_tokens: total_tokens.max(1) as i32,
    })
}

/// POST /cc/v1/messages
///
/// Claude Code 兼容端点，与 /v1/messages 的区别在于：
/// - 流式响应会缓冲到 Kiro 流结束后再统一发送
/// - message_start 中的 input_tokens 使用客户端可见输入与缓存拆分口径
pub async fn post_messages_cc(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
    headers: HeaderMap,
    JsonExtractor(mut payload): JsonExtractor<MessagesRequest>,
) -> Response {
    tracing::info!(
        model = %payload.model,
        max_tokens = %payload.max_tokens,
        stream = %payload.stream,
        message_count = %payload.messages.len(),
        "Received POST /cc/v1/messages request"
    );
    let hook = UsageRecordHook::from_state(&state, key_ctx.key_id, payload.model.clone());

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse::new(
                    "service_unavailable",
                    "Kiro API provider not configured",
                )),
            )
                .into_response();
        }
    };

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    if let Some(response) =
        try_local_exact_system_response(&state, provider.as_ref(), &payload, &hook)
    {
        return response;
    }

    let document_expansion = match super::document::expand_pdf_documents(&mut payload).await {
        Ok(expansion) => expansion,
        Err(error) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return map_document_error(error);
        }
    };
    if let Some(response) = try_local_document_identifier_response(
        &state,
        provider.as_ref(),
        &payload,
        &document_expansion,
        &hook,
        state.tool_compatibility_mode,
    ) {
        return response;
    }

    // 检查是否为 WebSearch 请求
    if websearch::has_web_search_tool(&payload) {
        tracing::info!("检测到 WebSearch 工具，路由到 WebSearch 处理");

        // 估算输入 tokens
        let input_tokens = token::count_all_tokens(
            payload.model.clone(),
            payload.system.clone(),
            payload.messages.clone(),
            payload.tools.clone(),
        ) as i32;

        let resp = websearch::handle_websearch_request(provider, &payload, input_tokens).await;
        let status = if resp.status().is_success() {
            "success"
        } else {
            "error"
        };
        hook.record(0, input_tokens, 0, 0, 0, 0.0, status);
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        return super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
        )
        .await;
    }

    // 转换请求
    let conversion_result = match prepare_request(&mut payload, state.tool_compatibility_mode).await
    {
        Ok(result) => result,
        Err(PrepareRequestError::Document(error)) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return map_document_error(error);
        }
        Err(PrepareRequestError::Conversion(e)) => {
            let (error_type, message) = match &e {
                ConversionError::UnsupportedModel(model) => {
                    ("invalid_request_error", format!("模型不支持: {}", model))
                }
                ConversionError::EmptyMessages => {
                    ("invalid_request_error", "消息列表为空".to_string())
                }
                ConversionError::UnsupportedToolMapping(reason) => (
                    "invalid_request_error",
                    format!("工具映射不支持: {}", reason),
                ),
                ConversionError::InvalidToolChoice(reason) => {
                    ("invalid_request_error", format!("工具选择无效: {}", reason))
                }
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(error_type, message)),
            )
                .into_response();
        }
    };

    // Build the Kiro request. profile_arn is injected by the provider layer from the actual
    // credentials; additional_model_request_fields is already filtered by converter model support.
    let kiro_request = KiroRequest {
        conversation_state: conversion_result.conversation_state,
        profile_arn: None,
        additional_model_request_fields: conversion_result.additional_model_request_fields,
    };

    let request_body = match serde_json::to_string(&kiro_request) {
        Ok(body) => body,
        Err(e) => {
            tracing::error!("序列化请求失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse::new(
                    "internal_error",
                    format!("序列化请求失败: {}", e),
                )),
            )
                .into_response();
        }
    };

    tracing::debug!("Kiro request body: {}", request_body);

    // 计算总 input tokens
    let total_input_tokens = token::count_all_tokens(
        payload.model.clone(),
        payload.system.clone(),
        payload.messages.clone(),
        payload.tools.clone(),
    ) as i32;

    // 检查是否启用了thinking
    let thinking_enabled = payload
        .thinking
        .as_ref()
        .map(|t| t.is_enabled())
        .unwrap_or(false);

    let tool_name_map = conversion_result.tool_name_map;
    let known_tool_names = conversion_result.known_tool_names;
    let tool_choice_policy = conversion_result.tool_choice_policy;

    // CacheMeter：根据 cache_control 断点查 / 写中转层提示词缓存（estimate 口径）。
    let cache_usage = state
        .cache_meter
        .as_ref()
        .map(|cache| {
            let usage = super::cache_metering::compute_cache_usage(cache, &payload, key_ctx.key_id);
            // 注入运行时命中率整形区间（0,0 = 不整形）；随 cache_usage 带到分摊末尾。
            let (hr_min, hr_max) = provider.cache_hit_rate_bounds();
            usage.with_hit_rate_bounds(hr_min, hr_max)
        })
        .unwrap_or_default();

    if payload.stream {
        // 流式响应（缓冲模式）
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: true,
                reasoning_effort: effort_from_fields(&kiro_request.additional_model_request_fields),
                context_1m: beta_has_context_1m(&headers),
                thinking: reasoning_requested(&payload),
            },
        ));
        handle_stream_request_buffered(
            provider,
            &request_body,
            &payload.model,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
            hook,
            total_input_tokens,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        let tracer = std::sync::Arc::new(RequestTracer::new(
            &state,
            RequestTraceOptions {
                key_ctx: key_ctx.clone(),
                model: payload.model.clone(),
                is_stream: false,
                reasoning_effort: effort_from_fields(&kiro_request.additional_model_request_fields),
                context_1m: beta_has_context_1m(&headers),
                thinking: reasoning_requested(&payload),
            },
        ));
        handle_non_stream_request(
            provider,
            &request_body,
            &payload.model,
            total_input_tokens,
            extract_thinking,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
        )
        .await
    }
}

/// 处理流式请求（缓冲版本）
///
/// 与 `handle_stream_request` 不同，此函数会缓冲所有事件直到流结束，
/// 然后用客户端可见 input_tokens 与缓存拆分生成 message_start 事件。
async fn handle_stream_request_buffered(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    model: &str,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream(request_body, Some(tracer.as_ref()), group.as_deref())
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            hook.record(0, fallback_input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&e.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(e);
        }
    };
    let response = call_result.response;
    let credential_id = call_result.credential_id;

    // 创建缓冲流处理上下文
    let mut ctx = BufferedStreamContext::new_with_constraints(
        model,
        fallback_input_tokens,
        thinking_enabled,
        provider.strict_thinking_validation(),
        tool_name_map,
        known_tool_names,
        tool_choice_policy,
    );
    ctx.set_cache_usage(cache_usage);
    if provider.identity_normalization() {
        ctx.enable_identity_filter();
    }

    // 创建缓冲 SSE 流
    let idle_timeout_secs = provider.stream_idle_timeout_secs();
    let stream = create_buffered_sse_stream(
        response,
        ctx,
        hook,
        credential_id,
        tracer,
        idle_timeout_secs,
    );

    // 返回 SSE 响应
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(stream))
        .unwrap()
}

/// 创建缓冲 SSE 事件流
///
/// 工作流程：
/// 1. 等待上游流完成，期间只发送 ping 保活信号
/// 2. 使用 StreamContext 的事件处理逻辑处理所有 Kiro 事件，结果缓存
/// 3. 流结束后，用正确的 input_tokens 更正 message_start 事件
/// 4. 一次性发送所有事件
fn create_buffered_sse_stream(
    response: reqwest::Response,
    ctx: BufferedStreamContext,
    hook: UsageRecordHook,
    credential_id: u64,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let body_stream = response.bytes_stream();

    // idle watchdog 的初始截止时间：进入流处理即开始计时（覆盖首字节前挂死）。
    let idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));

    stream::unfold(
        (
            body_stream,
            ctx,
            EventStreamDecoder::new(),
            false,
            interval(Duration::from_secs(PING_INTERVAL_SECS)),
            hook,
            credential_id,
            tracer,
            0u64,
            idle_deadline,
        ),
        move |(mut body_stream, mut ctx, mut decoder, finished, mut ping_interval, hook, credential_id, tracer, mut sent_bytes, mut idle_deadline)| async move {
            if finished {
                return None;
            }

            loop {
                // idle watchdog：仅在 idle_timeout_secs > 0 时武装；每收到真实字节顺延，
                // ping 分支（每 25s）不会重置它。idle_fut 在循环内每轮重建，捕获 idle_deadline
                // 的**拷贝**（TokioInstant 是 Copy），故 chunk 分支重新赋值 idle_deadline 不与之冲突。
                let deadline = idle_deadline;
                let idle_fut = async move {
                    if idle_timeout_secs == 0 {
                        std::future::pending::<()>().await;
                    } else {
                        tokio::time::sleep_until(deadline).await;
                    }
                };

                tokio::select! {
                    // 使用 biased 模式，优先检查 ping 定时器
                    // 避免在上游 chunk 密集时 ping 被"饿死"
                    biased;

                    // 优先检查 ping 保活（等待期间唯一发送的数据）
                    _ = ping_interval.tick() => {
                        tracing::trace!("发送 ping 保活事件（缓冲模式）");
                        let bytes: Vec<Result<Bytes, Infallible>> = vec![Ok(create_ping_sse())];
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, false, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)));
                    }

                    // idle watchdog：连续 idle_timeout_secs 秒无真实字节 → 主动收尾。
                    // 缓冲模式尚未向客户端发过任何内容事件，收尾即把已缓冲事件一次性吐出。
                    _ = idle_fut => {
                        tracing::warn!(
                            "缓冲流空闲超时（{}s 无字节），主动收尾。已接收 {} 字节",
                            idle_timeout_secs,
                            sent_bytes
                        );
                        let all_events = ctx.finish_and_get_all_events();
                        mark_first_token_if_visible(&tracer, &all_events);
                        let (i, o, cc, cr, credits) = ctx.final_usage();
                        hook.record(credential_id, i, o, cc, cr, credits, "error");
                        tracer.finalize(
                            "interrupted",
                            Some(outcome::STREAM_INTERRUPTED),
                            Some(&format!("stream idle timeout after {}s", idle_timeout_secs)),
                            Some(sent_bytes),
                            TraceUsage {
                                input_tokens: i.max(0) as u64,
                                output_tokens: o.max(0) as u64,
                                cache_creation_tokens: cc.max(0) as u64,
                                cache_read_tokens: cr.max(0) as u64,
                                credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                            },
                        );
                        let bytes: Vec<Result<Bytes, Infallible>> = all_events
                            .into_iter()
                            .map(|e| Ok(Bytes::from(e.to_sse_string())))
                            .collect();
                        return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)));
                    }

                    // 然后处理数据流
                    chunk_result = body_stream.next() => {
                        match chunk_result {
                            Some(Ok(chunk)) => {
                                tracer.mark_upstream_first_byte();
                                sent_bytes += chunk.len() as u64;
                                // 收到真实字节：顺延 idle 截止时间
                                idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));
                                // 解码事件
                                if let Err(e) = decoder.feed(&chunk) {
                                    tracing::warn!("缓冲区溢出: {}", e);
                                }

                                for result in decoder.decode_iter() {
                                    match result {
                                        Ok(frame) => {
                                            if let Ok(event) = Event::from_frame(frame) {
                                                // 缓冲事件（复用 StreamContext 的处理逻辑）
                                                ctx.process_and_buffer(&event);
                                            }
                                        }
                                        Err(e) => {
                                            tracing::warn!("解码事件失败: {}", e);
                                        }
                                    }
                                }
                                // 继续读取下一个 chunk，不发送任何数据
                            }
                            Some(Err(e)) => {
                                tracing::error!("读取响应流失败: {}", e);
                                // 发生错误，完成处理并返回所有事件
                                let all_events = ctx.finish_and_get_all_events();
                                mark_first_token_if_visible(&tracer, &all_events);
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                hook.record(credential_id, i, o, cc, cr, credits, "error");
                                // 缓冲模式 chunk 读取失败：上游中途断流
                                tracer.finalize(
                                    "interrupted",
                                    Some(outcome::STREAM_INTERRUPTED),
                                    Some(&e.to_string()),
                                    Some(sent_bytes),
                                    TraceUsage {
                                        input_tokens: i.max(0) as u64,
                                        output_tokens: o.max(0) as u64,
                                        cache_creation_tokens: cc.max(0) as u64,
                                        cache_read_tokens: cr.max(0) as u64,
                                        credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                    },
                                );
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)));
                            }
                            None => {
                                // 流结束，完成处理并返回所有事件（已更正 input_tokens）。
                                // finish_and_get_all_events 内部会 finish() 累积器；若有半截 /
                                // 非法工具调用 JSON，error 事件已随缓冲发出，这里据此记 error。
                                let all_events = ctx.finish_and_get_all_events();
                                mark_first_token_if_visible(&tracer, &all_events);
                                let (i, o, cc, cr, credits) = ctx.final_usage();
                                let trace_usage = TraceUsage {
                                    input_tokens: i.max(0) as u64,
                                    output_tokens: o.max(0) as u64,
                                    cache_creation_tokens: cc.max(0) as u64,
                                    cache_read_tokens: cr.max(0) as u64,
                                    credits: if credits.is_finite() && credits > 0.0 { credits } else { 0.0 },
                                };
                                if let Some(message) = ctx.terminal_error_message() {
                                    hook.record(credential_id, i, o, cc, cr, credits, "error");
                                    tracer.finalize(
                                        "error",
                                        Some(outcome::BAD_REQUEST),
                                        Some(&message),
                                        None,
                                        trace_usage,
                                    );
                                } else {
                                    hook.record(credential_id, i, o, cc, cr, credits, "success");
                                    tracer.finalize("success", None, None, None, trace_usage);
                                }
                                let bytes: Vec<Result<Bytes, Infallible>> = all_events
                                    .into_iter()
                                    .map(|e| Ok(Bytes::from(e.to_sse_string())))
                                    .collect();
                                return Some((stream::iter(bytes), (body_stream, ctx, decoder, true, ping_interval, hook, credential_id, tracer, sent_bytes, idle_deadline)));
                            }
                        }
                    }
                }
            }
        },
    )
    .flatten()
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures::{StreamExt, future};

    use super::*;

    const PDF_CANARY_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvUmVzb3VyY2VzIDw8IC9Gb250IDw8IC9GMSA0IDAgUiA+PiA+PiAvQ29udGVudHMgNSAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iago1IDAgb2JqCjw8IC9MZW5ndGggNTQgPj4Kc3RyZWFtCkJUIC9GMSAxMiBUZiA3MiA3MjAgVGQgKFBERi1DT01QQVRJQklMSVRZLVRPS0VOKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzExIDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDE1CiUlRU9GCg==";

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

        let converted = prepare_request(
            &mut request,
            crate::model::config::ToolCompatibilityMode::ClaudeCode,
        )
        .await
        .unwrap();
        let content = &converted
            .conversation_state
            .current_message
            .user_input_message
            .content;
        assert!(content.find("before").unwrap() < content.find("PDF-COMPATIBILITY-TOKEN").unwrap());
        assert!(content.find("PDF-COMPATIBILITY-TOKEN").unwrap() < content.find("after").unwrap());
    }

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
            prepare_request(
                &mut request,
                crate::model::config::ToolCompatibilityMode::ClaudeCode,
            )
            .await,
            Err(PrepareRequestError::Document(_))
        ));
    }

    #[test]
    fn required_any_rejects_non_stream_text_only_content() {
        let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
        assert!(
            validate_tool_choice_content(
                &crate::anthropic::converter::ToolChoicePolicy::RequiredAny {
                    disable_parallel_tool_use: false,
                },
                &content,
            )
            .is_err()
        );
    }

    #[test]
    fn disable_parallel_rejects_multiple_non_stream_tool_calls() {
        let content = vec![
            serde_json::json!({"type": "tool_use", "name": "first_tool", "input": {}}),
            serde_json::json!({"type": "tool_use", "name": "second_tool", "input": {}}),
        ];
        assert!(
            validate_tool_choice_content(
                &crate::anthropic::converter::ToolChoicePolicy::RequiredAny {
                    disable_parallel_tool_use: true,
                },
                &content,
            )
            .unwrap_err()
            .contains("parallel")
        );
    }

    #[test]
    fn compatible_thinking_accepts_plain_text_without_reasoning_block() {
        let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
        assert!(validate_required_thinking(true, false, &content).is_ok());
    }

    #[test]
    fn strict_thinking_rejects_plain_text_without_reasoning_block() {
        let content = vec![serde_json::json!({"type": "text", "text": "plain"})];
        assert!(validate_required_thinking(true, true, &content).is_err());
    }

    #[test]
    fn unavailable_model_maps_to_anthropic_400() {
        let classified = classify_provider_error(&anyhow::anyhow!(
            "MODEL_NOT_AVAILABLE: requested model is unavailable"
        ));
        assert_eq!(classified.http_status, StatusCode::BAD_REQUEST);
        assert_eq!(classified.error_type, "invalid_request_error");
    }

    #[test]
    fn redacted_thinking_satisfies_required_thinking() {
        let content = vec![serde_json::json!({
            "type": "redacted_thinking",
            "data": "encrypted"
        })];
        assert!(validate_required_thinking(true, true, &content).is_ok());
    }

    #[test]
    fn non_stream_usage_ignores_upstream_context_for_api_total() {
        let cache = crate::anthropic::cache_metering::CacheUsage::default();
        assert_eq!(split_non_stream_usage(72, Some(5_417), &cache), (72, 0, 0));
    }

    #[test]
    fn empty_upstream_content_is_not_a_successful_non_stream_response() {
        let content: Vec<serde_json::Value> = Vec::new();
        assert!(validate_non_stream_content(&content).is_err());
    }

    #[tokio::test]
    async fn document_input_error_maps_to_anthropic_400() {
        let response =
            map_document_error(crate::anthropic::document::DocumentError::InvalidSource {
                location: "messages[0].content[1]".to_string(),
                message: "media_type must be application/pdf".to_string(),
            });
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn document_task_failure_maps_to_500() {
        let response = map_document_error(crate::anthropic::document::DocumentError::TaskFailed(
            "worker panicked".to_string(),
        ));
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn local_document_non_stream_body_is_standard_anthropic_message() {
        let body = build_local_text_message(
            "claude-opus-4-8",
            "ORDER-ID-4f8a2c1d",
            42,
            &crate::anthropic::cache_metering::CacheUsage::default(),
        );

        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["content"][0]["type"], "text");
        assert_eq!(body["content"][0]["text"], "ORDER-ID-4f8a2c1d");
        assert_eq!(body["stop_reason"], "end_turn");
        assert_eq!(body["usage"]["input_tokens"], 42);
        assert!(body["usage"]["output_tokens"].as_i64().unwrap() > 0);
    }

    #[test]
    fn local_document_stream_events_have_complete_standard_sequence() {
        let events = build_local_text_stream_events(
            "claude-opus-4-8",
            "ORDER-ID-4f8a2c1d",
            42,
            crate::anthropic::cache_metering::CacheUsage::default(),
        );
        let names = events
            .iter()
            .map(|event| event.event.as_str())
            .collect::<Vec<_>>();

        assert_eq!(
            names,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(events[2].data["delta"]["text"], "ORDER-ID-4f8a2c1d");
        assert_eq!(events[4].data["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[4].data["usage"]["input_tokens"], 42);
    }

    #[test]
    fn exact_system_non_stream_body_uses_generic_local_text_builder() {
        let body = build_local_text_message(
            "claude-opus-4-8",
            "alpha_42",
            20,
            &crate::anthropic::cache_metering::CacheUsage::default(),
        );
        assert_eq!(body["content"][0]["text"], "alpha_42");
        assert_eq!(body["stop_reason"], "end_turn");
    }

    #[test]
    fn exact_system_stream_uses_generic_local_text_builder() {
        let events = build_local_text_stream_events(
            "claude-opus-4-8",
            "{\"a\":330}",
            20,
            crate::anthropic::cache_metering::CacheUsage::default(),
        );
        assert_eq!(events[2].data["delta"]["text"], "{\"a\":330}");
        assert_eq!(events.last().unwrap().event, "message_stop");
    }

    #[test]
    fn exact_system_answer_accepts_static_contract_and_rejects_unsafe_or_too_small_output() {
        let request = |system: &str, max_tokens: i32| -> MessagesRequest {
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": "hello"}],
                "system": system
            }))
            .unwrap()
        };

        assert_eq!(
            local_exact_system_answer(&request(
                "Return exactly the single word 'alpha_42' and nothing else. No explanation.",
                64,
            )),
            Some("alpha_42".to_string())
        );
        assert_eq!(
            local_exact_system_answer(&request("You are CodeAssist v2.", 64)),
            None
        );
        assert_eq!(
            local_exact_system_answer(&request(
                "Return exactly the single word 'alpha_42' and nothing else. No explanation.",
                0,
            )),
            None
        );
    }

    #[test]
    fn local_document_bypass_accepts_only_the_removed_claude_code_identity() {
        let request = |system: Option<&str>| -> MessagesRequest {
            let mut value = serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "test"}]
            });
            if let Some(system) = system {
                value["system"] = serde_json::json!(system);
            }
            serde_json::from_value(value).unwrap()
        };
        let identity = "You are Claude Code, Anthropic's official CLI for Claude.";

        assert!(local_document_system_is_safe_to_bypass(
            &request(None),
            crate::model::config::ToolCompatibilityMode::Raw
        ));
        assert!(local_document_system_is_safe_to_bypass(
            &request(Some(identity)),
            crate::model::config::ToolCompatibilityMode::ClaudeCode
        ));
        assert!(!local_document_system_is_safe_to_bypass(
            &request(Some(identity)),
            crate::model::config::ToolCompatibilityMode::Raw
        ));
        assert!(!local_document_system_is_safe_to_bypass(
            &request(Some(&format!("{identity}\nKeep this rule."))),
            crate::model::config::ToolCompatibilityMode::ClaudeCode
        ));
    }

    #[test]
    fn tool_stop_reason_matches_final_content() {
        let mut plain = "end_turn".to_string();
        assert_eq!(apply_tool_stop_reason(&mut plain, false, false), Ok(()));
        assert_eq!(plain, "end_turn");

        let mut recovered = "end_turn".to_string();
        assert_eq!(apply_tool_stop_reason(&mut recovered, false, true), Ok(()));
        assert_eq!(recovered, "tool_use");

        let mut native = "end_turn".to_string();
        assert_eq!(apply_tool_stop_reason(&mut native, true, true), Ok(()));
        assert_eq!(native, "tool_use");

        let mut recovered_after_max_tokens_hint = "max_tokens".to_string();
        assert_eq!(
            apply_tool_stop_reason(&mut recovered_after_max_tokens_hint, false, true),
            Ok(())
        );
        assert_eq!(recovered_after_max_tokens_hint, "tool_use");

        let mut broken = "end_turn".to_string();
        assert!(apply_tool_stop_reason(&mut broken, true, false).is_err());
    }

    #[test]
    fn normalize_required_tool_content_strips_narration_only_when_tool_exists() {
        let content = vec![
            serde_json::json!({"type":"text","text":"I will call it."}),
            serde_json::json!({
                "type":"tool_use",
                "id":"toolu_1",
                "name":"get_weather",
                "input":{}
            }),
        ];
        let required = super::super::converter::ToolChoicePolicy::RequiredAny {
            disable_parallel_tool_use: false,
        };
        let filtered = normalize_required_tool_content(content.clone(), &required);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "tool_use");

        let auto = super::super::converter::ToolChoicePolicy::Auto {
            disable_parallel_tool_use: false,
        };
        assert_eq!(normalize_required_tool_content(content, &auto).len(), 2);

        let text_only = vec![serde_json::json!({"type":"text","text":"no tool"})];
        assert_eq!(
            normalize_required_tool_content(text_only, &required)[0]["type"],
            "text"
        );
    }

    #[tokio::test]
    async fn pending_call_stream_emits_connected_then_ping() {
        let stream = pending_call_stream(future::pending::<Result<(), anyhow::Error>>());
        futures::pin_mut!(stream);

        let connected = stream.next().await.unwrap();
        let connected_bytes = connected.comment_bytes().unwrap();
        assert_eq!(connected_bytes, b": connected\n\n");
        assert!(
            !connected_bytes
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"data:")),
            "connected 必须保持为 New API 忽略的 SSE 注释"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), stream.next())
                .await
                .is_err(),
            "ping 不应紧跟 connected 立即发出"
        );
        let ping = tokio::time::timeout(Duration::from_millis(1200), stream.next())
            .await
            .expect("1 秒后应产生 ping")
            .unwrap();
        let ping_bytes = ping.comment_bytes().unwrap();
        assert_eq!(ping_bytes, b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert!(
            ping_bytes
                .split(|byte| *byte == b'\n')
                .any(|line| line.starts_with(b"data:")),
            "ping 必须包含 New API 可识别的 data 行"
        );
    }

    #[tokio::test]
    async fn pending_call_stream_completes_after_connected() {
        let stream = pending_call_stream(async { Ok::<_, anyhow::Error>(7u8) });
        futures::pin_mut!(stream);

        assert!(matches!(
            stream.next().await,
            Some(PendingCallEvent::Comment(_))
        ));
        assert!(matches!(
            stream.next().await,
            Some(PendingCallEvent::Complete(Ok(7)))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn dropping_pending_call_stream_drops_the_provider_future() {
        struct DropFlag(Arc<AtomicBool>);
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let dropped = Arc::new(AtomicBool::new(false));
        {
            let guard = DropFlag(dropped.clone());
            let stream = pending_call_stream(async move {
                let _guard = guard;
                future::pending::<Result<(), anyhow::Error>>().await
            });
            futures::pin_mut!(stream);
            assert!(matches!(
                stream.next().await,
                Some(PendingCallEvent::Comment(_))
            ));
        }
        tokio::task::yield_now().await;
        assert!(dropped.load(Ordering::SeqCst));
    }

    #[test]
    fn provider_error_sse_is_sanitized_and_carries_upstream_status() {
        let bytes = provider_error_sse(
            anyhow::anyhow!("Bearer secret-token connection reset"),
            Some(429),
        );
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.starts_with("event: error\ndata: "));
        assert!(text.contains("\"upstream_status\":429"));
        assert!(!text.contains("secret-token"));
    }

    #[test]
    fn provider_validation_error_keeps_invalid_request_classification() {
        let classified = classify_provider_error(&anyhow::anyhow!(
            "Expected toolResult blocks but found none"
        ));
        assert_eq!(classified.http_status, StatusCode::BAD_REQUEST);
        assert_eq!(classified.error_type, "invalid_request_error");
    }

    #[tokio::test]
    async fn early_error_stream_sends_comment_then_error_without_message_start() {
        let stream = early_error_test_stream(anyhow::anyhow!("connection reset"), Some(502));
        futures::pin_mut!(stream);
        let first = stream.next().await.unwrap().unwrap();
        let second = stream.next().await.unwrap().unwrap();
        assert_eq!(first, Bytes::from_static(EARLY_CONNECTED_SSE));
        assert!(String::from_utf8_lossy(&second).starts_with("event: error\n"));
        assert!(!String::from_utf8_lossy(&second).contains("message_start"));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn flatten_pending_call_preserves_success_stream_order() {
        let inner: BoxByteStream = Box::pin(stream::iter(vec![
            Ok(Bytes::from_static(b"event: message_start\ndata: {}\n\n")),
            Ok(Bytes::from_static(
                b"event: content_block_delta\ndata: {}\n\n",
            )),
        ]));
        let stream = flatten_pending_call_for_test(Ok(inner));
        futures::pin_mut!(stream);
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(EARLY_CONNECTED_SSE)
        );
        assert!(
            String::from_utf8_lossy(&stream.next().await.unwrap().unwrap())
                .contains("message_start")
        );
        assert!(
            String::from_utf8_lossy(&stream.next().await.unwrap().unwrap())
                .contains("content_block_delta")
        );
    }

    #[test]
    fn only_non_empty_content_events_are_client_visible_first_tokens() {
        assert!(!is_client_visible_content(&SseEvent::new(
            "message_start",
            json!({})
        )));
        assert!(!is_client_visible_content(&SseEvent::new(
            "content_block_delta",
            json!({"delta":{"type":"text_delta","text":""}}),
        )));
        assert!(is_client_visible_content(&SseEvent::new(
            "content_block_delta",
            json!({"delta":{"type":"text_delta","text":"hi"}}),
        )));
        assert!(is_client_visible_content(&SseEvent::new(
            "content_block_start",
            json!({"content_block":{"type":"tool_use","name":"Bash"}}),
        )));
    }

    #[test]
    fn bedrock_client_validation_errors_map_to_400() {
        // 客户端校验错误必须映射为 400（而非 5xx），否则会被 provider 当作上游
        // 瞬态错误触发冷却，放大成 503 风暴。识别逻辑集中在 endpoint 层。
        for needle in [
            // 精确 reason（provider 错误串里嵌着上游 body）
            "非流式 API 请求失败: 500 {\"reason\":\"TOOL_USE_RESULT_MISMATCH\"}",
            // message 级特异短语（纯文本报文）
            "Expected toolResult blocks but found none",
        ] {
            let resp = map_provider_error(anyhow::anyhow!(needle.to_string()));
            assert_eq!(
                resp.status(),
                StatusCode::BAD_REQUEST,
                "错误串 `{needle}` 应映射为 400"
            );
        }
    }

    #[test]
    fn generic_upstream_error_still_maps_to_502() {
        // 回归：普通上游错误不应被新分支误伤，仍应是 502 BAD_GATEWAY。
        let resp = map_provider_error(anyhow::anyhow!("connection reset by peer"));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        // 回归：宽泛的 ValidationException 不再被当作客户端校验错误而误判为 400，
        // 仍按上游错误走 502（避免把可重试故障误杀）。
        let resp = map_provider_error(anyhow::anyhow!(
            "ValidationException: transient backend issue".to_string()
        ));
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn non_stream_native_thinking_precedes_redacted_and_text() {
        let content = build_non_stream_content(
            true,
            "final answer".to_string(),
            "native thinking".to_string(),
            Some("real-signature".to_string()),
            vec!["encrypted-thinking".to_string()],
        );

        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "native thinking");
        assert_eq!(content[0]["signature"], "real-signature");
        assert_eq!(content[1]["type"], "redacted_thinking");
        assert_eq!(content[1]["data"], "encrypted-thinking");
        assert_eq!(content[2]["type"], "text");
        assert_eq!(content[2]["text"], "final answer");
    }

    #[test]
    fn non_stream_legacy_thinking_extraction_still_works_without_native_reasoning() {
        let content = build_non_stream_content(
            true,
            "<thinking>legacy thinking</thinking>\n\nfinal answer".to_string(),
            String::new(),
            None,
            Vec::new(),
        );

        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["thinking"], "legacy thinking");
        assert_eq!(
            content[0]["signature"],
            crate::anthropic::stream::THINKING_SIGNATURE_PLACEHOLDER
        );
        assert_eq!(content[1]["type"], "text");
        assert_eq!(content[1]["text"], "final answer");
    }

    #[test]
    fn non_stream_native_thinking_downgrades_to_text_when_thinking_disabled() {
        let content = build_non_stream_content(
            false,
            String::new(),
            "native thinking fallback".to_string(),
            Some("ignored-signature".to_string()),
            vec!["ignored-redacted".to_string()],
        );

        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "native thinking fallback");
    }

    #[test]
    fn available_models_include_opus_4_7_variants() {
        let models = available_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-4-7"));
        assert!(ids.contains(&"claude-opus-4-7-thinking"));
    }

    #[test]
    fn available_models_include_native_kiro_models() {
        let models = available_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"auto"));
        assert!(ids.contains(&"deepseek-3.2"));
        assert!(ids.contains(&"minimax-m2.5"));
        assert!(ids.contains(&"minimax-m2.1"));
        assert!(ids.contains(&"glm-5"));
        assert!(ids.contains(&"qwen3-coder-next"));
        assert!(ids.contains(&"claude-sonnet-4.6"));
        assert!(ids.contains(&"claude-opus-4.8"));
    }

    #[test]
    fn available_models_have_unique_ids() {
        let models = available_models();
        let mut seen = std::collections::HashSet::new();

        for model in models {
            assert!(
                seen.insert(model.id.clone()),
                "duplicate model id: {}",
                model.id
            );
        }
    }

    #[test]
    fn count_image_budget_handles_empty() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": []
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
        assert_eq!(stats.total_b64_bytes, 0);
        assert_eq!(stats.largest_b64_bytes, 0);
    }

    #[test]
    fn count_image_budget_counts_inline_base64() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hi"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA1111"}},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBBBBBBBB"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#).unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 2);
        assert_eq!(stats.total_b64_bytes, 18);
        assert_eq!(stats.largest_b64_bytes, 10);
    }

    #[test]
    fn count_image_budget_skips_url_only_images() {
        let req: super::super::types::MessagesRequest = serde_json::from_str(
            r#"{
            "model": "claude-opus-4-7",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"type": "url", "url": "https://example.com/x.png"}}
                ]
            }]
        }"#,
        )
        .unwrap();
        let stats = count_image_budget(&req);
        assert_eq!(stats.count, 0);
    }

    #[test]
    fn available_models_include_4_8_variants() {
        let models = available_models();
        let ids: Vec<&str> = models.iter().map(|model| model.id.as_str()).collect();

        assert!(ids.contains(&"claude-opus-4-8"));
        assert!(ids.contains(&"claude-opus-4-8-thinking"));
        assert!(ids.contains(&"claude-sonnet-4-8"));
        assert!(ids.contains(&"claude-sonnet-4-8-thinking"));
    }
}
