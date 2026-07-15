//! Anthropic API Handler 函数

use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::admin::client_keys::{ClientResponseMode, SharedClientKeyManager};
use crate::admin::trace_db::{
    SharedTraceStore, TraceAttempt, TraceDiagnosticEvent, TraceKeySource, TraceRecord, TraceSink,
    outcome,
};
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder, UsageRecord};
use crate::kiro::image_budget::{ImageBudgetError, PreparedKiroBodies, prepare_kiro_bodies};
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
use sha2::{Digest as _, Sha256};
use std::time::Duration;
use tokio::time::{Instant as TokioInstant, interval};
use uuid::Uuid;

use super::converter::{ConversionError, convert_request_with_mode};
use super::middleware::{AppState, KeyContext};
use super::stream::{BufferedStreamContext, SseEvent, StreamContext};
use super::tool_attempt::{AttemptTermination, ProbationBuffer};
use super::types::{
    CountTokensRequest, CountTokensResponse, ErrorResponse, MessagesRequest, ModelsResponse,
    OutputConfig, Thinking,
};
use super::websearch;

fn detection_only<T>(mode: ClientResponseMode, action: impl FnOnce() -> Option<T>) -> Option<T> {
    mode.allows_detection_shortcuts().then(action).flatten()
}

fn effective_identity_normalization(globally_enabled: bool, mode: ClientResponseMode) -> bool {
    globally_enabled && mode.allows_identity_normalization()
}

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
    snapshot: Option<std::sync::Arc<super::error_snapshot::ErrorSnapshotContext>>,
    finalized: std::sync::atomic::AtomicBool,
    trace_id: String,
    ts: String,
    key_id: u64,
    key_source: TraceKeySource,
    response_mode: ClientResponseMode,
    model: String,
    is_stream: bool,
    /// 本次请求实际下发的思考档位（low/medium/high/xhigh/max）；未启用/不支持为 None。
    reasoning_effort: parking_lot::Mutex<Option<String>>,
    /// 是否声明 1M 扩展上下文（客户端带 `anthropic-beta: context-1m-...` 头）。
    context_1m: bool,
    /// 客户端是否请求了推理（thinking 启用 或 显式 effort）；与档位独立。
    thinking: bool,
    /// 是否对精确空 user 请求应用了最小兼容文本。
    empty_user_compat_applied: std::sync::atomic::AtomicBool,
    started_at: Instant,
    /// 首个客户端可见内容事件产出时刻（仅流式标记；取第一次）
    first_token_at: parking_lot::Mutex<Option<Instant>>,
    /// 首个 Kiro 原始 body chunk 到达时刻（仅流式标记；取第一次）
    upstream_first_byte_at: parking_lot::Mutex<Option<Instant>>,
    attempts: parking_lot::Mutex<Vec<TraceAttempt>>,
}

/// 本次请求的用量快照（落入 trace 行，与 usage_log 同源）
#[derive(Debug, Clone, Copy, Default)]
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
    fn new(
        state: &AppState,
        options: RequestTraceOptions,
        headers: &HeaderMap,
        request: &MessagesRequest,
    ) -> Self {
        let trace_id = Uuid::new_v4().to_string();
        let snapshot = state.error_snapshot_store.as_ref().and_then(|store| {
            super::error_snapshot::ErrorSnapshotContext::new_if_enabled(
                store.clone(),
                trace_id.clone(),
                &options.key_ctx,
                headers,
                request,
            )
            .map(std::sync::Arc::new)
        });
        Self {
            store: state.trace_store.clone(),
            snapshot,
            finalized: std::sync::atomic::AtomicBool::new(false),
            trace_id,
            ts: Utc::now().to_rfc3339(),
            key_id: options.key_ctx.key_id,
            key_source: options.key_ctx.key_source,
            response_mode: options.key_ctx.response_mode,
            model: options.model,
            is_stream: options.is_stream,
            reasoning_effort: parking_lot::Mutex::new(options.reasoning_effort),
            context_1m: options.context_1m,
            thinking: options.thinking,
            empty_user_compat_applied: std::sync::atomic::AtomicBool::new(false),
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

    pub fn set_reasoning_effort(&self, value: Option<String>) {
        *self.reasoning_effort.lock() = value;
    }

    pub fn mark_empty_user_compat_applied(&self) {
        self.empty_user_compat_applied
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    pub fn record_protocol_error(&self, error_type: &str, message: &str) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_internal_error(error_type, message);
        }
    }

    pub fn record_tool_schema_failure(
        &self,
        failure: &super::tool_schema::ToolSchemaFailure,
        attempt: u8,
    ) {
        let safe_summary = failure.safe_summary(attempt);
        tracing::warn!(
            attempt,
            tool = %failure.tool_name(),
            summary = %safe_summary,
            "上游工具参数 Schema 校验失败（仅记录安全形状摘要）"
        );
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_tool_schema_failure(&safe_summary);
        }
    }

    pub fn record_local_error(&self, status: StatusCode, error_type: &str, message: &str) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_attempt_status(0, Some(status.as_u16()), "local_error");
            snapshot.record_internal_error(error_type, message);
        }
    }

    pub fn record_stream_chunk(&self, chunk: &[u8]) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_stream_chunk(chunk);
        }
    }

    pub fn record_upstream_body(&self, attempt: u32, body: &[u8]) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_upstream_body(attempt, body);
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
        if self
            .finalized
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            return;
        }
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
        let snapshot_id = self.snapshot.as_ref().and_then(|snapshot| {
            let state = super::error_snapshot::SnapshotFinalState {
                final_status: final_status.to_string(),
                error_type: error_type.map(str::to_string),
                error_message: error_message.map(str::to_string),
                http_status: attempts.last().and_then(|attempt| attempt.http_status),
                interrupted_after_bytes,
            };
            match snapshot.finalize(state) {
                Ok(id) => id,
                Err(error) => {
                    tracing::error!(%error, trace_id = %self.trace_id, "持久化错误快照失败");
                    None
                }
            }
        });
        let Some(store) = &self.store else { return };
        let rec = TraceRecord {
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            key_id: self.key_id,
            key_source: self.key_source,
            response_mode: self.response_mode,
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
            reasoning_effort: self.reasoning_effort.lock().clone(),
            context_1m: self.context_1m,
            thinking: self.thinking,
            empty_user_compat_applied: self
                .empty_user_compat_applied
                .load(std::sync::atomic::Ordering::Relaxed),
            snapshot_id,
            attempts,
        };
        store.insert(&rec);
    }
}

impl TraceSink for RequestTracer {
    fn on_attempt(&self, attempt: TraceAttempt) {
        if let Some(snapshot) = &self.snapshot {
            snapshot.record_attempt_status(attempt.attempt, attempt.http_status, &attempt.outcome);
        }
        self.attempts.lock().push(attempt);
    }

    fn on_diagnostic(&self, event: TraceDiagnosticEvent<'_>) {
        let Some(snapshot) = &self.snapshot else {
            return;
        };
        match event {
            TraceDiagnosticEvent::UpstreamRequest {
                attempt,
                credential_id,
                endpoint,
                body,
            } => {
                snapshot.record_kiro_request(attempt, credential_id, endpoint, body);
            }
            TraceDiagnosticEvent::UpstreamResponse {
                attempt,
                credential_id,
                endpoint,
                status,
                body,
            } => {
                snapshot.record_upstream_response(attempt, credential_id, endpoint, status, body);
            }
            TraceDiagnosticEvent::NetworkError {
                attempt,
                credential_id,
                endpoint,
                message,
            } => {
                snapshot.record_network_error(attempt, credential_id, endpoint, message);
            }
        }
    }
}

fn finalize_immediate_response(tracer: &RequestTracer, response: &Response, error_type: &str) {
    if response.status().is_success() {
        tracer.finalize("success", None, None, None, TraceUsage::zero());
    } else {
        let message = format!(
            "local response returned HTTP {} {}",
            response.status().as_u16(),
            response.status().canonical_reason().unwrap_or("error")
        );
        finalize_immediate_error(tracer, response.status(), error_type, &message);
    }
}

fn finalize_immediate_error(
    tracer: &RequestTracer,
    status: StatusCode,
    error_type: &str,
    message: &str,
) {
    tracer.record_local_error(status, error_type, message);
    tracer.finalize(
        "error",
        Some(error_type),
        Some(message),
        None,
        TraceUsage::zero(),
    );
}

fn finalize_client_disconnected(tracer: &RequestTracer, received_bytes: u64, usage: TraceUsage) {
    const MESSAGE: &str = "client disconnected before the response stream completed";
    tracer.record_protocol_error("client_disconnected", MESSAGE);
    tracer.finalize(
        "interrupted",
        Some("client_disconnected"),
        Some(MESSAGE),
        Some(received_bytes),
        usage,
    );
}

fn record_strict_json_recovery(tracer: &RequestTracer, attempts: usize) {
    if attempts > 1 {
        tracer.record_protocol_error(
            "structured_output_retry",
            "the first structured output attempt was invalid and a retry recovered",
        );
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
            public_message: "The upstream request body is too large. Reduce image count, attachment size, or conversation history.",
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

fn local_text_stream_chunks(events: Vec<SseEvent>) -> Vec<Bytes> {
    events
        .into_iter()
        .map(|event| Bytes::from(event.to_sse_string()))
        .collect()
}

const LOCAL_SSE_EVENT_DELAY: Duration = Duration::from_millis(2);

fn local_text_stream_response(events: Vec<SseEvent>) -> Response {
    let body_stream = stream::unfold(
        (local_text_stream_chunks(events).into_iter(), true),
        |(mut chunks, first)| async move {
            let chunk = chunks.next()?;
            if !first {
                tokio::time::sleep(LOCAL_SSE_EVENT_DELAY).await;
            }
            Some((Ok::<_, Infallible>(chunk), (chunks, false)))
        },
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header(header::CONNECTION, "keep-alive")
        .body(Body::from_stream(body_stream))
        .unwrap()
}

#[derive(Debug)]
struct BufferedAttempt {
    events: Vec<SseEvent>,
    credential_id: u64,
    usage: TraceUsage,
    credits: f64,
    terminal_error: Option<String>,
    attempt_failure: Option<super::tool_attempt::AttemptFailure>,
}

#[derive(Debug)]
struct StrictJsonRecovery {
    json: String,
    attempts: Vec<BufferedAttempt>,
}

#[derive(Debug)]
struct StrictJsonRecoveryFailure {
    attempts: Vec<BufferedAttempt>,
    source: Option<anyhow::Error>,
    terminal_failure: Option<super::tool_attempt::AttemptFailure>,
}

fn strict_json_from_events(events: &[SseEvent]) -> Option<String> {
    let text = visible_text_from_events(events)?;
    super::exact_output::extract_single_json(&text)
}

fn visible_text_from_events(events: &[SseEvent]) -> Option<String> {
    if events.iter().any(|event| {
        event.event == "content_block_start" && event.data["content_block"]["type"] == "tool_use"
    }) {
        return None;
    }
    Some(
        events
            .iter()
            .filter(|event| event.event == "content_block_delta")
            .filter(|event| event.data["delta"]["type"] == "text_delta")
            .filter_map(|event| event.data["delta"]["text"].as_str())
            .collect::<String>(),
    )
}

#[cfg(test)]
async fn recover_strict_json_attempts<F, Fut>(
    collect: F,
) -> Result<StrictJsonRecovery, StrictJsonRecoveryFailure>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = anyhow::Result<BufferedAttempt>>,
{
    recover_strict_json_attempts_with_validator(collect, false, |_| true).await
}

async fn recover_strict_json_attempts_with_validator<F, Fut, V>(
    mut collect: F,
    require_exact_json: bool,
    mut validate: V,
) -> Result<StrictJsonRecovery, StrictJsonRecoveryFailure>
where
    F: FnMut(usize) -> Fut,
    Fut: Future<Output = anyhow::Result<BufferedAttempt>>,
    V: FnMut(&str) -> bool,
{
    let mut attempts = Vec::with_capacity(2);
    for attempt_index in 0..2 {
        let attempt = match collect(attempt_index).await {
            Ok(attempt) => attempt,
            Err(source) => {
                return Err(StrictJsonRecoveryFailure {
                    attempts,
                    source: Some(source),
                    terminal_failure: None,
                });
            }
        };
        let terminal_failure = attempt.attempt_failure.clone().filter(|failure| {
            matches!(
                failure,
                super::tool_attempt::AttemptFailure::ContextWindowExceeded
                    | super::tool_attempt::AttemptFailure::UpstreamError { .. }
            )
        });
        let json = attempt
            .terminal_error
            .is_none()
            .then(|| {
                if require_exact_json {
                    visible_text_from_events(&attempt.events)
                        .and_then(|text| super::structured_output::extract_output_json(&text))
                } else {
                    strict_json_from_events(&attempt.events)
                }
            })
            .flatten();
        attempts.push(attempt);
        if terminal_failure.is_some() {
            return Err(StrictJsonRecoveryFailure {
                attempts,
                source: None,
                terminal_failure,
            });
        }
        if let Some(json) = json.filter(|json| validate(json)) {
            return Ok(StrictJsonRecovery { json, attempts });
        }
    }
    let terminal_failure = attempts
        .last()
        .and_then(|attempt| attempt.attempt_failure.clone());
    Err(StrictJsonRecoveryFailure {
        attempts,
        source: None,
        terminal_failure,
    })
}

async fn collect_buffered_attempt(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    threshold_retry_body: Option<String>,
    model: String,
    input_tokens: i32,
    context_window_size: i32,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    attempt_index: usize,
    identity_normalization: bool,
) -> anyhow::Result<BufferedAttempt> {
    let call_result = provider
        .call_api_with_content_length_retry(
            &request_body,
            threshold_retry_body.as_deref(),
            Some(tracer.as_ref()),
            group.as_deref(),
        )
        .await?;
    let credential_id = call_result.credential_id;
    let body = call_result.response.bytes().await?;
    tracer.mark_upstream_first_byte();
    tracer.record_stream_chunk(&body);
    tracer.record_upstream_body(attempt_index as u32, &body);

    let mut decoder = EventStreamDecoder::new();
    if let Err(error) = decoder.feed(&body) {
        tracing::warn!(error = %error, "strict JSON attempt decoder buffer overflow");
        tracer.record_protocol_error("sse_state_error", &error.to_string());
    }
    let mut context = BufferedStreamContext::new_with_constraints(
        model,
        input_tokens,
        false,
        false,
        std::collections::HashMap::new(),
        std::collections::HashSet::new(),
        super::converter::ToolChoicePolicy::Auto {
            disable_parallel_tool_use: false,
        },
    );
    context.set_context_window_size(context_window_size);
    context.set_cache_usage(cache_usage);
    if identity_normalization {
        context.enable_identity_filter();
    }
    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => match Event::from_frame(frame) {
                Ok(event) => context.process_and_buffer(&event),
                Err(error) => {
                    tracing::warn!(error = %error, "strict JSON attempt event decode failed");
                    tracer.record_protocol_error("sse_state_error", &error.to_string());
                }
            },
            Err(error) => {
                tracing::warn!(error = %error, "strict JSON attempt frame decode failed");
                tracer.record_protocol_error("sse_state_error", &error.to_string());
            }
        }
    }
    let events = context.finish_and_get_all_events();
    let terminal_error = context.terminal_error_message();
    let attempt_failure = context.terminal_attempt_failure().cloned();
    let (input, output, creation, read, credits) = context.final_usage();
    Ok(BufferedAttempt {
        events,
        credential_id,
        usage: TraceUsage {
            input_tokens: input.max(0) as u64,
            output_tokens: output.max(0) as u64,
            cache_creation_tokens: creation.max(0) as u64,
            cache_read_tokens: read.max(0) as u64,
            credits: if credits.is_finite() && credits > 0.0 {
                credits
            } else {
                0.0
            },
        },
        credits,
        terminal_error,
        attempt_failure,
    })
}

fn strict_json_route_allowed(payload: &MessagesRequest) -> bool {
    let has_structured_format = payload
        .output_config
        .as_ref()
        .and_then(|config| config.format.as_ref())
        .is_some();
    if !(has_structured_format || super::exact_output::strict_json_requested(payload))
        || payload
            .tools
            .as_ref()
            .is_some_and(|tools| !tools.is_empty())
        || payload.tool_choice.is_some()
        || payload.thinking.as_ref().is_some_and(Thinking::is_enabled)
        || websearch::has_web_search_tool(payload)
        || websearch::has_web_search_among_tools(payload)
    {
        return false;
    }
    !payload.messages.iter().any(|message| {
        message.content.as_array().is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block.get("type").and_then(serde_json::Value::as_str) == Some("document")
            })
        })
    })
}

struct StrictJsonRequestBodies {
    bodies: [String; 2],
    threshold_retry_bodies: [Option<String>; 2],
}

fn prepare_strict_json_request_bodies(
    request_body: &str,
    threshold_retry_body: Option<&str>,
    structured_format: Option<&super::types::OutputFormat>,
) -> Option<StrictJsonRequestBodies> {
    if let Some(format) = structured_format {
        let retry =
            super::exact_output::append_structured_output_instruction(request_body, format)?;
        let threshold_retry = threshold_retry_body.and_then(|body| {
            super::exact_output::append_structured_output_instruction(body, format)
        });
        return Some(StrictJsonRequestBodies {
            bodies: [request_body.to_owned(), retry],
            threshold_retry_bodies: [threshold_retry_body.map(str::to_owned), threshold_retry],
        });
    }

    Some(StrictJsonRequestBodies {
        bodies: [
            request_body.to_owned(),
            super::exact_output::append_strict_json_retry_instruction(request_body)?,
        ],
        threshold_retry_bodies: [
            threshold_retry_body.map(str::to_owned),
            threshold_retry_body
                .and_then(super::exact_output::append_strict_json_retry_instruction),
        ],
    })
}

async fn handle_strict_json_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    threshold_retry_body: Option<&str>,
    payload: &MessagesRequest,
    input_tokens: i32,
    context_window_size: i32,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    identity_normalization: bool,
) -> Response {
    let structured_format = payload
        .output_config
        .as_ref()
        .and_then(|config| config.format.as_ref());
    let Some(prepared) =
        prepare_strict_json_request_bodies(request_body, threshold_retry_body, structured_format)
    else {
        hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
        let (status, error_type, internal_message, client_message) = if structured_format.is_some()
        {
            (
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
                "invalid or oversized structured output format",
                "Invalid or oversized output_config.format",
            )
        } else {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "failed to prepare strict JSON retry",
                "Failed to prepare strict JSON request",
            )
        };
        finalize_immediate_error(tracer.as_ref(), status, error_type, internal_message);
        return (status, Json(ErrorResponse::new(error_type, client_message))).into_response();
    };
    let StrictJsonRequestBodies {
        bodies,
        threshold_retry_bodies,
    } = prepared;
    let model = payload.model.clone();
    let recovery = recover_strict_json_attempts_with_validator(
        |attempt_index| {
            let provider = provider.clone();
            let (body, threshold_retry_body) = if attempt_index == 0 {
                (bodies[0].clone(), threshold_retry_bodies[0].clone())
            } else {
                (
                    threshold_retry_bodies[1]
                        .clone()
                        .unwrap_or_else(|| bodies[1].clone()),
                    None,
                )
            };
            let model = model.clone();
            let tracer = tracer.clone();
            let group = group.clone();
            async move {
                collect_buffered_attempt(
                    provider,
                    body,
                    threshold_retry_body,
                    model,
                    input_tokens,
                    context_window_size,
                    cache_usage,
                    tracer,
                    group,
                    attempt_index,
                    identity_normalization,
                )
                .await
            }
        },
        structured_format.is_some(),
        |json| {
            structured_format.map_or_else(
                || super::exact_output::json_satisfies_explicit_constraints(payload, json),
                |format| super::structured_output::validate_output_json(json, format).is_ok(),
            )
        },
    )
    .await;

    let (final_input, cache_creation, cache_read) =
        split_non_stream_usage(input_tokens, None, &cache_usage);
    match recovery {
        Ok(recovered) => {
            let credential_id = recovered
                .attempts
                .last()
                .map(|attempt| attempt.credential_id)
                .unwrap_or(0);
            let credits = recovered
                .attempts
                .iter()
                .map(|attempt| attempt.credits.max(0.0))
                .sum::<f64>();
            let internal_output_tokens = recovered
                .attempts
                .iter()
                .map(|attempt| attempt.usage.output_tokens)
                .sum::<u64>();
            let output_tokens = token::count_tokens(&recovered.json).max(1) as i32;
            let trace_usage = TraceUsage {
                input_tokens: final_input.max(0) as u64,
                output_tokens: output_tokens.max(0) as u64,
                cache_creation_tokens: cache_creation.max(0) as u64,
                cache_read_tokens: cache_read.max(0) as u64,
                credits,
            };
            tracing::debug!(
                attempts = recovered.attempts.len(),
                output_bytes = recovered.json.len(),
                internal_output_tokens,
                "recovered strict JSON response"
            );
            hook.record(
                credential_id,
                final_input,
                output_tokens,
                cache_creation,
                cache_read,
                credits,
                "success",
            );
            record_strict_json_recovery(tracer.as_ref(), recovered.attempts.len());
            tracer.mark_first_token();
            tracer.finalize("success", None, None, None, trace_usage);

            if payload.stream {
                local_text_stream_response(build_local_text_stream_events(
                    &payload.model,
                    &recovered.json,
                    input_tokens,
                    cache_usage,
                ))
            } else {
                (
                    StatusCode::OK,
                    Json(build_local_text_message(
                        &payload.model,
                        &recovered.json,
                        input_tokens,
                        &cache_usage,
                    )),
                )
                    .into_response()
            }
        }
        Err(failure) => {
            let credential_id = failure
                .attempts
                .last()
                .map(|attempt| attempt.credential_id)
                .unwrap_or(0);
            let credits = failure
                .attempts
                .iter()
                .map(|attempt| attempt.credits.max(0.0))
                .sum::<f64>();
            let trace_usage = TraceUsage {
                input_tokens: final_input.max(0) as u64,
                output_tokens: 0,
                cache_creation_tokens: cache_creation.max(0) as u64,
                cache_read_tokens: cache_read.max(0) as u64,
                credits,
            };
            hook.record(
                credential_id,
                final_input,
                0,
                cache_creation,
                cache_read,
                credits,
                "error",
            );
            if let Some(source) = failure.source {
                let attempt_outcome = last_attempt_outcome(&tracer);
                tracer.record_protocol_error("structured_output_error", &source.to_string());
                tracer.finalize(
                    "error",
                    attempt_outcome,
                    Some(&source.to_string()),
                    None,
                    trace_usage,
                );
                return map_provider_error(source);
            }

            if let Some(attempt_failure) = failure.terminal_failure {
                let (error_type, message) = attempt_failure.public_error();
                tracer.record_protocol_error(error_type, &message);
                tracer.finalize(
                    "error",
                    Some(outcome::BAD_REQUEST),
                    Some(&message),
                    None,
                    trace_usage,
                );
                if payload.stream {
                    let body = SseEvent::new(
                        "error",
                        json!({
                            "type": "error",
                            "error": {"type": error_type, "message": message}
                        }),
                    )
                    .to_sse_string();
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .body(Body::from(body))
                        .unwrap();
                }
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response();
            }

            let (error_type, message) = if structured_format.is_some() {
                (
                    "upstream_structured_output_error",
                    "Upstream did not produce JSON matching output_config.format after one retry",
                )
            } else {
                (
                    "upstream_json_protocol_error",
                    "Upstream did not produce one complete JSON value after one retry",
                )
            };
            tracing::warn!(
                attempts = failure.attempts.len(),
                "strict JSON recovery exhausted"
            );
            tracer.record_protocol_error("structured_output_error", message);
            tracer.finalize(
                "error",
                Some(outcome::BAD_REQUEST),
                Some(message),
                None,
                trace_usage,
            );
            if payload.stream {
                let body = SseEvent::new(
                    "error",
                    json!({
                        "type": "error",
                        "error": {
                            "type": error_type,
                            "message": message
                        }
                    }),
                )
                .to_sse_string();
                Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "text/event-stream")
                    .header(header::CACHE_CONTROL, "no-cache")
                    .header(header::CONNECTION, "keep-alive")
                    .body(Body::from(body))
                    .unwrap()
            } else {
                (
                    StatusCode::BAD_GATEWAY,
                    Json(ErrorResponse::new(error_type, message)),
                )
                    .into_response()
            }
        }
    }
}

fn local_exact_system_output(
    payload: &MessagesRequest,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<super::exact_output::ExactOutput> {
    let output = super::exact_output::exact_system_output(payload, mode)?;
    let output_tokens = token::count_tokens(output.as_str()).max(1) as i32;
    (output_tokens <= payload.max_tokens.max(0)).then_some(output)
}

#[cfg(test)]
fn local_exact_system_answer(
    payload: &MessagesRequest,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<String> {
    local_exact_system_output(payload, mode).map(|output| output.as_str().to_owned())
}

fn try_local_exact_system_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<Response> {
    let output = local_exact_system_output(payload, mode)?;
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
        Some(local_text_stream_response(build_local_text_stream_events(
            &payload.model,
            answer,
            input_tokens,
            cache_usage,
        )))
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

fn local_exact_user_answer(
    payload: &MessagesRequest,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<String> {
    if !local_document_system_is_safe_to_bypass(payload, mode) {
        return None;
    }
    let answer = super::exact_output::exact_user_echo(payload)?;
    let output_tokens = token::count_tokens(&answer).max(1) as i32;
    (output_tokens <= payload.max_tokens.max(0)).then_some(answer)
}

fn try_local_exact_user_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
    mode: crate::model::config::ToolCompatibilityMode,
) -> Option<Response> {
    let answer = local_exact_user_answer(payload, mode)?;
    let output_tokens = token::count_tokens(&answer).max(1) as i32;
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
        output_bytes = answer.len(),
        input_tokens = final_input_tokens,
        output_tokens,
        stream = payload.stream,
        "served bounded explicit user echo locally"
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
        Some(local_text_stream_response(build_local_text_stream_events(
            &payload.model,
            &answer,
            input_tokens,
            cache_usage,
        )))
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

fn try_local_ping_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
) -> Option<Response> {
    let answer = super::exact_output::local_ping_answer(payload, provider.local_ping_response())?;
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

    tracing::debug!(
        input_tokens = final_input_tokens,
        output_tokens,
        stream = payload.stream,
        "served bounded ping health response locally"
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
        Some(local_text_stream_response(build_local_text_stream_events(
            &payload.model,
            answer,
            input_tokens,
            cache_usage,
        )))
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

fn try_local_model_profile_response(
    state: &AppState,
    provider: &crate::kiro::provider::KiroProvider,
    payload: &MessagesRequest,
    hook: &UsageRecordHook,
) -> Option<Response> {
    let store = state.model_profiles.as_ref()?;
    let mapped_model = state
        .model_mappings
        .as_ref()
        .and_then(|mappings| mappings.resolve(&payload.model))
        .unwrap_or_else(|| payload.model.clone());
    let profile = store.resolve(&mapped_model);
    let answer = super::model_profile_answer::exact_model_profile_answer(
        payload,
        &profile,
        store.exact_answers_enabled(),
    )?;
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
    hook.record(
        0,
        final_input_tokens,
        output_tokens,
        cache_creation_tokens,
        cache_read_tokens,
        0.0,
        "success",
    );
    tracing::info!(
        model_id = %profile.model_id,
        stream = payload.stream,
        "served strict model profile answer locally"
    );
    if payload.stream {
        Some(local_text_stream_response(build_local_text_stream_events(
            &payload.model,
            &answer,
            input_tokens,
            cache_usage,
        )))
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

fn request_context_window_size(state: &AppState, payload: &MessagesRequest) -> i32 {
    let mapped_model = state
        .model_mappings
        .as_ref()
        .and_then(|mappings| mappings.resolve(&payload.model))
        .unwrap_or_else(|| payload.model.clone());
    state
        .model_profiles
        .as_ref()
        .and_then(|store| store.resolve(&mapped_model).context_window_tokens)
        .and_then(|value| i32::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or_else(|| super::converter::get_context_window_size(&mapped_model))
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
        Some(local_text_stream_response(build_local_text_stream_events(
            &payload.model,
            &answer,
            input_tokens,
            cache_usage,
        )))
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

/// GET /v1/models
///
/// 返回可用的模型列表
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!(group = ?key_ctx.group, "Received GET /v1/models request");

    let Some(provider) = state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "no_available_credentials",
                "No Kiro provider is configured.",
            )),
        )
            .into_response();
    };

    match provider.available_models(key_ctx.group.as_deref()).await {
        Ok(upstream) => Json(ModelsResponse {
            object: "list".to_string(),
            data: super::model_catalog::public_models(upstream),
        })
        .into_response(),
        Err(crate::kiro::model_catalog::ModelCatalogError::NoAvailableCredentials) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "no_available_credentials",
                "No enabled credentials are available for this key group.",
            )),
        )
            .into_response(),
        Err(crate::kiro::model_catalog::ModelCatalogError::UpstreamModelCatalog { failures }) => {
            tracing::error!(failures, "全部动态模型目录查询失败且没有可用缓存");
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "upstream_model_catalog_error",
                    "Upstream model catalog is temporarily unavailable.",
                )),
            )
                .into_response()
        }
    }
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

fn conversion_error_trace_type(error: &ConversionError) -> &'static str {
    if matches!(error, ConversionError::InvalidImage { .. }) {
        "image_validation_error"
    } else {
        "request_conversion_error"
    }
}

struct ImageBudgetFailureDetails {
    status: StatusCode,
    error_type: &'static str,
    safe_message: String,
    client_error_type: &'static str,
    client_message: &'static str,
}

fn image_budget_failure_details(error: &ImageBudgetError) -> ImageBudgetFailureDetails {
    match error {
        ImageBudgetError::Exceeded {
            count,
            history_count,
            current_count,
            before,
            after,
            soft_limit,
            hard_limit,
        } => ImageBudgetFailureDetails {
            status: StatusCode::BAD_REQUEST,
            error_type: "image_budget_exceeded",
            safe_message: format!(
                "image budget exceeded: count={count}, history={history_count}, current={current_count}, before={before}, after={after}, soft={soft_limit}, hard={hard_limit}"
            ),
            client_error_type: "invalid_request_error",
            client_message: "Image payload exceeds the configured upstream hard limit after compressing historical images. Reduce images or start a new conversation.",
        },
        ImageBudgetError::InvalidPolicy(_) | ImageBudgetError::Serialization(_) => {
            ImageBudgetFailureDetails {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                error_type: "request_body_error",
                safe_message: "failed to prepare the upstream image request body".to_string(),
                client_error_type: "internal_error",
                client_message: "Failed to prepare the upstream request body.",
            }
        }
    }
}

struct OutboundKiroBodyError {
    response: Response,
    status: StatusCode,
    error_type: &'static str,
    safe_message: String,
}

fn prepare_outbound_kiro_bodies(
    request: &KiroRequest,
    provider: &crate::kiro::provider::KiroProvider,
) -> Result<PreparedKiroBodies, OutboundKiroBodyError> {
    match prepare_kiro_bodies(request, provider.image_budget_policy()) {
        Ok(prepared) => {
            tracing::info!(
                image_count = prepared.primary_stats.image_count,
                history_image_count = prepared.primary_stats.history_image_count,
                current_image_count = prepared.primary_stats.current_image_count,
                image_before_b64_kb = prepared.primary_stats.before_base64_bytes / 1024,
                image_after_b64_kb = prepared.primary_stats.after_base64_bytes / 1024,
                resized_history_images = prepared.primary_stats.resized_history_images,
                threshold_retry_available = prepared.threshold_retry_body.is_some(),
                threshold_retry_b64_kb = prepared
                    .retry_stats
                    .map(|stats| stats.after_base64_bytes / 1024),
                "Kiro 出站图片预算处理完成"
            );
            Ok(prepared)
        }
        Err(error) => {
            let details = image_budget_failure_details(&error);
            if details.status.is_client_error() {
                tracing::warn!(
                    error_type = details.error_type,
                    diagnostic = %details.safe_message,
                    "图片总量预检失败"
                );
            } else {
                tracing::error!(%error, "准备 Kiro 图片预算请求体失败");
            }
            let response = (
                details.status,
                Json(ErrorResponse::new(
                    details.client_error_type,
                    details.client_message,
                )),
            )
                .into_response();
            Err(OutboundKiroBodyError {
                response,
                status: details.status,
                error_type: details.error_type,
                safe_message: details.safe_message,
            })
        }
    }
}

pub(crate) const EMPTY_USER_MESSAGE_ERROR: &str = "The only user message is empty. Add user text, a tool result, an image, or a document; or enable emptyUserMessageCompat in the admin settings.";

fn has_non_empty_system(payload: &MessagesRequest) -> bool {
    payload
        .system
        .as_ref()
        .is_some_and(|blocks| blocks.iter().any(|block| !block.text.trim().is_empty()))
}

fn is_empty_text_only_content(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(text) => text.trim().is_empty(),
        serde_json::Value::Array(blocks) => {
            !blocks.is_empty()
                && blocks.iter().all(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        && block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|text| text.trim().is_empty())
                })
        }
        _ => false,
    }
}

fn is_effectively_empty_user_content(content: &serde_json::Value) -> bool {
    match content {
        serde_json::Value::String(text) => text.trim().is_empty(),
        serde_json::Value::Array(blocks) => {
            blocks.is_empty()
                || blocks.iter().all(|block| {
                    block.get("type").and_then(serde_json::Value::as_str) == Some("text")
                        && block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|text| text.trim().is_empty())
                })
        }
        _ => false,
    }
}

fn current_user_message_is_effectively_empty(payload: &MessagesRequest) -> bool {
    payload.messages.last().is_some_and(|message| {
        message.role == "user" && is_effectively_empty_user_content(&message.content)
    })
}

fn is_exact_empty_user_message_shape(payload: &MessagesRequest) -> bool {
    has_non_empty_system(payload)
        && payload.messages.len() == 1
        && payload.messages[0].role == "user"
        && is_empty_text_only_content(&payload.messages[0].content)
        && payload.tools.as_ref().map_or(true, Vec::is_empty)
        && payload.tool_choice.is_none()
}

/// 对精确的空 user 请求形状执行本地拒绝或最小兼容改写。
fn apply_empty_user_message_compat(
    payload: &mut MessagesRequest,
    enabled: bool,
) -> Result<bool, &'static str> {
    if !current_user_message_is_effectively_empty(payload) {
        return Ok(false);
    }
    if enabled && is_exact_empty_user_message_shape(payload) {
        payload.messages[0].content = serde_json::Value::String("Continue.".to_string());
        return Ok(true);
    }
    Err(EMPTY_USER_MESSAGE_ERROR)
}

fn handle_empty_user_message(
    state: &AppState,
    payload: &mut MessagesRequest,
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
) -> Option<Response> {
    let enabled = state
        .kiro_provider
        .as_ref()
        .is_some_and(|provider| provider.empty_user_message_compat());
    match apply_empty_user_message_compat(payload, enabled) {
        Ok(true) => {
            tracer.mark_empty_user_compat_applied();
            tracing::info!(
                empty_user_compat_applied = true,
                "已为精确匹配的空 user 请求补入最小上游兼容文本"
            );
            None
        }
        Ok(false) => None,
        Err(message) => {
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(
                tracer,
                StatusCode::BAD_REQUEST,
                "empty_user_message",
                message,
            );
            Some(
                (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse::new("invalid_request_error", message)),
                )
                    .into_response(),
            )
        }
    }
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
    let tracer = std::sync::Arc::new(RequestTracer::new(
        &state,
        RequestTraceOptions {
            key_ctx: key_ctx.clone(),
            model: payload.model.clone(),
            is_stream: payload.stream,
            reasoning_effort: payload
                .output_config
                .as_ref()
                .map(|value| value.effort.clone()),
            context_1m: beta_has_context_1m(&headers),
            thinking: reasoning_requested(&payload),
        },
        &headers,
        &payload,
    ));
    if let Some(response) = handle_empty_user_message(&state, &mut payload, &hook, &tracer) {
        return response;
    }
    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(
                &tracer,
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Kiro API provider not configured",
            );
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
    let identity_normalization =
        effective_identity_normalization(provider.identity_normalization(), key_ctx.response_mode);

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_model_profile_response(&state, provider.as_ref(), &payload, &hook)
    }) {
        finalize_immediate_response(&tracer, &response, "model_profile_error");
        return response;
    }

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_exact_system_response(
            &state,
            provider.as_ref(),
            &payload,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "exact_system_error");
        return response;
    }

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_exact_user_response(
            &state,
            provider.as_ref(),
            &payload,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "exact_user_error");
        return response;
    }

    if let Some(response) = try_local_ping_response(&state, provider.as_ref(), &payload, &hook) {
        finalize_immediate_response(&tracer, &response, "local_ping_error");
        return response;
    }

    let context_window_size = request_context_window_size(&state, &payload);

    let strict_json_candidate = strict_json_route_allowed(&payload);

    let document_expansion = match super::document::expand_pdf_documents(&mut payload).await {
        Ok(expansion) => expansion,
        Err(error) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let message = error.to_string();
            let response = map_document_error(error);
            finalize_immediate_error(&tracer, response.status(), "document_error", &message);
            return response;
        }
    };
    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_document_identifier_response(
            &state,
            provider.as_ref(),
            &payload,
            &document_expansion,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "document_identifier_error");
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
        finalize_immediate_response(&tracer, &resp, "websearch_error");
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        let response = super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            context_window_size,
        )
        .await;
        finalize_immediate_response(&tracer, &response, "websearch_loop_error");
        return response;
    }

    // 转换请求
    let conversion_result = match prepare_request(&mut payload, state.tool_compatibility_mode).await
    {
        Ok(result) => result,
        Err(PrepareRequestError::Document(error)) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let message = error.to_string();
            let response = map_document_error(error);
            finalize_immediate_error(&tracer, response.status(), "document_error", &message);
            return response;
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
                ConversionError::InvalidToolHistory(reason) => (
                    "invalid_request_error",
                    format!("工具调用历史无效: {}", reason),
                ),
                ConversionError::InvalidToolChoice(reason) => {
                    ("invalid_request_error", format!("工具选择无效: {}", reason))
                }
                ConversionError::InvalidImage { location, source } => (
                    "invalid_request_error",
                    format!("图片 {location} 无效: {source}"),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(
                &tracer,
                StatusCode::BAD_REQUEST,
                conversion_error_trace_type(&e),
                &message,
            );
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

    let prepared_bodies = match prepare_outbound_kiro_bodies(&kiro_request, provider.as_ref()) {
        Ok(prepared) => prepared,
        Err(error) => {
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(&tracer, error.status, error.error_type, &error.safe_message);
            return error.response;
        }
    };
    let request_body = &prepared_bodies.primary_body;
    let threshold_retry_body = prepared_bodies.threshold_retry_body.as_deref();
    tracing::debug!(
        trace_id = %tracer.trace_id(),
        body_bytes = request_body.len(),
        body_sha256 = %hex::encode(Sha256::digest(request_body.as_bytes())),
        model = %payload.model,
        stream = payload.stream,
        "Kiro request prepared"
    );

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
    let tool_contracts = conversion_result.tool_contracts;
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

    if strict_json_candidate {
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        return handle_strict_json_request(
            provider,
            request_body,
            threshold_retry_body,
            &payload,
            total_input_tokens,
            context_window_size,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
        )
        .await;
    }

    if payload.stream {
        // 流式响应
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        handle_stream_request(
            provider,
            request_body,
            threshold_retry_body,
            &payload.model,
            total_input_tokens,
            context_window_size,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        handle_non_stream_request(
            provider,
            request_body,
            threshold_retry_body,
            &payload.model,
            total_input_tokens,
            context_window_size,
            extract_thinking,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
        )
        .await
    }
}

/// 处理流式请求
struct StreamAttemptSetup {
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    threshold_retry_body: Option<String>,
    model: String,
    input_tokens: i32,
    context_window_size: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_contracts: std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    cache_usage: super::cache_metering::CacheUsage,
    group: Option<String>,
    identity_normalization: bool,
    strict_thinking_validation: bool,
}

fn prepare_retry_request_body(
    request_body: &str,
    threshold_retry_body: Option<&str>,
    failure: Option<&super::tool_attempt::AttemptFailure>,
    tool_name_map: &std::collections::HashMap<String, String>,
) -> Option<String> {
    let base = threshold_retry_body.unwrap_or(request_body);
    match failure {
        Some(super::tool_attempt::AttemptFailure::InvalidToolSchema { failure }) => {
            super::tool_schema::append_tool_schema_retry_instruction(base, failure, tool_name_map)
        }
        _ => Some(base.to_owned()),
    }
}

impl StreamAttemptSetup {
    fn new_context(&self) -> StreamContext {
        let mut ctx = StreamContext::new_with_constraints(
            &self.model,
            self.input_tokens,
            self.thinking_enabled,
            self.strict_thinking_validation,
            self.tool_name_map.clone(),
            self.known_tool_names.clone(),
            self.tool_choice_policy.clone(),
        );
        ctx.set_context_window_size(self.context_window_size);
        ctx.set_tool_contracts(self.tool_contracts.clone());
        ctx.cache_usage = self.cache_usage;
        if self.identity_normalization {
            ctx.enable_identity_filter();
        }
        ctx
    }

    fn new_buffered_context(&self) -> BufferedStreamContext {
        let mut ctx = BufferedStreamContext::new_with_constraints(
            &self.model,
            self.input_tokens,
            self.thinking_enabled,
            self.strict_thinking_validation,
            self.tool_name_map.clone(),
            self.known_tool_names.clone(),
            self.tool_choice_policy.clone(),
        );
        ctx.set_context_window_size(self.context_window_size);
        ctx.set_tool_contracts(self.tool_contracts.clone());
        ctx.set_cache_usage(self.cache_usage);
        if self.identity_normalization {
            ctx.enable_identity_filter();
        }
        ctx
    }

    async fn call_retry(
        &self,
        request_body: &str,
        tracer: &RequestTracer,
    ) -> anyhow::Result<crate::kiro::provider::KiroCallResult> {
        self.provider
            .call_api_stream(request_body, Some(tracer), self.group.as_deref())
            .await
    }
}

async fn handle_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    threshold_retry_body: Option<&str>,
    model: &str,
    input_tokens: i32,
    context_window_size: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_contracts: std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    identity_normalization: bool,
) -> Response {
    if provider.early_stream_handshake() {
        let idle_timeout_secs = provider.stream_idle_timeout_secs();
        let stream = create_early_sse_stream(
            provider,
            request_body.to_owned(),
            threshold_retry_body.map(str::to_owned),
            model.to_owned(),
            input_tokens,
            context_window_size,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            group,
            idle_timeout_secs,
            identity_normalization,
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
        .call_api_stream_with_content_length_retry(
            request_body,
            threshold_retry_body,
            Some(tracer.as_ref()),
            group.as_deref(),
        )
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
    let attempt_setup = StreamAttemptSetup {
        provider: provider.clone(),
        request_body: request_body.to_owned(),
        threshold_retry_body: threshold_retry_body.map(str::to_owned),
        model: model.to_owned(),
        input_tokens,
        context_window_size,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        tool_contracts,
        tool_choice_policy,
        cache_usage,
        group,
        identity_normalization,
        strict_thinking_validation: provider.strict_thinking_validation(),
    };

    // 创建 SSE 流（带 idle watchdog：上游首字节前挂死 / 中途停流超阈值主动收尾）
    let idle_timeout_secs = provider.stream_idle_timeout_secs();
    let stream = create_sse_stream(call_result, attempt_setup, hook, tracer, idle_timeout_secs);

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
    attempt: StreamAttemptSetup,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
}

#[allow(clippy::too_many_arguments)]
fn create_early_sse_stream(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: String,
    threshold_retry_body: Option<String>,
    model: String,
    input_tokens: i32,
    context_window_size: i32,
    thinking_enabled: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_contracts: std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    idle_timeout_secs: u64,
    identity_normalization: bool,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let tracer_for_call = tracer.clone();
    let provider_for_call = provider.clone();
    let request_body_for_call = request_body.clone();
    let threshold_retry_body_for_call = threshold_retry_body.clone();
    let group_for_call = group.clone();
    let call = async move {
        provider_for_call
            .call_api_stream_with_content_length_retry(
                &request_body_for_call,
                threshold_retry_body_for_call.as_deref(),
                Some(tracer_for_call.as_ref()),
                group_for_call.as_deref(),
            )
            .await
    };
    let mut setup = Some(EarlyStreamSetup {
        attempt: StreamAttemptSetup {
            provider: provider.clone(),
            request_body,
            threshold_retry_body,
            model,
            input_tokens,
            context_window_size,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            cache_usage,
            group,
            identity_normalization,
            strict_thinking_validation: provider.strict_thinking_validation(),
        },
        hook,
        tracer,
        idle_timeout_secs,
    });

    flatten_pending_call(call, move |result| {
        let setup = setup.take().expect("early stream setup consumed once");
        match result {
            Ok(call_result) => Box::pin(create_sse_stream(
                call_result,
                setup.attempt,
                setup.hook,
                setup.tracer,
                setup.idle_timeout_secs,
            )),
            Err(err) => {
                setup
                    .hook
                    .record(0, setup.attempt.input_tokens, 0, 0, 0, 0.0, "error");
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
        return matches!(
            event
                .data
                .pointer("/content_block/type")
                .and_then(serde_json::Value::as_str),
            Some("tool_use" | "redacted_thinking")
        );
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
    first_call: crate::kiro::provider::KiroCallResult,
    setup: StreamAttemptSetup,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(run_realtime_sse_attempts(
        first_call,
        setup,
        hook,
        tracer,
        idle_timeout_secs,
        sender,
    ));
    stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
}

async fn send_sse_events(
    sender: &tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
    tracer: &RequestTracer,
    events: Vec<SseEvent>,
) -> bool {
    mark_first_token_if_visible(tracer, &events);
    for event in events {
        if sender
            .send(Ok(Bytes::from(event.to_sse_string())))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

async fn run_realtime_sse_attempts(
    first_call: crate::kiro::provider::KiroCallResult,
    setup: StreamAttemptSetup,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
    sender: tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let mut first_call = Some(first_call);
    let mut retry_request_body = None;
    for attempt_index in 0_u8..=1 {
        let call_result = if let Some(call_result) = first_call.take() {
            call_result
        } else {
            let Some(request_body) = retry_request_body.as_deref() else {
                tracing::error!("缺少受控重试请求体，停止第二次上游调用");
                return;
            };
            let retry_result = tokio::select! {
                biased;
                _ = sender.closed() => {
                    finalize_client_disconnected(tracer.as_ref(), 0, TraceUsage::zero());
                    return;
                },
                result = setup.call_retry(request_body, tracer.as_ref()) => result,
            };
            match retry_result {
                Ok(call_result) => call_result,
                Err(error) => {
                    hook.record(0, setup.input_tokens, 0, 0, 0, 0.0, "error");
                    let upstream_status = tracer.last_http_status();
                    let error_type = last_attempt_outcome(&tracer);
                    let message = error.to_string();
                    tracer.finalize(
                        "error",
                        error_type,
                        Some(&message),
                        None,
                        TraceUsage::zero(),
                    );
                    let _ = sender
                        .send(Ok(provider_error_sse(error, upstream_status)))
                        .await;
                    return;
                }
            }
        };
        let credential_id = call_result.credential_id;
        let mut body_stream = Box::pin(call_result.response.bytes_stream());
        let mut ctx = setup.new_context();
        let mut probation = ProbationBuffer::default();
        let initial_events = probation.push_all(ctx.generate_initial_events());
        if !send_sse_events(&sender, tracer.as_ref(), initial_events).await {
            finalize_realtime_client_disconnected(&hook, tracer.as_ref(), &ctx, credential_id, 0);
            return;
        }
        let mut decoder = EventStreamDecoder::new();
        let mut ping_interval = interval(Duration::from_secs(PING_INTERVAL_SECS));
        let mut received_bytes = 0_u64;
        let mut idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));

        let termination = loop {
            let idle_fut = async {
                if idle_timeout_secs == 0 {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep_until(idle_deadline).await;
                }
            };
            tokio::select! {
                biased;
                _ = sender.closed() => break AttemptTermination::ClientClosed,
                chunk_result = body_stream.next() => match chunk_result {
                    Some(Ok(chunk)) => {
                        tracer.mark_upstream_first_byte();
                        tracer.record_stream_chunk(&chunk);
                        received_bytes += chunk.len() as u64;
                        idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));
                        if let Err(error) = decoder.feed(&chunk) {
                            tracing::warn!(%error, attempt = attempt_index + 1, "流式解码缓冲区溢出");
                            tracer.record_protocol_error("sse_state_error", &error.to_string());
                        }
                        let mut events = Vec::new();
                        for result in decoder.decode_iter() {
                            match result {
                                Ok(frame) => match Event::from_frame(frame) {
                                    Ok(event) => events.extend(ctx.process_kiro_event(&event)),
                                    Err(error) => {
                                        tracing::warn!(%error, attempt = attempt_index + 1, "流式事件解码失败");
                                        tracer.record_protocol_error("sse_state_error", &error.to_string());
                                    }
                                },
                                Err(error) => {
                                    tracing::warn!(%error, attempt = attempt_index + 1, "流式 frame 解码失败");
                                    tracer.record_protocol_error("sse_state_error", &error.to_string());
                                }
                            }
                        }
                        let visible = probation.push_all(events);
                        if !send_sse_events(&sender, tracer.as_ref(), visible).await {
                            finalize_realtime_client_disconnected(
                                &hook,
                                tracer.as_ref(),
                                &ctx,
                                credential_id,
                                received_bytes,
                            );
                            return;
                        }
                    }
                    Some(Err(error)) => {
                        tracing::error!(%error, attempt = attempt_index + 1, "读取响应流失败");
                        tracer.record_protocol_error("stream_read_error", &error.to_string());
                        break AttemptTermination::ReadError(error.to_string());
                    }
                    None => break AttemptTermination::Eof,
                },
                _ = ping_interval.tick() => {
                    if sender.send(Ok(create_ping_sse())).await.is_err() {
                        finalize_realtime_client_disconnected(
                            &hook,
                            tracer.as_ref(),
                            &ctx,
                            credential_id,
                            received_bytes,
                        );
                        return;
                    }
                }
                _ = idle_fut => {
                    tracing::warn!(attempt = attempt_index + 1, received_bytes, idle_timeout_secs, "流式空闲超时，主动收尾");
                    tracer.record_protocol_error(
                        "stream_idle_timeout",
                        &format!("stream idle timeout after {idle_timeout_secs}s"),
                    );
                    break AttemptTermination::IdleTimeout;
                }
            }
        };

        if matches!(termination, AttemptTermination::ClientClosed) {
            finalize_realtime_client_disconnected(
                &hook,
                tracer.as_ref(),
                &ctx,
                credential_id,
                received_bytes,
            );
            return;
        }

        let final_events = ctx.generate_final_events_for(&termination);
        let visible = probation.push_all(final_events);
        let attempt_failure = ctx.terminal_attempt_failure().cloned();
        if let Some(super::tool_attempt::AttemptFailure::InvalidToolSchema { failure }) =
            &attempt_failure
        {
            tracer.record_tool_schema_failure(failure, attempt_index + 1);
        }
        let can_retry = probation.should_retry_attempt(
            attempt_index,
            termination.clone(),
            attempt_failure.clone(),
        );
        let prepared_retry_body = can_retry
            .then(|| {
                prepare_retry_request_body(
                    &setup.request_body,
                    setup.threshold_retry_body.as_deref(),
                    attempt_failure.as_ref(),
                    &setup.tool_name_map,
                )
            })
            .flatten();
        let retryable = prepared_retry_body.is_some()
            && probation.prepare_attempt_retry(attempt_index, termination.clone(), attempt_failure);
        if retryable {
            retry_request_body = prepared_retry_body;
            tracing::warn!(
                attempt = attempt_index + 1,
                termination = ?termination,
                "实时首轮未提交语义输出，丢弃整轮并重试一次"
            );
            continue;
        }

        let mut visible = visible;
        visible.extend(probation.take_pending());
        if !send_sse_events(&sender, tracer.as_ref(), visible).await {
            finalize_realtime_client_disconnected(
                &hook,
                tracer.as_ref(),
                &ctx,
                credential_id,
                received_bytes,
            );
            return;
        }
        match termination {
            AttemptTermination::Eof => {
                if let Some(message) = ctx.terminal_error_message() {
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
                    tracer.finalize("success", None, None, None, stream_trace_usage(&ctx));
                }
            }
            AttemptTermination::ReadError(message) => {
                record_stream_usage(&hook, &ctx, credential_id, "error");
                tracer.finalize(
                    "interrupted",
                    Some("stream_read_error"),
                    Some(&message),
                    Some(received_bytes),
                    stream_trace_usage(&ctx),
                );
            }
            AttemptTermination::IdleTimeout => {
                record_stream_usage(&hook, &ctx, credential_id, "error");
                tracer.finalize(
                    "interrupted",
                    Some("stream_idle_timeout"),
                    Some(&format!("stream idle timeout after {}s", idle_timeout_secs)),
                    Some(received_bytes),
                    stream_trace_usage(&ctx),
                );
            }
            AttemptTermination::ClientClosed => return,
        }
        return;
    }
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

fn finalize_realtime_client_disconnected(
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
    ctx: &StreamContext,
    credential_id: u64,
    received_bytes: u64,
) {
    record_stream_usage(hook, ctx, credential_id, "error");
    finalize_client_disconnected(tracer, received_bytes, stream_trace_usage(ctx));
}

struct NonStreamToolAttempt {
    credential_id: u64,
    content: Vec<serde_json::Value>,
    upstream_signalled_tool_use: bool,
    stop_reason: String,
    upstream_context_tokens: Option<i32>,
    credits: f64,
    state: super::tool_attempt::ToolAttemptState,
}

enum NonStreamCollectError {
    Provider(anyhow::Error),
    Body {
        credential_id: u64,
        message: String,
        received_bytes: u64,
    },
    IdleTimeout {
        credential_id: u64,
        idle_timeout_secs: u64,
        received_bytes: u64,
    },
}

fn should_retry_non_stream_collect_error(attempt_index: u8, error: &NonStreamCollectError) -> bool {
    attempt_index == 0
        && matches!(
            error,
            NonStreamCollectError::Body { .. } | NonStreamCollectError::IdleTimeout { .. }
        )
}

fn non_stream_collect_error_type(error: &NonStreamCollectError) -> Option<&'static str> {
    match error {
        NonStreamCollectError::Body { .. } => Some("stream_read_error"),
        NonStreamCollectError::IdleTimeout { .. } => Some("stream_idle_timeout"),
        NonStreamCollectError::Provider(_) => None,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum NonStreamBodyReadFailure {
    Read {
        message: String,
        received_bytes: u64,
    },
    IdleTimeout {
        received_bytes: u64,
    },
}

async fn collect_body_stream_with_idle_timeout<S, E>(
    stream: S,
    idle_timeout: Option<Duration>,
) -> Result<Bytes, NonStreamBodyReadFailure>
where
    S: Stream<Item = Result<Bytes, E>>,
    E: std::fmt::Display,
{
    futures::pin_mut!(stream);
    let mut body = bytes::BytesMut::new();
    loop {
        let next = if let Some(timeout) = idle_timeout {
            tokio::time::timeout(timeout, stream.next())
                .await
                .map_err(|_| NonStreamBodyReadFailure::IdleTimeout {
                    received_bytes: body.len() as u64,
                })?
        } else {
            stream.next().await
        };
        match next {
            Some(Ok(chunk)) => body.extend_from_slice(&chunk),
            Some(Err(error)) => {
                return Err(NonStreamBodyReadFailure::Read {
                    message: error.to_string(),
                    received_bytes: body.len() as u64,
                });
            }
            None => return Ok(body.freeze()),
        }
    }
}

fn non_stream_attempt_error(
    failure: &super::tool_attempt::AttemptFailure,
    attempt_count: u8,
) -> (StatusCode, &'static str, String) {
    let (error_type, mut message) = failure.public_error();
    if attempt_count == 2
        && matches!(
            failure,
            super::tool_attempt::AttemptFailure::InvalidToolSchema { .. }
        )
    {
        message = format!("Upstream tool input still violated schema after one retry: {message}");
    }
    (StatusCode::BAD_GATEWAY, error_type, message)
}

fn normalize_and_validate_non_stream_content(
    base_content: Vec<serde_json::Value>,
    native_tool_uses: Vec<serde_json::Value>,
    known_tool_names: &std::collections::HashSet<String>,
    tool_name_map: &std::collections::HashMap<String, String>,
    tool_contracts: &std::collections::HashMap<String, super::tool_schema::ToolContract>,
) -> (
    Vec<serde_json::Value>,
    Result<Vec<String>, super::tool_schema::ToolSchemaError>,
) {
    let native_tool_ids: std::collections::HashSet<String> = native_tool_uses
        .iter()
        .filter_map(|block| block.get("id").and_then(serde_json::Value::as_str))
        .map(str::to_owned)
        .collect();
    let mut content = super::stream::normalize_non_stream_content_blocks(
        base_content,
        native_tool_uses,
        known_tool_names,
        tool_name_map,
    );
    let validation = super::tool_schema::validate_tool_use_blocks(tool_contracts, &mut content);
    if validation.is_ok() {
        super::stream::dedupe_reclaimed_tools_after_repair(&mut content, &native_tool_ids);
    }
    (content, validation)
}

fn non_stream_content_has_non_tool_semantic_output(content: &[serde_json::Value]) -> bool {
    content.iter().any(
        |block| match block.get("type").and_then(serde_json::Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            Some("thinking") => block
                .get("thinking")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.is_empty()),
            Some("redacted_thinking") => block
                .get("data")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|data| !data.is_empty()),
            _ => false,
        },
    )
}

#[allow(clippy::too_many_arguments)]
async fn collect_non_stream_tool_attempt(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    threshold_retry_body: Option<&str>,
    input_tokens: i32,
    context_window_size: i32,
    thinking_enabled: bool,
    tool_name_map: &std::collections::HashMap<String, String>,
    known_tool_names: &std::collections::HashSet<String>,
    tool_contracts: &std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<&str>,
    attempt_index: u8,
    identity_normalization: bool,
) -> Result<NonStreamToolAttempt, NonStreamCollectError> {
    let call_result = provider
        .call_api_with_content_length_retry(
            request_body,
            threshold_retry_body,
            Some(tracer.as_ref()),
            group,
        )
        .await
        .map_err(NonStreamCollectError::Provider)?;
    let credential_id = call_result.credential_id;
    let idle_timeout_secs = provider.stream_idle_timeout_secs();
    let idle_timeout = (idle_timeout_secs > 0).then(|| Duration::from_secs(idle_timeout_secs));
    let body_bytes =
        collect_body_stream_with_idle_timeout(call_result.response.bytes_stream(), idle_timeout)
            .await
            .map_err(|failure| match failure {
                NonStreamBodyReadFailure::Read {
                    message,
                    received_bytes,
                } => NonStreamCollectError::Body {
                    credential_id,
                    message,
                    received_bytes,
                },
                NonStreamBodyReadFailure::IdleTimeout { received_bytes } => {
                    NonStreamCollectError::IdleTimeout {
                        credential_id,
                        idle_timeout_secs,
                        received_bytes,
                    }
                }
            })?;

    let mut decoder = EventStreamDecoder::new();
    if let Err(error) = decoder.feed(&body_bytes) {
        tracing::warn!(%error, attempt = attempt_index + 1, "非流式响应解码缓冲区溢出");
        tracer.record_protocol_error("sse_state_error", &error.to_string());
    }

    let mut text_content = String::new();
    let mut native_thinking = String::new();
    let mut native_thinking_signature = None;
    let mut native_redacted_thinking = Vec::new();
    let mut tool_uses = Vec::new();
    let mut upstream_signalled_tool_use = false;
    let mut stop_reason = "end_turn".to_string();
    let mut upstream_context_tokens = None;
    let mut credits = 0.0_f64;
    let mut tool_accumulator = super::stream::ToolJsonAccumulator::new();
    let mut tool_json_error = None;
    let mut observation = super::tool_attempt::AttemptObservation::default();

    for result in decoder.decode_iter() {
        match result {
            Ok(frame) => match Event::from_frame(frame) {
                Ok(event) => {
                    observation.observe(&event);
                    match event {
                        Event::AssistantResponse(response) => {
                            text_content.push_str(&response.content);
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
                                attempt = attempt_index + 1,
                                "received upstream non-stream tool_use fragment"
                            );
                            match tool_accumulator.push(&tool_use, tool_name_map) {
                                Ok(Some(completed)) => {
                                    tool_uses.push(completed.to_anthropic_block());
                                }
                                Ok(None) => {}
                                Err(error) => {
                                    tracing::error!(%error, attempt = attempt_index + 1);
                                    tracer.record_protocol_error(
                                        "upstream_tool_protocol_error",
                                        &error.to_string(),
                                    );
                                    tool_json_error = Some(error);
                                }
                            }
                        }
                        Event::ContextUsage(context_usage) => {
                            let actual_input_tokens = (context_usage.context_usage_percentage
                                * f64::from(context_window_size)
                                / 100.0)
                                as i32;
                            upstream_context_tokens = Some(actual_input_tokens);
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
                        Event::Metering(metering) => credits += metering.usage,
                        Event::Exception { exception_type, .. }
                            if exception_type == "ContentLengthExceededException" =>
                        {
                            stop_reason = "max_tokens".to_string();
                        }
                        _ => {}
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, attempt = attempt_index + 1, "事件帧解码失败");
                    tracer.record_protocol_error("sse_state_error", &error.to_string());
                }
            },
            Err(error) => {
                tracing::warn!(%error, attempt = attempt_index + 1, "解码事件失败");
                tracer.record_protocol_error("sse_state_error", &error.to_string());
            }
        }
    }

    if tool_json_error.is_none() {
        let (completed, error) = tool_accumulator.finish(tool_name_map);
        if error.is_none() {
            for tool_use in completed {
                tracing::warn!(
                    tool_id = %tool_use.id,
                    tool_name = %tool_use.name,
                    attempt = attempt_index + 1,
                    "上游未发 stop=true；残留入参已严格解析为完整 JSON，按隐式 stop 打捞"
                );
                tool_uses.push(tool_use.to_anthropic_block());
            }
        }
        tool_json_error = error;
        if let Some(error) = &tool_json_error {
            tracer.record_protocol_error("upstream_tool_protocol_error", &error.to_string());
        }
    }

    let text_content = crate::kiro::model::events::strip_tool_use_xml_leaks(&text_content);
    let text_content = if identity_normalization {
        super::identity::normalize_identity_text(&text_content)
    } else {
        text_content
    };
    let base_content = build_non_stream_content(
        thinking_enabled,
        text_content,
        native_thinking,
        native_thinking_signature,
        native_redacted_thinking,
    );
    let (content, validation) = normalize_and_validate_non_stream_content(
        base_content,
        tool_uses,
        known_tool_names,
        tool_name_map,
        tool_contracts,
    );
    let schema_failure = match validation {
        Ok(repaired) => {
            if !repaired.is_empty() {
                tracing::warn!(paths = ?repaired, attempt = attempt_index + 1, "确定性修复上游工具固定字段");
            }
            None
        }
        Err(error) => {
            tracing::warn!(tool = %error.tool_name, attempt = attempt_index + 1, "上游工具参数不满足客户端Schema");
            Some(super::tool_attempt::AttemptFailure::InvalidToolSchema {
                failure: super::tool_schema::ToolSchemaFailure::from_error_and_blocks(
                    error, &content,
                ),
            })
        }
    };
    let has_completed_tool = schema_failure.is_none()
        && content
            .iter()
            .any(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("tool_use"));
    let has_non_tool_semantic_output = non_stream_content_has_non_tool_semantic_output(&content);
    let failure = schema_failure
        .clone()
        .or_else(|| observation.failure(tool_json_error, has_completed_tool));
    if let Some(failure) = &failure {
        match failure {
            super::tool_attempt::AttemptFailure::InvalidToolSchema {
                failure: schema_failure,
            } => tracer.record_tool_schema_failure(schema_failure, attempt_index + 1),
            _ => tracer.record_upstream_body(attempt_index as u32, &body_bytes),
        }
    }
    let semantic_output_started =
        has_non_tool_semantic_output || (has_completed_tool && schema_failure.is_none());
    tracing::debug!(
        attempt = attempt_index + 1,
        saw_frame = observation.saw_frame(),
        semantic_output_started,
        has_completed_tool,
        failure_type = failure.as_ref().map(|failure| failure.public_error().0),
        "classified non-stream upstream attempt"
    );

    Ok(NonStreamToolAttempt {
        credential_id,
        content,
        upstream_signalled_tool_use,
        stop_reason,
        upstream_context_tokens,
        credits,
        state: super::tool_attempt::ToolAttemptState {
            attempt_index,
            termination: super::tool_attempt::AttemptTermination::Eof,
            failure,
            semantic_output_started,
            tool_forwarded: false,
        },
    })
}

/// 处理非流式请求
async fn handle_non_stream_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    request_body: &str,
    threshold_retry_body: Option<&str>,
    model: &str,
    input_tokens: i32,
    context_window_size: i32,
    thinking_enabled: bool,
    require_thinking: bool,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_contracts: std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    identity_normalization: bool,
) -> Response {
    let collection = async {
        let mut retry_request_body = None;
        let mut retry_failure_type = None;
        for attempt_index in 0_u8..=1 {
            let (attempt_body, attempt_threshold_retry_body) = if attempt_index == 0 {
                (request_body, threshold_retry_body)
            } else {
                (
                    retry_request_body
                        .as_deref()
                        .expect("second attempt requires a prepared retry body"),
                    None,
                )
            };
            let attempt = match collect_non_stream_tool_attempt(
                provider.clone(),
                attempt_body,
                attempt_threshold_retry_body,
                input_tokens,
                context_window_size,
                thinking_enabled,
                &tool_name_map,
                &known_tool_names,
                &tool_contracts,
                tracer.clone(),
                group.as_deref(),
                attempt_index,
                identity_normalization,
            )
            .await
            {
                Ok(attempt) => attempt,
                Err(error) if should_retry_non_stream_collect_error(attempt_index, &error) => {
                    let error_type = non_stream_collect_error_type(&error)
                        .expect("retryable body errors have a stable type");
                    retry_failure_type = Some(error_type);
                    retry_request_body = Some(request_body.to_owned());
                    tracer.record_protocol_error(
                        error_type,
                        "the first non-stream response body ended before delivery; retrying once",
                    );
                    continue;
                }
                Err(error) => return Err(error),
            };
            if attempt.state.should_retry()
                && let Some(body) = prepare_retry_request_body(
                    request_body,
                    threshold_retry_body,
                    attempt.state.failure.as_ref(),
                    &tool_name_map,
                )
            {
                retry_failure_type = attempt
                    .state
                    .failure
                    .as_ref()
                    .map(|failure| failure.public_error().0);
                retry_request_body = Some(body);
                continue;
            }
            return Ok((attempt, attempt_index + 1, retry_failure_type));
        }
        unreachable!("第二次工具生成 attempt 不允许继续重试")
    }
    .await;

    let (attempt, attempt_count, retry_failure_type) = match collection {
        Ok(result) => result,
        Err(NonStreamCollectError::Provider(error)) => {
            hook.record(0, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "error",
                last_attempt_outcome(&tracer),
                Some(&error.to_string()),
                None,
                TraceUsage::zero(),
            );
            return map_provider_error(error);
        }
        Err(NonStreamCollectError::Body {
            credential_id,
            message,
            received_bytes,
        }) => {
            tracing::error!(%message, received_bytes, "读取非流式响应体失败");
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.finalize(
                "interrupted",
                Some(outcome::STREAM_INTERRUPTED),
                Some(&message),
                Some(received_bytes),
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "api_error",
                    format!("读取响应失败: {message}"),
                )),
            )
                .into_response();
        }
        Err(NonStreamCollectError::IdleTimeout {
            credential_id,
            idle_timeout_secs,
            received_bytes,
        }) => {
            let message = format!("stream idle timeout after {idle_timeout_secs}s");
            tracing::error!(idle_timeout_secs, received_bytes, "非流式响应体空闲超时");
            hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
            tracer.record_protocol_error("stream_idle_timeout", &message);
            tracer.finalize(
                "interrupted",
                Some("stream_idle_timeout"),
                Some(&message),
                Some(received_bytes),
                TraceUsage::zero(),
            );
            return (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new("api_error", message)),
            )
                .into_response();
        }
    };

    if attempt_count == 2 {
        let retry_failure_type = retry_failure_type.unwrap_or("upstream_empty_response");
        tracing::warn!(
            first_attempt_error = retry_failure_type,
            "非流式首轮未交付响应，已完成一次受控重试"
        );
        tracer.record_protocol_error(
            retry_failure_type,
            "the first non-stream response was rejected before delivery and retried once",
        );
    }

    let NonStreamToolAttempt {
        credential_id,
        content,
        upstream_signalled_tool_use,
        mut stop_reason,
        upstream_context_tokens,
        credits,
        state,
    } = attempt;
    // 上游 attempt 失败：非流式路径尚未发送任何字节，直接回 502。
    // 显式 Error/Exception 的原始正文只保留在内部分类中，不回显给客户端。
    if let Some(failure) = state.failure {
        let (status, error_type, message) = non_stream_attempt_error(&failure, attempt_count);
        hook.record(credential_id, input_tokens, 0, 0, 0, 0.0, "error");
        tracer.record_protocol_error(error_type, &message);
        tracer.finalize(
            "error",
            Some(outcome::BAD_REQUEST),
            Some(&message),
            None,
            TraceUsage::zero(),
        );
        return (status, Json(ErrorResponse::new(error_type, message))).into_response();
    }

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
                "signature": native_thinking_signature.unwrap_or_else(|| {
                    super::thinking_signature::issue_signature("non-stream", &native_thinking)
                }),
            }));
        } else {
            // 从完整文本中提取 thinking 块，兼容旧的 <thinking> 文本路径。
            let (thinking, remaining_text) =
                super::stream::extract_thinking_from_complete_text(&text_content);

            if let Some(thinking_text) = thinking {
                content.push(json!({
                    "type": "thinking",
                    "thinking": thinking_text,
                    "signature": super::thinking_signature::issue_signature(
                        "non-stream",
                        &thinking_text,
                    ),
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
            format: None,
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
    let tracer = std::sync::Arc::new(RequestTracer::new(
        &state,
        RequestTraceOptions {
            key_ctx: key_ctx.clone(),
            model: payload.model.clone(),
            is_stream: payload.stream,
            reasoning_effort: payload
                .output_config
                .as_ref()
                .map(|value| value.effort.clone()),
            context_1m: beta_has_context_1m(&headers),
            thinking: reasoning_requested(&payload),
        },
        &headers,
        &payload,
    ));

    if let Some(response) = handle_empty_user_message(&state, &mut payload, &hook, &tracer) {
        return response;
    }

    // 检查 KiroProvider 是否可用
    let provider = match &state.kiro_provider {
        Some(p) => p.clone(),
        None => {
            tracing::error!("KiroProvider 未配置");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(
                &tracer,
                StatusCode::SERVICE_UNAVAILABLE,
                "service_unavailable",
                "Kiro API provider not configured",
            );
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
    let identity_normalization =
        effective_identity_normalization(provider.identity_normalization(), key_ctx.response_mode);

    // 检测模型名是否包含 "thinking" 后缀，若包含则覆写 thinking 配置
    override_thinking_from_model_name(&mut payload);

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_model_profile_response(&state, provider.as_ref(), &payload, &hook)
    }) {
        finalize_immediate_response(&tracer, &response, "model_profile_error");
        return response;
    }

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_exact_system_response(
            &state,
            provider.as_ref(),
            &payload,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "exact_system_error");
        return response;
    }

    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_exact_user_response(
            &state,
            provider.as_ref(),
            &payload,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "exact_user_error");
        return response;
    }

    if let Some(response) = try_local_ping_response(&state, provider.as_ref(), &payload, &hook) {
        finalize_immediate_response(&tracer, &response, "local_ping_error");
        return response;
    }

    let context_window_size = request_context_window_size(&state, &payload);

    let strict_json_candidate = strict_json_route_allowed(&payload);

    let document_expansion = match super::document::expand_pdf_documents(&mut payload).await {
        Ok(expansion) => expansion,
        Err(error) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let message = error.to_string();
            let response = map_document_error(error);
            finalize_immediate_error(&tracer, response.status(), "document_error", &message);
            return response;
        }
    };
    if let Some(response) = detection_only(key_ctx.response_mode, || {
        try_local_document_identifier_response(
            &state,
            provider.as_ref(),
            &payload,
            &document_expansion,
            &hook,
            state.tool_compatibility_mode,
        )
    }) {
        finalize_immediate_response(&tracer, &response, "document_identifier_error");
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
        finalize_immediate_response(&tracer, &resp, "websearch_error");
        return resp;
    }

    let payload_stream = payload.stream;
    // Mixed-tools (web_search + exec...) case: web_search coexists with other tools and falls onto the normal chat path,
    // where the upstream may return a tool_use with name=web_search. Take the internal agentic loop: search internally and feed the results back.
    if websearch::has_web_search_among_tools(&payload) {
        tracing::info!(
            "detected mixed tools containing web_search, entering the web_search agentic loop"
        );
        let response = super::websearch_loop::run_web_search_loop(
            provider,
            payload,
            hook,
            payload_stream,
            key_ctx.group.clone(),
            state.tool_compatibility_mode,
            context_window_size,
        )
        .await;
        finalize_immediate_response(&tracer, &response, "websearch_loop_error");
        return response;
    }

    // 转换请求
    let conversion_result = match prepare_request(&mut payload, state.tool_compatibility_mode).await
    {
        Ok(result) => result,
        Err(PrepareRequestError::Document(error)) => {
            tracing::warn!(error = %error, "Anthropic document preprocessing failed");
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            let message = error.to_string();
            let response = map_document_error(error);
            finalize_immediate_error(&tracer, response.status(), "document_error", &message);
            return response;
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
                ConversionError::InvalidToolHistory(reason) => (
                    "invalid_request_error",
                    format!("工具调用历史无效: {}", reason),
                ),
                ConversionError::InvalidToolChoice(reason) => {
                    ("invalid_request_error", format!("工具选择无效: {}", reason))
                }
                ConversionError::InvalidImage { location, source } => (
                    "invalid_request_error",
                    format!("图片 {location} 无效: {source}"),
                ),
            };
            tracing::warn!("请求转换失败: {}", e);
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(
                &tracer,
                StatusCode::BAD_REQUEST,
                conversion_error_trace_type(&e),
                &message,
            );
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

    let prepared_bodies = match prepare_outbound_kiro_bodies(&kiro_request, provider.as_ref()) {
        Ok(prepared) => prepared,
        Err(error) => {
            hook.record(0, 0, 0, 0, 0, 0.0, "error");
            finalize_immediate_error(&tracer, error.status, error.error_type, &error.safe_message);
            return error.response;
        }
    };
    let request_body = &prepared_bodies.primary_body;
    let threshold_retry_body = prepared_bodies.threshold_retry_body.as_deref();
    tracing::debug!(
        trace_id = %tracer.trace_id(),
        body_bytes = request_body.len(),
        body_sha256 = %hex::encode(Sha256::digest(request_body.as_bytes())),
        model = %payload.model,
        stream = payload.stream,
        "Kiro request prepared"
    );

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
    let tool_contracts = conversion_result.tool_contracts;
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

    if strict_json_candidate {
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        return handle_strict_json_request(
            provider,
            request_body,
            threshold_retry_body,
            &payload,
            total_input_tokens,
            context_window_size,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
        )
        .await;
    }

    if payload.stream {
        // 流式响应（缓冲模式）
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        handle_stream_request_buffered(
            provider,
            request_body,
            threshold_retry_body,
            &payload.model,
            thinking_enabled,
            context_window_size,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            hook,
            total_input_tokens,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
        )
        .await
    } else {
        // 非流式响应：仅在配置开启时提取 thinking 块
        let extract_thinking = state.extract_thinking && thinking_enabled;
        tracer.set_reasoning_effort(effort_from_fields(
            &kiro_request.additional_model_request_fields,
        ));
        handle_non_stream_request(
            provider,
            request_body,
            threshold_retry_body,
            &payload.model,
            total_input_tokens,
            context_window_size,
            extract_thinking,
            thinking_enabled,
            tool_name_map,
            known_tool_names,
            tool_contracts,
            tool_choice_policy,
            hook,
            cache_usage,
            tracer,
            key_ctx.group.clone(),
            identity_normalization,
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
    threshold_retry_body: Option<&str>,
    model: &str,
    thinking_enabled: bool,
    context_window_size: i32,
    tool_name_map: std::collections::HashMap<String, String>,
    known_tool_names: std::collections::HashSet<String>,
    tool_contracts: std::collections::HashMap<String, super::tool_schema::ToolContract>,
    tool_choice_policy: super::converter::ToolChoicePolicy,
    hook: UsageRecordHook,
    fallback_input_tokens: i32,
    cache_usage: super::cache_metering::CacheUsage,
    tracer: std::sync::Arc<RequestTracer>,
    group: Option<String>,
    identity_normalization: bool,
) -> Response {
    // 调用 Kiro API（支持多凭据故障转移）
    let call_result = match provider
        .call_api_stream_with_content_length_retry(
            request_body,
            threshold_retry_body,
            Some(tracer.as_ref()),
            group.as_deref(),
        )
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
    let attempt_setup = StreamAttemptSetup {
        provider: provider.clone(),
        request_body: request_body.to_owned(),
        threshold_retry_body: threshold_retry_body.map(str::to_owned),
        model: model.to_owned(),
        input_tokens: fallback_input_tokens,
        context_window_size,
        thinking_enabled,
        tool_name_map,
        known_tool_names,
        tool_contracts,
        tool_choice_policy,
        cache_usage,
        group,
        identity_normalization,
        strict_thinking_validation: provider.strict_thinking_validation(),
    };

    // 创建缓冲 SSE 流
    let idle_timeout_secs = provider.stream_idle_timeout_secs();
    let stream =
        create_buffered_sse_stream(call_result, attempt_setup, hook, tracer, idle_timeout_secs);

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
    first_call: crate::kiro::provider::KiroCallResult,
    setup: StreamAttemptSetup,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let (sender, receiver) = tokio::sync::mpsc::channel(16);
    tokio::spawn(run_buffered_sse_attempts(
        first_call,
        setup,
        hook,
        tracer,
        idle_timeout_secs,
        sender,
    ));
    stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|item| (item, receiver))
    })
}

async fn run_buffered_sse_attempts(
    first_call: crate::kiro::provider::KiroCallResult,
    setup: StreamAttemptSetup,
    hook: UsageRecordHook,
    tracer: std::sync::Arc<RequestTracer>,
    idle_timeout_secs: u64,
    sender: tokio::sync::mpsc::Sender<Result<Bytes, Infallible>>,
) {
    let mut first_call = Some(first_call);
    let mut retry_request_body = None;
    for attempt_index in 0_u8..=1 {
        let call_result = if let Some(call_result) = first_call.take() {
            call_result
        } else {
            let Some(request_body) = retry_request_body.as_deref() else {
                tracing::error!("缺少受控重试请求体，停止第二次上游调用");
                return;
            };
            let retry_result = tokio::select! {
                biased;
                _ = sender.closed() => {
                    finalize_client_disconnected(tracer.as_ref(), 0, TraceUsage::zero());
                    return;
                },
                result = setup.call_retry(request_body, tracer.as_ref()) => result,
            };
            match retry_result {
                Ok(call_result) => call_result,
                Err(error) => {
                    hook.record(0, setup.input_tokens, 0, 0, 0, 0.0, "error");
                    let upstream_status = tracer.last_http_status();
                    let error_type = last_attempt_outcome(&tracer);
                    let message = error.to_string();
                    tracer.finalize(
                        "error",
                        error_type,
                        Some(&message),
                        None,
                        TraceUsage::zero(),
                    );
                    let _ = sender
                        .send(Ok(provider_error_sse(error, upstream_status)))
                        .await;
                    return;
                }
            }
        };
        let credential_id = call_result.credential_id;
        let mut body_stream = Box::pin(call_result.response.bytes_stream());
        let mut ctx = setup.new_buffered_context();
        let mut decoder = EventStreamDecoder::new();
        let mut ping_interval = interval(Duration::from_secs(PING_INTERVAL_SECS));
        let mut received_bytes = 0_u64;
        let mut idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));

        let termination = loop {
            let deadline = idle_deadline;
            let idle_fut = async move {
                if idle_timeout_secs == 0 {
                    std::future::pending::<()>().await;
                } else {
                    tokio::time::sleep_until(deadline).await;
                }
            };
            tokio::select! {
                biased;
                _ = sender.closed() => break AttemptTermination::ClientClosed,
                _ = ping_interval.tick() => {
                    if sender.send(Ok(create_ping_sse())).await.is_err() {
                        finalize_buffered_client_disconnected(
                            &hook,
                            tracer.as_ref(),
                            &ctx,
                            credential_id,
                            received_bytes,
                        );
                        return;
                    }
                }
                _ = idle_fut => {
                    tracing::warn!(attempt = attempt_index + 1, received_bytes, idle_timeout_secs, "缓冲流空闲超时，主动收尾");
                    tracer.record_protocol_error(
                        "stream_idle_timeout",
                        &format!("stream idle timeout after {idle_timeout_secs}s"),
                    );
                    break AttemptTermination::IdleTimeout;
                }
                chunk_result = body_stream.next() => match chunk_result {
                    Some(Ok(chunk)) => {
                        tracer.mark_upstream_first_byte();
                        tracer.record_stream_chunk(&chunk);
                        received_bytes += chunk.len() as u64;
                        idle_deadline = TokioInstant::now() + Duration::from_secs(idle_timeout_secs.max(1));
                        if let Err(error) = decoder.feed(&chunk) {
                            tracing::warn!(%error, attempt = attempt_index + 1, "缓冲流解码缓冲区溢出");
                            tracer.record_protocol_error("sse_state_error", &error.to_string());
                        }
                        for result in decoder.decode_iter() {
                            match result {
                                Ok(frame) => match Event::from_frame(frame) {
                                    Ok(event) => ctx.process_and_buffer(&event),
                                    Err(error) => tracing::warn!(%error, attempt = attempt_index + 1, "缓冲流事件解码失败"),
                                },
                                Err(error) => tracing::warn!(%error, attempt = attempt_index + 1, "缓冲流 frame 解码失败"),
                            }
                        }
                    }
                    Some(Err(error)) => {
                        tracing::error!(%error, attempt = attempt_index + 1, "读取缓冲响应流失败");
                        tracer.record_protocol_error("stream_read_error", &error.to_string());
                        break AttemptTermination::ReadError(error.to_string());
                    }
                    None => break AttemptTermination::Eof,
                }
            }
        };

        if matches!(termination, AttemptTermination::ClientClosed) {
            finalize_buffered_client_disconnected(
                &hook,
                tracer.as_ref(),
                &ctx,
                credential_id,
                received_bytes,
            );
            return;
        }

        let all_events = ctx.finish_and_get_all_events_for(&termination);
        let mut probation = ProbationBuffer::default();
        let visible = probation.push_all(all_events);
        let attempt_failure = ctx.terminal_attempt_failure().cloned();
        if let Some(super::tool_attempt::AttemptFailure::InvalidToolSchema { failure }) =
            &attempt_failure
        {
            tracer.record_tool_schema_failure(failure, attempt_index + 1);
        }
        let can_retry = probation.should_retry_attempt(
            attempt_index,
            termination.clone(),
            attempt_failure.clone(),
        );
        let prepared_retry_body = can_retry
            .then(|| {
                prepare_retry_request_body(
                    &setup.request_body,
                    setup.threshold_retry_body.as_deref(),
                    attempt_failure.as_ref(),
                    &setup.tool_name_map,
                )
            })
            .flatten();
        let retryable = prepared_retry_body.is_some()
            && probation.prepare_attempt_retry(attempt_index, termination.clone(), attempt_failure);
        if retryable {
            retry_request_body = prepared_retry_body;
            tracing::warn!(
                attempt = attempt_index + 1,
                termination = ?termination,
                "CC 缓冲首轮未提交语义输出，丢弃整轮并重试一次"
            );
            continue;
        }

        let mut visible = visible;
        visible.extend(probation.take_pending());
        if !send_sse_events(&sender, tracer.as_ref(), visible).await {
            finalize_buffered_client_disconnected(
                &hook,
                tracer.as_ref(),
                &ctx,
                credential_id,
                received_bytes,
            );
            return;
        }
        let (input, output, cache_creation, cache_read, credits) = ctx.final_usage();
        let trace_usage = buffered_stream_trace_usage(&ctx);
        match termination {
            AttemptTermination::Eof => {
                if let Some(message) = ctx.terminal_error_message() {
                    hook.record(
                        credential_id,
                        input,
                        output,
                        cache_creation,
                        cache_read,
                        credits,
                        "error",
                    );
                    tracer.finalize(
                        "error",
                        Some(outcome::BAD_REQUEST),
                        Some(&message),
                        None,
                        trace_usage,
                    );
                } else {
                    hook.record(
                        credential_id,
                        input,
                        output,
                        cache_creation,
                        cache_read,
                        credits,
                        "success",
                    );
                    tracer.finalize("success", None, None, None, trace_usage);
                }
            }
            AttemptTermination::ReadError(message) => {
                hook.record(
                    credential_id,
                    input,
                    output,
                    cache_creation,
                    cache_read,
                    credits,
                    "error",
                );
                tracer.finalize(
                    "interrupted",
                    Some("stream_read_error"),
                    Some(&message),
                    Some(received_bytes),
                    trace_usage,
                );
            }
            AttemptTermination::IdleTimeout => {
                hook.record(
                    credential_id,
                    input,
                    output,
                    cache_creation,
                    cache_read,
                    credits,
                    "error",
                );
                tracer.finalize(
                    "interrupted",
                    Some("stream_idle_timeout"),
                    Some(&format!("stream idle timeout after {}s", idle_timeout_secs)),
                    Some(received_bytes),
                    trace_usage,
                );
            }
            AttemptTermination::ClientClosed => return,
        }
        return;
    }
}

fn buffered_stream_trace_usage(ctx: &BufferedStreamContext) -> TraceUsage {
    let (input, output, cache_creation, cache_read, credits) = ctx.final_usage();
    TraceUsage {
        input_tokens: input.max(0) as u64,
        output_tokens: output.max(0) as u64,
        cache_creation_tokens: cache_creation.max(0) as u64,
        cache_read_tokens: cache_read.max(0) as u64,
        credits: if credits.is_finite() && credits > 0.0 {
            credits
        } else {
            0.0
        },
    }
}

fn finalize_buffered_client_disconnected(
    hook: &UsageRecordHook,
    tracer: &RequestTracer,
    ctx: &BufferedStreamContext,
    credential_id: u64,
    received_bytes: u64,
) {
    let (input, output, cache_creation, cache_read, credits) = ctx.final_usage();
    hook.record(
        credential_id,
        input,
        output,
        cache_creation,
        cache_read,
        credits,
        "error",
    );
    finalize_client_disconnected(tracer, received_bytes, buffered_stream_trace_usage(ctx));
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    use futures::{StreamExt, future};

    use crate::admin::client_keys::ClientResponseMode;

    use super::*;

    #[test]
    fn response_mode_native_does_not_execute_detection_shortcut() {
        let called = std::cell::Cell::new(false);
        let result = detection_only(ClientResponseMode::KiroNative, || {
            called.set(true);
            Some("local")
        });
        assert_eq!(result, None);
        assert!(!called.get());
    }

    #[test]
    fn response_mode_detection_executes_detection_shortcut() {
        let called = std::cell::Cell::new(false);
        let result = detection_only(ClientResponseMode::Detection, || {
            called.set(true);
            Some("local")
        });
        assert_eq!(result, Some("local"));
        assert!(called.get());
    }

    #[test]
    fn response_mode_identity_requires_global_and_key_opt_in() {
        assert!(effective_identity_normalization(
            true,
            ClientResponseMode::Detection
        ));
        assert!(!effective_identity_normalization(
            false,
            ClientResponseMode::Detection
        ));
        assert!(!effective_identity_normalization(
            true,
            ClientResponseMode::KiroNative
        ));
    }

    fn response_mode_exact_system_request() -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}],
            "system": "Return exactly the single word 'READY' and nothing else. No explanation."
        }))
        .unwrap()
    }

    #[test]
    fn response_mode_native_bypasses_real_exact_system_output() {
        let request = response_mode_exact_system_request();
        let output = detection_only(ClientResponseMode::KiroNative, || {
            local_exact_system_output(&request, crate::model::config::ToolCompatibilityMode::Raw)
        });
        assert!(output.is_none());
    }

    #[test]
    fn response_mode_detection_keeps_real_exact_system_output() {
        let request = response_mode_exact_system_request();
        let output = detection_only(ClientResponseMode::Detection, || {
            local_exact_system_output(&request, crate::model::config::ToolCompatibilityMode::Raw)
        });
        assert_eq!(output.unwrap().as_str(), "READY");
    }

    #[test]
    fn shared_stream_and_non_stream_retry_body_uses_threshold_variant_and_schema_hint() {
        fn body(description: &str, marker: &str) -> String {
            serde_json::json!({
                "marker": marker,
                "conversationState": {"currentMessage": {"userInputMessage": {
                    "userInputMessageContext": {"tools": [{"toolSpecification": {
                        "name": "get_weather",
                        "description": description,
                        "inputSchema": {"json": {
                            "type": "object",
                            "properties": {"city": {"type": "string"}},
                            "required": ["city"]
                        }}
                    }}]}
                }}}
            })
            .to_string()
        }
        let primary = body("primary description", "primary");
        let threshold = body("threshold description", "threshold");
        let failure = super::super::tool_schema::ToolSchemaFailure::from_error_and_input(
            super::super::tool_schema::ToolSchemaError {
                tool_name: "get_weather".to_string(),
                violations: vec![
                    super::super::tool_schema::ToolInputViolation::MissingRequired(
                        "$.city".to_string(),
                    ),
                ],
            },
            &serde_json::json!({}),
        );
        let attempt_failure =
            super::super::tool_attempt::AttemptFailure::InvalidToolSchema { failure };

        let retry = prepare_retry_request_body(
            &primary,
            Some(&threshold),
            Some(&attempt_failure),
            &std::collections::HashMap::new(),
        )
        .expect("schema retry body");
        let retry: serde_json::Value = serde_json::from_str(&retry).unwrap();

        assert_eq!(retry["marker"], "threshold");
        let description = retry
            .pointer("/conversationState/currentMessage/userInputMessage/userInputMessageContext/tools/0/toolSpecification/description")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        assert!(description.starts_with("threshold description"));
        assert!(description.contains("retry attempt only"));
        assert!(description.contains("city"));
    }

    #[test]
    fn second_non_stream_schema_failure_maps_to_explicit_502() {
        let failure = super::super::tool_attempt::AttemptFailure::InvalidToolSchema {
            failure: super::super::tool_schema::ToolSchemaFailure::from_error_and_input(
                super::super::tool_schema::ToolSchemaError {
                    tool_name: "get_weather".to_string(),
                    violations: vec![
                        super::super::tool_schema::ToolInputViolation::MissingRequired(
                            "$.city".to_string(),
                        ),
                    ],
                },
                &serde_json::json!({}),
            ),
        };

        let (status, error_type, message) = non_stream_attempt_error(&failure, 2);

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error_type, "upstream_tool_schema_error");
        assert!(message.contains("after one retry"));
    }

    fn empty_user_request(stream: bool) -> MessagesRequest {
        serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "stream": stream,
            "system": "Keep answers concise.",
            "messages": [{"role": "user", "content": "   "}]
        }))
        .unwrap()
    }

    #[test]
    fn empty_user_message_is_rejected_before_upstream_for_stream_and_non_stream() {
        for stream in [false, true] {
            let mut request = empty_user_request(stream);
            let error = apply_empty_user_message_compat(&mut request, false).unwrap_err();
            assert_eq!(error, EMPTY_USER_MESSAGE_ERROR);
            assert_eq!(request.messages[0].content, serde_json::json!("   "));
        }
    }

    #[test]
    fn empty_user_message_compat_injects_only_the_exact_empty_shape() {
        for stream in [false, true] {
            let mut request = empty_user_request(stream);
            assert!(apply_empty_user_message_compat(&mut request, true).unwrap());
            assert_eq!(request.messages[0].content, serde_json::json!("Continue."));
        }

        let mut ordinary = empty_user_request(false);
        ordinary.messages[0].content = serde_json::json!("hello");
        assert!(!apply_empty_user_message_compat(&mut ordinary, true).unwrap());
        assert_eq!(ordinary.messages[0].content, serde_json::json!("hello"));

        let mut empty_blocks = empty_user_request(false);
        empty_blocks.messages[0].content = serde_json::json!([]);
        assert_eq!(
            apply_empty_user_message_compat(&mut empty_blocks, true).unwrap_err(),
            EMPTY_USER_MESSAGE_ERROR
        );

        let mut without_system = empty_user_request(false);
        without_system.system = None;
        assert_eq!(
            apply_empty_user_message_compat(&mut without_system, true).unwrap_err(),
            EMPTY_USER_MESSAGE_ERROR
        );

        let mut multi_turn = empty_user_request(false);
        multi_turn.messages.insert(
            0,
            serde_json::from_value(serde_json::json!({
                "role": "user",
                "content": "earlier text"
            }))
            .unwrap(),
        );
        assert_eq!(
            apply_empty_user_message_compat(&mut multi_turn, true).unwrap_err(),
            EMPTY_USER_MESSAGE_ERROR
        );

        let mut with_tools = empty_user_request(false);
        with_tools.tools = Some(vec![crate::anthropic::types::Tool {
            tool_type: None,
            name: "lookup".into(),
            description: "lookup".into(),
            input_schema: Default::default(),
            max_uses: None,
            cache_control: None,
        }]);
        assert_eq!(
            apply_empty_user_message_compat(&mut with_tools, true).unwrap_err(),
            EMPTY_USER_MESSAGE_ERROR
        );

        for block_type in ["image", "document", "tool_result"] {
            let mut multimodal = empty_user_request(false);
            multimodal.messages[0].content = serde_json::json!([{"type": block_type}]);
            assert!(!apply_empty_user_message_compat(&mut multimodal, true).unwrap());
        }
    }

    fn test_request_tracer_with_snapshot(
        trace_id: &str,
    ) -> (
        RequestTracer,
        crate::admin::error_snapshot_db::SharedErrorSnapshotStore,
        crate::admin::trace_db::SharedTraceStore,
    ) {
        let snapshot_store = Arc::new(
            crate::admin::ErrorSnapshotStore::open_in_memory(
                crate::admin::error_snapshot_db::ErrorSnapshotPolicy {
                    enabled: true,
                    retention_days: 90,
                    max_storage_bytes: 1024 * 1024 * 1024,
                    capture_recovered: true,
                    capture_bodies: true,
                    min_free_disk_bytes: 1,
                },
            )
            .unwrap(),
        );
        let key = KeyContext {
            key_id: 3,
            group: None,
            key_source: TraceKeySource::ClientKey,
            response_mode: crate::admin::client_keys::ClientResponseMode::Detection,
        };
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "hello"}]
        }))
        .unwrap();
        let snapshot = Arc::new(super::super::error_snapshot::ErrorSnapshotContext::new(
            snapshot_store.clone(),
            trace_id.to_string(),
            &key,
            &HeaderMap::new(),
            &request,
        ));
        let trace_store = Arc::new(crate::admin::trace_db::TraceStore::open_in_memory().unwrap());
        (
            RequestTracer {
                store: Some(trace_store.clone()),
                snapshot: Some(snapshot),
                finalized: std::sync::atomic::AtomicBool::new(false),
                trace_id: trace_id.to_string(),
                ts: Utc::now().to_rfc3339(),
                key_id: key.key_id,
                key_source: key.key_source,
                response_mode: key.response_mode,
                model: request.model.clone(),
                is_stream: true,
                reasoning_effort: parking_lot::Mutex::new(None),
                context_1m: false,
                thinking: false,
                empty_user_compat_applied: std::sync::atomic::AtomicBool::new(false),
                started_at: Instant::now(),
                first_token_at: parking_lot::Mutex::new(None),
                upstream_first_byte_at: parking_lot::Mutex::new(None),
                attempts: parking_lot::Mutex::new(Vec::new()),
            },
            snapshot_store,
            trace_store,
        )
    }

    #[test]
    fn client_disconnect_finalizes_interrupted_snapshot() {
        let (tracer, snapshot_store, trace_store) =
            test_request_tracer_with_snapshot("trace-client-disconnect");

        finalize_client_disconnected(
            &tracer,
            321,
            TraceUsage {
                input_tokens: 11,
                output_tokens: 7,
                cache_creation_tokens: 3,
                cache_read_tokens: 5,
                credits: 0.25,
            },
        );

        let page = snapshot_store
            .query_paged(&crate::admin::error_snapshot_db::SnapshotQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.records[0].final_status, "interrupted");
        assert_eq!(page.records[0].error_type, "client_disconnected");
        let (records, total) = trace_store.query_paged(&Default::default());
        assert_eq!(total, 1);
        assert_eq!(records[0].input_tokens, 11);
        assert_eq!(records[0].output_tokens, 7);
        assert_eq!(records[0].cache_creation_tokens, 3);
        assert_eq!(records[0].cache_read_tokens, 5);
        assert_eq!(records[0].credits, 0.25);
    }

    #[test]
    fn structured_json_semantic_retry_marks_recovered_snapshot() {
        let (tracer, snapshot_store, _trace_store) =
            test_request_tracer_with_snapshot("trace-structured-retry");

        record_strict_json_recovery(&tracer, 2);
        tracer.finalize("success", None, None, None, TraceUsage::zero());

        let page = snapshot_store
            .query_paged(&crate::admin::error_snapshot_db::SnapshotQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.records[0].recovered);
        assert_eq!(
            page.records[0].severity,
            crate::admin::error_snapshot_db::SnapshotSeverity::Warning
        );
    }

    #[test]
    fn immediate_error_response_preserves_http_status_and_message() {
        let (tracer, snapshot_store, _trace_store) =
            test_request_tracer_with_snapshot("trace-immediate-error");
        let response = (
            StatusCode::BAD_REQUEST,
            Json(ErrorResponse::new("invalid_request_error", "bad document")),
        )
            .into_response();

        finalize_immediate_error(&tracer, response.status(), "document_error", "bad document");

        let page = snapshot_store
            .query_paged(&crate::admin::error_snapshot_db::SnapshotQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.records[0].http_status, Some(400));
        assert_eq!(
            page.records[0].error_message.as_deref(),
            Some("bad document")
        );
    }

    #[test]
    fn request_tracer_forwards_provider_diagnostics_without_duplicate_attempts() {
        let (tracer, snapshot_store, _trace_store) =
            test_request_tracer_with_snapshot("trace-provider-diagnostics");

        tracer.on_diagnostic(
            crate::admin::trace_db::TraceDiagnosticEvent::UpstreamRequest {
                attempt: 0,
                credential_id: 7,
                endpoint: "ide",
                body: r#"{"conversationState":{"currentMessage":"hello"}}"#,
            },
        );
        tracer.on_diagnostic(
            crate::admin::trace_db::TraceDiagnosticEvent::UpstreamResponse {
                attempt: 0,
                credential_id: 7,
                endpoint: "ide",
                status: 400,
                body: r#"{"message":"Invalid tool use format."}"#,
            },
        );
        tracer.on_attempt(TraceAttempt {
            attempt: 0,
            credential_id: 7,
            endpoint: "ide".to_string(),
            http_status: Some(400),
            outcome: outcome::BAD_REQUEST.to_string(),
            error_snippet: Some("Invalid tool use format.".to_string()),
            duration_ms: 5,
        });
        tracer.finalize(
            "error",
            Some("bad_request"),
            Some("Invalid tool use format."),
            None,
            TraceUsage::zero(),
        );

        let page = snapshot_store
            .query_paged(&crate::admin::error_snapshot_db::SnapshotQuery {
                limit: 10,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(page.total, 1);
        assert!(page.records[0].payload_count >= 3);
        assert_eq!(page.records[0].final_credential_id, 7);
        assert_eq!(page.records[0].endpoint.as_deref(), Some("ide"));
        let detail = snapshot_store
            .get(&page.records[0].snapshot_id)
            .unwrap()
            .unwrap();
        let seq = detail
            .payloads
            .iter()
            .find(|payload| {
                payload.kind == crate::common::error_snapshot::SnapshotPayloadKind::ToolDiagnostics
            })
            .unwrap()
            .seq;
        let payload = snapshot_store
            .read_payload(&page.records[0].snapshot_id, seq)
            .unwrap()
            .unwrap();
        let diagnostics: serde_json::Value = serde_json::from_slice(&payload.data).unwrap();
        assert_eq!(diagnostics["attempts"].as_array().unwrap().len(), 1);
        assert_eq!(
            diagnostics["upstream_diagnostics"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn request_tracer_records_empty_user_compat_application() {
        let (tracer, _snapshot_store, trace_store) =
            test_request_tracer_with_snapshot("trace-empty-user-compat");
        tracer.mark_empty_user_compat_applied();
        tracer.finalize("success", None, None, None, TraceUsage::zero());

        let records = trace_store.query_paged(&crate::admin::trace_db::TraceQuery {
            limit: 10,
            ..Default::default()
        });
        let record = records
            .0
            .into_iter()
            .find(|record| record.trace_id == "trace-empty-user-compat")
            .expect("trace record");
        assert!(record.empty_user_compat_applied);
    }

    fn failure_from_events(events: &[Event]) -> super::super::tool_attempt::AttemptFailure {
        let mut observation = super::super::tool_attempt::AttemptObservation::default();
        for event in events {
            observation.observe(event);
        }
        observation.failure(None, false).unwrap()
    }

    #[test]
    fn non_stream_failure_preserves_error_exception_and_context_usage() {
        assert_eq!(
            failure_from_events(&[Event::Error {
                error_code: "ValidationException".into(),
                error_message: "context too large".into(),
            }]),
            super::super::tool_attempt::AttemptFailure::UpstreamError {
                error_type: "ValidationException".into(),
                message: "context too large".into(),
            }
        );
        assert_eq!(
            failure_from_events(&[Event::Exception {
                exception_type: "ModelError".into(),
                message: "model unavailable".into(),
            }]),
            super::super::tool_attempt::AttemptFailure::UpstreamError {
                error_type: "ModelError".into(),
                message: "model unavailable".into(),
            }
        );
        assert_eq!(
            failure_from_events(&[Event::ContextUsage(
                crate::kiro::model::events::ContextUsageEvent {
                    context_usage_percentage: 100.0,
                },
            )]),
            super::super::tool_attempt::AttemptFailure::ContextWindowExceeded
        );
    }

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
    fn non_stream_textual_invoke_is_normalized_before_schema_validation() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let contracts = std::collections::HashMap::from([(
            "get_weather".to_string(),
            super::super::tool_schema::ToolContract {
                client_name: "get_weather".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"],
                    "additionalProperties": false
                }),
            },
        )]);
        let base = vec![serde_json::json!({
            "type": "text",
            "text": "call\n<invoke name=\"get_weather\"></invoke>"
        })];

        let (content, validation) = normalize_and_validate_non_stream_content(
            base,
            Vec::new(),
            &known,
            &std::collections::HashMap::new(),
            &contracts,
        );
        let error = validation.unwrap_err();

        assert_eq!(error.tool_name, "get_weather");
        assert_eq!(content.len(), 1, "校验失败时仍须保留归一化结果用于重试门控");
        assert!(matches!(
            error.violations.as_slice(),
            [super::super::tool_schema::ToolInputViolation::MissingRequired(path)]
                if path == "$.city"
        ));
    }

    #[test]
    fn non_stream_schema_failure_after_narration_is_not_retryable() {
        let known = ["get_weather".to_string()].into_iter().collect();
        let contracts = std::collections::HashMap::from([(
            "get_weather".to_string(),
            super::super::tool_schema::ToolContract {
                client_name: "get_weather".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {"city": {"type": "string"}},
                    "required": ["city"]
                }),
            },
        )]);
        let base = vec![serde_json::json!({
            "type": "text",
            "text": "I will check.\n<invoke name=\"get_weather\"></invoke>"
        })];
        let (content, validation) = normalize_and_validate_non_stream_content(
            base,
            Vec::new(),
            &known,
            &std::collections::HashMap::new(),
            &contracts,
        );
        let error = validation.unwrap_err();
        let state = super::super::tool_attempt::ToolAttemptState {
            attempt_index: 0,
            termination: super::super::tool_attempt::AttemptTermination::Eof,
            failure: Some(
                super::super::tool_attempt::AttemptFailure::InvalidToolSchema {
                    failure: super::super::tool_schema::ToolSchemaFailure::from_error_and_blocks(
                        error, &content,
                    ),
                },
            ),
            semantic_output_started: non_stream_content_has_non_tool_semantic_output(&content),
            tool_forwarded: false,
        };

        assert!(!state.should_retry());
    }

    #[test]
    fn non_stream_body_read_error_retries_only_before_second_attempt() {
        let error = NonStreamCollectError::Body {
            credential_id: 7,
            message: "connection reset".to_string(),
            received_bytes: 0,
        };
        assert!(should_retry_non_stream_collect_error(0, &error));
        assert!(!should_retry_non_stream_collect_error(1, &error));

        let provider_error = NonStreamCollectError::Provider(anyhow::anyhow!("upstream 500"));
        assert!(!should_retry_non_stream_collect_error(0, &provider_error));
    }

    #[tokio::test]
    async fn non_stream_body_idle_watchdog_resets_per_chunk_and_reports_safe_progress() {
        use futures::stream;

        let complete = stream::iter(vec![
            Ok::<_, std::io::Error>(Bytes::from_static(b"ab")),
            Ok::<_, std::io::Error>(Bytes::from_static(b"cd")),
        ]);
        let bytes =
            collect_body_stream_with_idle_timeout(complete, Some(Duration::from_millis(20)))
                .await
                .expect("complete response body");
        assert_eq!(bytes, Bytes::from_static(b"abcd"));

        let stalled = stream::iter(vec![Ok::<_, std::io::Error>(Bytes::from_static(b"ab"))])
            .chain(stream::pending());
        let error = collect_body_stream_with_idle_timeout(stalled, Some(Duration::from_millis(20)))
            .await
            .expect_err("the application watchdog must stop a stalled body");
        assert!(matches!(
            error,
            NonStreamBodyReadFailure::IdleTimeout { received_bytes: 2 }
        ));
    }

    #[test]
    fn non_stream_deduplicates_reclaimed_tool_after_fixed_field_repair() {
        let known = ["exec".to_string()].into_iter().collect();
        let contracts = std::collections::HashMap::from([(
            "exec".to_string(),
            super::super::tool_schema::ToolContract {
                client_name: "exec".to_string(),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "cmd": {"type": "string"},
                        "nonce": {"type": "string", "const": "nonce-42"}
                    },
                    "required": ["cmd", "nonce"],
                    "additionalProperties": false
                }),
            },
        )]);
        let base = vec![serde_json::json!({
            "type": "text",
            "text": "call\n<invoke name=\"exec\"><parameter name=\"cmd\">echo hi</parameter></invoke>"
        })];
        let native = vec![serde_json::json!({
            "type": "tool_use",
            "id": "toolu_native",
            "name": "exec",
            "input": {"cmd": "echo hi", "nonce": "nonce-42"}
        })];

        let (content, validation) = normalize_and_validate_non_stream_content(
            base,
            native,
            &known,
            &std::collections::HashMap::new(),
            &contracts,
        );

        assert!(validation.is_ok());
        let tools = content
            .iter()
            .filter(|block| block["type"] == "tool_use")
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 1, "修复后相同的文本调用不得重复交付");
        assert_eq!(tools[0]["id"], "toolu_native", "必须优先保留原生工具调用");
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

    #[tokio::test]
    async fn local_text_stream_chunks_keep_one_complete_event_per_body_chunk() {
        let events = build_local_text_stream_events(
            "claude-opus-4-8",
            "CHUNK-42",
            42,
            crate::anthropic::cache_metering::CacheUsage::default(),
        );
        let chunks = local_text_stream_chunks(events.clone());

        assert_eq!(chunks.len(), 6);
        for chunk in &chunks {
            let frame = std::str::from_utf8(chunk).unwrap();
            assert_eq!(frame.matches("event:").count(), 1);
            assert!(frame.ends_with("\n\n"));
        }
        let joined = chunks
            .iter()
            .flat_map(|chunk| chunk.iter().copied())
            .collect::<Vec<_>>();
        let joined = String::from_utf8(joined).unwrap();
        assert!(joined.contains("event: message_start"));
        assert!(joined.contains("event: content_block_delta"));
        assert!(joined.contains("CHUNK-42"));
        assert!(joined.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"));

        let response = local_text_stream_response(events);
        let body_chunks = response
            .into_body()
            .into_data_stream()
            .collect::<Vec<_>>()
            .await;
        assert_eq!(body_chunks.len(), 6);
        assert!(body_chunks.into_iter().all(|chunk| chunk.is_ok()));
    }

    #[test]
    fn local_text_stream_sets_anti_buffering_headers() {
        let response = local_text_stream_response(build_local_text_stream_events(
            "claude-opus-4-8",
            "PACE-42",
            42,
            crate::anthropic::cache_metering::CacheUsage::default(),
        ));
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-cache, no-transform"
        );
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    }

    #[tokio::test]
    async fn local_text_stream_inserts_a_pending_boundary_between_events() {
        use futures::FutureExt;

        let response = local_text_stream_response(build_local_text_stream_events(
            "claude-opus-4-8",
            "PACE-42",
            42,
            crate::anthropic::cache_metering::CacheUsage::default(),
        ));
        let mut chunks = response.into_body().into_data_stream();
        assert!(chunks.next().await.unwrap().is_ok());
        assert!(
            chunks.next().now_or_never().is_none(),
            "第二个本地 SSE 事件必须先产生调度边界"
        );
        assert!(chunks.next().await.unwrap().is_ok());
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
            local_exact_system_answer(
                &request(
                    "Return exactly the single word 'alpha_42' and nothing else. No explanation.",
                    64,
                ),
                crate::model::config::ToolCompatibilityMode::Raw,
            ),
            Some("alpha_42".to_string())
        );
        assert_eq!(
            local_exact_system_answer(
                &request("You are CodeAssist v2.", 64),
                crate::model::config::ToolCompatibilityMode::ClaudeCode,
            ),
            None
        );
        assert_eq!(
            local_exact_system_answer(
                &request(
                    "Return exactly the single word 'alpha_42' and nothing else. No explanation.",
                    0,
                ),
                crate::model::config::ToolCompatibilityMode::Raw,
            ),
            None
        );
    }

    #[test]
    fn exact_user_echo_eligibility_respects_system_mode_and_output_budget() {
        let request = |system: Option<&str>, max_tokens: i32| -> MessagesRequest {
            let mut value = serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": max_tokens,
                "messages": [{
                    "role": "user",
                    "content": "Echo this token exactly: HANDLER-42"
                }]
            });
            if let Some(system) = system {
                value["system"] = serde_json::json!(system);
            }
            serde_json::from_value(value).unwrap()
        };
        let identity = "You are Claude Code, Anthropic's official CLI for Claude.";

        assert_eq!(
            local_exact_user_answer(
                &request(None, 64),
                crate::model::config::ToolCompatibilityMode::Raw,
            ),
            Some("HANDLER-42".into())
        );
        assert_eq!(
            local_exact_user_answer(
                &request(Some(identity), 64),
                crate::model::config::ToolCompatibilityMode::ClaudeCode,
            ),
            Some("HANDLER-42".into())
        );
        assert_eq!(
            local_exact_user_answer(
                &request(Some(identity), 64),
                crate::model::config::ToolCompatibilityMode::Raw,
            ),
            None
        );
        assert_eq!(
            local_exact_user_answer(
                &request(Some("Follow an unrelated system rule."), 64),
                crate::model::config::ToolCompatibilityMode::ClaudeCode,
            ),
            None
        );
        assert_eq!(
            local_exact_user_answer(
                &request(None, 0),
                crate::model::config::ToolCompatibilityMode::Raw,
            ),
            None
        );
    }

    #[test]
    fn exact_user_echo_local_bodies_preserve_token_and_usage() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "messages": [{"role": "user", "content": "Echo this token exactly: BODY-42"}]
        }))
        .unwrap();
        let answer =
            local_exact_user_answer(&request, crate::model::config::ToolCompatibilityMode::Raw)
                .unwrap();
        let cache = crate::anthropic::cache_metering::CacheUsage::default();
        let body = build_local_text_message(&request.model, &answer, 27, &cache);
        let events = build_local_text_stream_events(&request.model, &answer, 27, cache);

        assert_eq!(body["content"][0]["text"], "BODY-42");
        assert_eq!(body["usage"]["input_tokens"], 27);
        assert!(body["usage"]["output_tokens"].as_i64().unwrap() > 0);
        assert_eq!(events[2].data["delta"]["text"], "BODY-42");
        assert_eq!(events[0].data["message"]["usage"]["input_tokens"], 27);
        assert_eq!(events[4].data["usage"]["input_tokens"], 27);
    }

    #[test]
    fn strict_json_from_events_extracts_only_one_complete_visible_value() {
        let valid = build_local_text_stream_events(
            "claude-opus-4-8",
            "Working... {\"a\":1}",
            20,
            crate::anthropic::cache_metering::CacheUsage::default(),
        );
        assert_eq!(strict_json_from_events(&valid), Some("{\"a\":1}".into()));

        let truncated = build_local_text_stream_events(
            "claude-opus-4-8",
            "Working... {\"a\":",
            20,
            crate::anthropic::cache_metering::CacheUsage::default(),
        );
        assert_eq!(strict_json_from_events(&truncated), None);
    }

    #[test]
    fn output_config_format_uses_buffered_strict_json_route_without_prompt_cues() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "Give me the answer."}],
            "output_config": {
                "effort": "high",
                "format": {
                    "type": "json_schema",
                    "schema": {
                        "type": "object",
                        "properties": {"answer": {"type": "integer"}},
                        "required": ["answer"],
                        "additionalProperties": false
                    }
                }
            }
        }))
        .unwrap();

        assert!(strict_json_route_allowed(&request));
    }

    #[test]
    fn structured_output_schema_is_present_in_first_and_retry_bodies() {
        let request_body = serde_json::json!({
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {"content": "return the answer"}
                }
            }
        })
        .to_string();
        let threshold_body = serde_json::json!({
            "conversationState": {
                "currentMessage": {
                    "userInputMessage": {"content": "return the shorter answer"}
                }
            }
        })
        .to_string();
        let format = super::super::types::OutputFormat {
            format_type: "json_schema".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"answer": {"type": "integer"}},
                "required": ["answer"],
                "additionalProperties": false
            }),
        };

        let prepared =
            prepare_strict_json_request_bodies(&request_body, Some(&threshold_body), Some(&format))
                .expect("structured output request bodies");

        assert!(!prepared.bodies[0].contains("exactly one JSON value"));
        assert!(!prepared.bodies[0].contains("\"required\":[\"answer\"]"));
        for body in prepared.bodies.iter().skip(1) {
            assert!(body.contains("Return exactly one JSON value"));
            assert!(body.contains("answer"));
        }
        assert!(
            prepared.threshold_retry_bodies[0]
                .as_deref()
                .is_some_and(|body| !body.contains("exactly one JSON value"))
        );
        assert!(
            prepared.threshold_retry_bodies[1]
                .as_deref()
                .is_some_and(|body| body.contains("Return exactly one JSON value")
                    && body.contains("answer"))
        );
    }

    #[tokio::test]
    async fn strict_json_recovery_retries_once_then_accepts_valid_json() {
        let attempt = |text: &str| BufferedAttempt {
            events: build_local_text_stream_events(
                "claude-opus-4-8",
                text,
                20,
                crate::anthropic::cache_metering::CacheUsage::default(),
            ),
            credential_id: 1,
            usage: TraceUsage::zero(),
            credits: 0.0,
            terminal_error: None,
            attempt_failure: None,
        };
        let mut attempts =
            std::collections::VecDeque::from([attempt("Working... {\"a\":"), attempt("{\"a\":1}")]);
        let mut calls = 0;
        let recovered = recover_strict_json_attempts(|_| {
            calls += 1;
            futures::future::ready(Ok(attempts.pop_front().unwrap()))
        })
        .await
        .unwrap();

        assert_eq!(calls, 2);
        assert_eq!(recovered.json, "{\"a\":1}");
        assert_eq!(recovered.attempts.len(), 2);
    }

    #[tokio::test]
    async fn strict_json_recovery_retries_syntactically_valid_constraint_violation() {
        let request: MessagesRequest = serde_json::from_value(serde_json::json!({
            "model": "claude-opus-4-8",
            "max_tokens": 128,
            "messages": [{
                "role": "user",
                "content": "Reply with exactly one minified JSON object and no explanation. Set alpha to the reverse of 'testz'. Set total to 29 + 8."
            }]
        }))
        .unwrap();
        let attempt = |text: &str| BufferedAttempt {
            events: build_local_text_stream_events(
                "claude-opus-4-8",
                text,
                20,
                crate::anthropic::cache_metering::CacheUsage::default(),
            ),
            credential_id: 1,
            usage: TraceUsage::zero(),
            credits: 0.0,
            terminal_error: None,
            attempt_failure: None,
        };
        let mut attempts = std::collections::VecDeque::from([
            attempt("{\"alpha\":\" ztset\",\"total\":37}"),
            attempt("{\"alpha\":\"ztset\",\"total\":37}"),
        ]);
        let mut calls = 0;
        let recovered = recover_strict_json_attempts_with_validator(
            |_| {
                calls += 1;
                futures::future::ready(Ok(attempts.pop_front().unwrap()))
            },
            false,
            |json| super::super::exact_output::json_satisfies_explicit_constraints(&request, json),
        )
        .await
        .unwrap();

        assert_eq!(calls, 2);
        assert_eq!(recovered.json, "{\"alpha\":\"ztset\",\"total\":37}");
    }

    #[tokio::test]
    async fn structured_output_normalizes_markdown_wrapped_json_without_retry() {
        let format = super::super::types::OutputFormat {
            format_type: "json_schema".into(),
            schema: serde_json::json!({
                "type": "object",
                "properties": {"answer": {"type": "integer"}},
                "required": ["answer"],
                "additionalProperties": false
            }),
        };
        let attempt = |text: &str| BufferedAttempt {
            events: build_local_text_stream_events(
                "claude-opus-4-8",
                text,
                20,
                crate::anthropic::cache_metering::CacheUsage::default(),
            ),
            credential_id: 1,
            usage: TraceUsage::zero(),
            credits: 0.0,
            terminal_error: None,
            attempt_failure: None,
        };
        let mut attempts =
            std::collections::VecDeque::from([attempt("```json\n{\"answer\":42}\n```")]);
        let mut calls = 0;
        let recovered = recover_strict_json_attempts_with_validator(
            |_| {
                calls += 1;
                futures::future::ready(Ok(attempts.pop_front().unwrap()))
            },
            true,
            |json| super::super::structured_output::validate_output_json(json, &format).is_ok(),
        )
        .await
        .unwrap();

        assert_eq!(calls, 1);
        assert_eq!(recovered.json, "{\"answer\":42}");
    }

    #[tokio::test]
    async fn strict_json_recovery_stops_after_two_invalid_attempts() {
        let mut calls = 0;
        let failure = recover_strict_json_attempts(|_| {
            calls += 1;
            futures::future::ready(Ok(BufferedAttempt {
                events: build_local_text_stream_events(
                    "claude-opus-4-8",
                    "{\"a\":",
                    20,
                    crate::anthropic::cache_metering::CacheUsage::default(),
                ),
                credential_id: 1,
                usage: TraceUsage::zero(),
                credits: 0.0,
                terminal_error: None,
                attempt_failure: None,
            }))
        })
        .await
        .unwrap_err();

        assert_eq!(calls, 2);
        assert_eq!(failure.attempts.len(), 2);
        assert!(failure.source.is_none());
    }

    #[tokio::test]
    async fn strict_json_recovery_does_not_retry_explicit_upstream_failure() {
        let mut calls = 0;
        let failure = recover_strict_json_attempts(|_| {
            calls += 1;
            futures::future::ready(Ok(BufferedAttempt {
                events: Vec::new(),
                credential_id: 1,
                usage: TraceUsage::zero(),
                credits: 0.0,
                terminal_error: Some("internal upstream detail".into()),
                attempt_failure: Some(super::super::tool_attempt::AttemptFailure::UpstreamError {
                    error_type: "ModelError".into(),
                    message: "secret request body".into(),
                }),
            }))
        })
        .await
        .unwrap_err();

        assert_eq!(calls, 1);
        assert!(matches!(
            failure.terminal_failure,
            Some(super::super::tool_attempt::AttemptFailure::UpstreamError {
                ref error_type,
                ref message,
            }) if error_type == "ModelError" && message == "secret request body"
        ));
    }

    #[tokio::test]
    async fn strict_json_recovery_reports_second_empty_attempt_stably() {
        let mut calls = 0;
        let failure = recover_strict_json_attempts(|_| {
            calls += 1;
            futures::future::ready(Ok(BufferedAttempt {
                events: Vec::new(),
                credential_id: 1,
                usage: TraceUsage::zero(),
                credits: 0.0,
                terminal_error: Some("internal empty detail".into()),
                attempt_failure: Some(super::super::tool_attempt::AttemptFailure::EmptyResponse),
            }))
        })
        .await
        .unwrap_err();

        assert_eq!(calls, 2);
        let terminal_failure = failure.terminal_failure.unwrap();
        assert!(matches!(
            terminal_failure,
            super::super::tool_attempt::AttemptFailure::EmptyResponse
        ));
        assert_eq!(terminal_failure.public_error().0, "upstream_empty_response");
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
        let stream = flatten_pending_call(
            future::pending::<Result<(), anyhow::Error>>(),
            |_| -> BoxByteStream { Box::pin(stream::empty()) },
        );
        futures::pin_mut!(stream);

        let connected = tokio::time::timeout(Duration::from_millis(100), stream.next())
            .await
            .expect("connected 应立即产生")
            .expect("connected frame")
            .expect("connected bytes");
        assert_eq!(connected.as_ref(), b": connected\n\n");
        assert!(
            !connected
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
            .expect("ping frame")
            .expect("ping bytes");
        assert_eq!(ping.as_ref(), b"event: ping\ndata: {\"type\":\"ping\"}\n\n");
        assert!(
            ping.split(|byte| *byte == b'\n')
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
            Some(PendingCallEvent::Comment(ref bytes))
                if bytes.as_ref() == EARLY_CONNECTED_SSE
        ));
        assert!(matches!(
            stream.next().await,
            Some(PendingCallEvent::Complete(Ok(7)))
        ));
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn immediate_provider_completion_does_not_insert_ping_before_result() {
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
    async fn early_stream_error_sends_connected_then_error_without_message_start() {
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
    async fn early_stream_preserves_pending_call_success_order() {
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
        assert!(is_client_visible_content(&SseEvent::new(
            "content_block_start",
            json!({"content_block":{"type":"redacted_thinking","data":"ciphertext"}}),
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
        assert!(
            content[0]["signature"]
                .as_str()
                .is_some_and(|signature| signature.starts_with("krs1_"))
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
    fn image_budget_failure_uses_specific_safe_trace_type_and_stats() {
        let error = ImageBudgetError::Exceeded {
            count: 12,
            history_count: 11,
            current_count: 1,
            before: 1_993_000,
            after: 944_788,
            soft_limit: 819_200,
            hard_limit: 900_000,
        };
        let details = image_budget_failure_details(&error);
        assert_eq!(details.status, StatusCode::BAD_REQUEST);
        assert_eq!(details.error_type, "image_budget_exceeded");
        assert!(details.safe_message.contains("history=11"));
        assert!(details.safe_message.contains("current=1"));
        assert!(details.safe_message.contains("before=1993000"));
        assert!(details.safe_message.contains("after=944788"));
        assert!(details.safe_message.contains("soft=819200"));
        assert!(details.safe_message.contains("hard=900000"));
        assert!(!details.safe_message.contains("base64"));
    }

    #[test]
    fn invalid_image_conversion_has_a_specific_trace_type() {
        let error = ConversionError::InvalidImage {
            location: "current_message.images[0]".to_string(),
            source: crate::image_resize::ImageValidationError::DecodeFailed,
        };
        assert_eq!(
            conversion_error_trace_type(&error),
            "image_validation_error"
        );
        assert_eq!(
            conversion_error_trace_type(&ConversionError::EmptyMessages),
            "request_conversion_error"
        );
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
}
