use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use axum::http::{HeaderMap, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use super::stream::SseEvent;
use super::types::MessagesRequest;

pub(crate) const HIGH_CONTEXT_PERCENTAGE: f64 = 80.0;
pub(crate) const HIGH_PAYLOAD_BYTES: u64 = 2_500_000;
const PERCENTAGE_SCALE: f64 = 10_000.0;
const UNKNOWN_U64: u64 = u64::MAX;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompactionTraceData {
    pub session_hash: Option<String>,
    pub client_version: Option<String>,
    pub diagnosis: String,
    pub request_body_bytes: u64,
    pub upstream_context_tokens: Option<u64>,
    pub upstream_context_percentage: Option<f64>,
    pub client_reported_tokens: Option<u64>,
    pub diagnostics_json: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct DiagnosisFacts {
    pub request_body_bytes: u64,
    pub upstream_request_max_bytes: u64,
    pub upstream_context_tokens: Option<u64>,
    pub upstream_context_percentage: Option<f64>,
    pub client_reported_tokens: Option<u64>,
    pub message_start_enqueued: bool,
    pub client_disconnected: bool,
    pub payload_limit_observed: bool,
}

pub(crate) struct CompactionFinalize<'a> {
    pub final_status: &'a str,
    pub error_type: Option<&'a str>,
    pub error_message: Option<&'a str>,
    pub is_stream: bool,
    pub usage_input_tokens: u64,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestShape {
    message_count: u64,
    system_count: u64,
    tool_count: u64,
    image_count: u64,
    tool_use_count: u64,
    tool_result_count: u64,
    message_bytes: u64,
    system_bytes: u64,
    tool_schema_bytes: u64,
    image_bytes: u64,
    tool_use_bytes: u64,
    tool_result_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SafeDiagnosticSnapshot<'a> {
    schema_version: u8,
    current_diagnosis: &'a str,
    known_third_party_autocompact_regression_possible: bool,
    request_shape: &'a RequestShape,
    request_body_bytes: u64,
    upstream_request_count: u64,
    upstream_request_first_bytes: Option<u64>,
    upstream_request_last_bytes: Option<u64>,
    upstream_request_min_bytes: Option<u64>,
    upstream_request_max_bytes: Option<u64>,
    context_usage_event_count: u64,
    metering_event_count: u64,
    upstream_context_tokens: Option<u64>,
    upstream_context_percentage: Option<f64>,
    upstream_context_limit_reached: bool,
    client_reported_tokens: Option<u64>,
    message_start_enqueued: bool,
    message_delta_enqueued: bool,
    context_window_exceeded_enqueued: bool,
    message_stop_enqueued: bool,
    client_error_enqueued: bool,
    semantic_output_enqueued: bool,
    probation_semantic_output_started: bool,
    probation_retry_considered: bool,
    probation_retry_started: bool,
    client_disconnected: bool,
    payload_limit_observed: bool,
    final_status: &'a str,
    final_error_type: Option<&'a str>,
}

#[derive(Debug)]
struct EnabledDiagnostics {
    session_hash: Option<String>,
    client_version: Option<String>,
    request_body_bytes: u64,
    request_shape: RequestShape,
    upstream_request_count: AtomicU64,
    upstream_request_first_bytes: AtomicU64,
    upstream_request_last_bytes: AtomicU64,
    upstream_request_min_bytes: AtomicU64,
    upstream_request_max_bytes: AtomicU64,
    context_usage_event_count: AtomicU64,
    metering_event_count: AtomicU64,
    upstream_context_tokens: AtomicU64,
    upstream_context_percentage_scaled: AtomicU64,
    upstream_context_limit_reached: AtomicBool,
    client_reported_tokens: AtomicU64,
    message_start_enqueued: AtomicBool,
    message_delta_enqueued: AtomicBool,
    context_window_exceeded_enqueued: AtomicBool,
    message_stop_enqueued: AtomicBool,
    client_error_enqueued: AtomicBool,
    semantic_output_enqueued: AtomicBool,
    probation_semantic_output_started: AtomicBool,
    probation_retry_considered: AtomicBool,
    probation_retry_started: AtomicBool,
    payload_limit_observed: AtomicBool,
}

#[derive(Debug, Default)]
pub(crate) struct CompactionDiagnostics {
    enabled: Option<EnabledDiagnostics>,
}

impl CompactionDiagnostics {
    pub(crate) fn new(enabled: bool, headers: &HeaderMap, request: &MessagesRequest) -> Self {
        if !enabled {
            return Self::default();
        }

        let request_body_bytes = headers
            .get(header::CONTENT_LENGTH)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or_else(|| {
                serde_json::to_vec(request)
                    .map(|bytes| bytes.len() as u64)
                    .unwrap_or(0)
            });
        let session_hash = request
            .metadata
            .as_ref()
            .and_then(|metadata| metadata.user_id.as_deref())
            .filter(|value| !value.is_empty())
            .map(|value| hex::encode(Sha256::digest(value.as_bytes())));

        Self {
            enabled: Some(EnabledDiagnostics {
                session_hash,
                client_version: extract_client_version(headers),
                request_body_bytes,
                request_shape: request_shape(request),
                upstream_request_count: AtomicU64::new(0),
                upstream_request_first_bytes: AtomicU64::new(UNKNOWN_U64),
                upstream_request_last_bytes: AtomicU64::new(UNKNOWN_U64),
                upstream_request_min_bytes: AtomicU64::new(UNKNOWN_U64),
                upstream_request_max_bytes: AtomicU64::new(0),
                context_usage_event_count: AtomicU64::new(0),
                metering_event_count: AtomicU64::new(0),
                upstream_context_tokens: AtomicU64::new(UNKNOWN_U64),
                upstream_context_percentage_scaled: AtomicU64::new(UNKNOWN_U64),
                upstream_context_limit_reached: AtomicBool::new(false),
                client_reported_tokens: AtomicU64::new(UNKNOWN_U64),
                message_start_enqueued: AtomicBool::new(false),
                message_delta_enqueued: AtomicBool::new(false),
                context_window_exceeded_enqueued: AtomicBool::new(false),
                message_stop_enqueued: AtomicBool::new(false),
                client_error_enqueued: AtomicBool::new(false),
                semantic_output_enqueued: AtomicBool::new(false),
                probation_semantic_output_started: AtomicBool::new(false),
                probation_retry_considered: AtomicBool::new(false),
                probation_retry_started: AtomicBool::new(false),
                payload_limit_observed: AtomicBool::new(false),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_enabled(&self) -> bool {
        self.enabled.is_some()
    }

    pub(crate) fn observe_upstream_request(&self, body_bytes: usize) {
        let Some(state) = &self.enabled else {
            return;
        };
        let body_bytes = body_bytes as u64;
        state.upstream_request_count.fetch_add(1, Ordering::Relaxed);
        let _ = state.upstream_request_first_bytes.compare_exchange(
            UNKNOWN_U64,
            body_bytes,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        state
            .upstream_request_last_bytes
            .store(body_bytes, Ordering::Relaxed);
        state
            .upstream_request_min_bytes
            .fetch_min(body_bytes, Ordering::Relaxed);
        state
            .upstream_request_max_bytes
            .fetch_max(body_bytes, Ordering::Relaxed);
    }

    pub(crate) fn observe_upstream_response(&self, body: &str) {
        if body.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD") {
            self.mark_payload_limit_observed();
        }
    }

    pub(crate) fn observe_upstream_context(&self, percentage: f64, window_tokens: i32) {
        let Some(state) = &self.enabled else {
            return;
        };
        if !percentage.is_finite() || percentage < 0.0 || window_tokens <= 0 {
            return;
        }
        let percentage = percentage.min(1000.0);
        state
            .context_usage_event_count
            .fetch_add(1, Ordering::Relaxed);
        state.upstream_context_tokens.store(
            (percentage * f64::from(window_tokens) / 100.0)
                .round()
                .max(0.0) as u64,
            Ordering::Relaxed,
        );
        state.upstream_context_percentage_scaled.store(
            (percentage * PERCENTAGE_SCALE).round() as u64,
            Ordering::Relaxed,
        );
        if percentage >= 100.0 {
            state
                .upstream_context_limit_reached
                .store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn observe_metering_event(&self) {
        if let Some(state) = &self.enabled {
            state.metering_event_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub(crate) fn observe_client_event_enqueued(&self, event: &SseEvent) {
        let Some(state) = &self.enabled else {
            return;
        };
        match event.event.as_str() {
            "message_start" => {
                state.message_start_enqueued.store(true, Ordering::Relaxed);
                let usage = event
                    .data
                    .pointer("/message/usage")
                    .or_else(|| event.data.get("usage"));
                if let Some(usage) = usage {
                    let total = [
                        "input_tokens",
                        "cache_creation_input_tokens",
                        "cache_read_input_tokens",
                    ]
                    .iter()
                    .filter_map(|name| usage.get(*name).and_then(serde_json::Value::as_u64))
                    .sum();
                    state.client_reported_tokens.store(total, Ordering::Relaxed);
                }
            }
            "message_delta" => {
                state.message_delta_enqueued.store(true, Ordering::Relaxed);
                if event
                    .data
                    .pointer("/delta/stop_reason")
                    .and_then(serde_json::Value::as_str)
                    == Some("model_context_window_exceeded")
                {
                    state
                        .context_window_exceeded_enqueued
                        .store(true, Ordering::Relaxed);
                }
            }
            "message_stop" => state.message_stop_enqueued.store(true, Ordering::Relaxed),
            "error" => state.client_error_enqueued.store(true, Ordering::Relaxed),
            "content_block_start" | "content_block_delta" => {
                if event_has_semantic_output(event) {
                    state
                        .semantic_output_enqueued
                        .store(true, Ordering::Relaxed);
                }
            }
            _ => {}
        }
    }

    pub(crate) fn observe_non_stream_client_usage(&self, input_tokens: u64) {
        let Some(state) = &self.enabled else {
            return;
        };
        state
            .client_reported_tokens
            .store(input_tokens, Ordering::Relaxed);
        // 非流式响应没有 message_start 事件；该字段在诊断中统一表示 usage 已暴露给客户端。
        state.message_start_enqueued.store(true, Ordering::Relaxed);
    }

    pub(crate) fn observe_probation(
        &self,
        semantic_output_started: bool,
        retry_considered: bool,
        retry_started: bool,
    ) {
        let Some(state) = &self.enabled else {
            return;
        };
        if semantic_output_started {
            state
                .probation_semantic_output_started
                .store(true, Ordering::Relaxed);
        }
        if retry_considered {
            state
                .probation_retry_considered
                .store(true, Ordering::Relaxed);
        }
        if retry_started {
            state.probation_retry_started.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn mark_payload_limit_observed(&self) {
        if let Some(state) = &self.enabled {
            state.payload_limit_observed.store(true, Ordering::Relaxed);
        }
    }

    pub(crate) fn finalize(&self, outcome: CompactionFinalize<'_>) -> Option<CompactionTraceData> {
        let state = self.enabled.as_ref()?;
        let upstream_context_tokens = optional_atomic(&state.upstream_context_tokens);
        let upstream_context_percentage =
            optional_atomic(&state.upstream_context_percentage_scaled)
                .map(|value| value as f64 / PERCENTAGE_SCALE);
        let mut client_reported_tokens = optional_atomic(&state.client_reported_tokens);
        let mut message_start_enqueued = state.message_start_enqueued.load(Ordering::Relaxed);
        if !outcome.is_stream && outcome.final_status == "success" {
            if outcome.usage_input_tokens > 0 || client_reported_tokens.is_none() {
                client_reported_tokens = Some(outcome.usage_input_tokens);
            }
            message_start_enqueued = true;
        }
        let client_disconnected = outcome.error_type == Some("client_disconnected");
        let payload_limit_observed = state.payload_limit_observed.load(Ordering::Relaxed)
            || outcome
                .error_message
                .is_some_and(|message| message.contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD"));
        let upstream_request_max_bytes = state.upstream_request_max_bytes.load(Ordering::Relaxed);
        let facts = DiagnosisFacts {
            request_body_bytes: state.request_body_bytes,
            upstream_request_max_bytes,
            upstream_context_tokens,
            upstream_context_percentage,
            client_reported_tokens,
            message_start_enqueued,
            client_disconnected,
            payload_limit_observed,
        };
        let diagnosis = classify_diagnosis(&facts);
        let snapshot = SafeDiagnosticSnapshot {
            schema_version: 1,
            current_diagnosis: diagnosis,
            known_third_party_autocompact_regression_possible:
                known_third_party_autocompact_regression_possible(state.client_version.as_deref()),
            request_shape: &state.request_shape,
            request_body_bytes: state.request_body_bytes,
            upstream_request_count: state.upstream_request_count.load(Ordering::Relaxed),
            upstream_request_first_bytes: optional_atomic(&state.upstream_request_first_bytes),
            upstream_request_last_bytes: optional_atomic(&state.upstream_request_last_bytes),
            upstream_request_min_bytes: optional_atomic(&state.upstream_request_min_bytes),
            upstream_request_max_bytes: (upstream_request_max_bytes > 0)
                .then_some(upstream_request_max_bytes),
            context_usage_event_count: state.context_usage_event_count.load(Ordering::Relaxed),
            metering_event_count: state.metering_event_count.load(Ordering::Relaxed),
            upstream_context_tokens,
            upstream_context_percentage,
            upstream_context_limit_reached: state
                .upstream_context_limit_reached
                .load(Ordering::Relaxed),
            client_reported_tokens,
            message_start_enqueued,
            message_delta_enqueued: state.message_delta_enqueued.load(Ordering::Relaxed),
            context_window_exceeded_enqueued: state
                .context_window_exceeded_enqueued
                .load(Ordering::Relaxed),
            message_stop_enqueued: state.message_stop_enqueued.load(Ordering::Relaxed),
            client_error_enqueued: state.client_error_enqueued.load(Ordering::Relaxed),
            semantic_output_enqueued: state.semantic_output_enqueued.load(Ordering::Relaxed),
            probation_semantic_output_started: state
                .probation_semantic_output_started
                .load(Ordering::Relaxed),
            probation_retry_considered: state.probation_retry_considered.load(Ordering::Relaxed),
            probation_retry_started: state.probation_retry_started.load(Ordering::Relaxed),
            client_disconnected,
            payload_limit_observed,
            final_status: outcome.final_status,
            final_error_type: outcome.error_type,
        };
        let diagnostics_json = serde_json::to_string(&snapshot).unwrap_or_else(|error| {
            tracing::warn!(%error, "自动压缩诊断 JSON 序列化失败");
            "{\"schemaVersion\":1,\"serializationError\":true}".to_string()
        });

        if is_high_pressure(&facts) {
            tracing::warn!(
                target: "auto_compact_diagnostics",
                diagnosis,
                session_hash = state.session_hash.as_deref().unwrap_or("none"),
                client_version = state.client_version.as_deref().unwrap_or("unknown"),
                request_body_bytes = state.request_body_bytes,
                upstream_request_max_bytes = facts.upstream_request_max_bytes,
                upstream_context_tokens = facts.upstream_context_tokens,
                upstream_context_percentage = facts.upstream_context_percentage,
                client_reported_tokens = facts.client_reported_tokens,
                message_start_enqueued = facts.message_start_enqueued,
                context_window_exceeded_enqueued = state
                    .context_window_exceeded_enqueued
                    .load(Ordering::Relaxed),
                client_disconnected = facts.client_disconnected,
                payload_limit_observed = facts.payload_limit_observed,
                "自动压缩诊断结论（仅安全计数，不含正文、工具参数或凭证）"
            );
        }

        Some(CompactionTraceData {
            session_hash: state.session_hash.clone(),
            client_version: state.client_version.clone(),
            diagnosis: diagnosis.to_string(),
            request_body_bytes: state.request_body_bytes,
            upstream_context_tokens,
            upstream_context_percentage,
            client_reported_tokens,
            diagnostics_json,
        })
    }
}

pub(crate) fn classify_diagnosis(facts: &DiagnosisFacts) -> &'static str {
    if facts.payload_limit_observed {
        return "payload_limit_preempted";
    }
    let high_context = facts
        .upstream_context_percentage
        .is_some_and(|percentage| percentage >= HIGH_CONTEXT_PERCENTAGE);
    if high_context
        && facts.client_disconnected
        && (!facts.message_start_enqueued || facts.client_reported_tokens.is_none())
    {
        return "client_disconnected_before_signal";
    }
    if high_context && (!facts.message_start_enqueued || facts.client_reported_tokens.is_none()) {
        return "proxy_context_signal_not_exposed";
    }
    if high_context
        && matches!(
            (facts.client_reported_tokens, facts.upstream_context_tokens),
            (Some(client), Some(upstream)) if client.saturating_mul(100) < upstream.saturating_mul(95)
        )
    {
        return "client_usage_signal_incomplete";
    }
    if high_context {
        return "context_signal_enqueued";
    }
    if facts.upstream_context_percentage.is_none()
        && (facts.request_body_bytes >= HIGH_PAYLOAD_BYTES
            || facts.upstream_request_max_bytes >= HIGH_PAYLOAD_BYTES)
    {
        return "upstream_context_unknown";
    }
    "normal"
}

fn is_high_pressure(facts: &DiagnosisFacts) -> bool {
    facts.payload_limit_observed
        || facts
            .upstream_context_percentage
            .is_some_and(|percentage| percentage >= HIGH_CONTEXT_PERCENTAGE)
        || facts.request_body_bytes >= HIGH_PAYLOAD_BYTES
        || facts.upstream_request_max_bytes >= HIGH_PAYLOAD_BYTES
}

pub(crate) fn known_third_party_autocompact_regression_possible(version: Option<&str>) -> bool {
    let Some(version) = version.and_then(parse_version) else {
        return false;
    };
    (2, 1, 161) <= version && version <= (2, 1, 220)
}

fn parse_version(value: &str) -> Option<(u64, u64, u64)> {
    let mut parts = value.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

fn extract_client_version(headers: &HeaderMap) -> Option<String> {
    for name in ["x-stainless-package-version", "x-stainless-client-version"] {
        if let Some(version) = headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .and_then(find_numeric_version)
        {
            return Some(version);
        }
    }
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .and_then(find_numeric_version)
}

fn find_numeric_version(value: &str) -> Option<String> {
    value
        .split(|character: char| !(character.is_ascii_digit() || character == '.'))
        .find(|candidate| parse_version(candidate).is_some())
        .map(str::to_string)
}

fn request_shape(request: &MessagesRequest) -> RequestShape {
    let mut shape = RequestShape {
        message_count: request.messages.len() as u64,
        system_count: request
            .system
            .as_ref()
            .map_or(0, |items| items.len() as u64),
        tool_count: request.tools.as_ref().map_or(0, |items| items.len() as u64),
        system_bytes: request
            .system
            .as_ref()
            .map(|items| items.iter().map(|item| item.text.len() as u64).sum())
            .unwrap_or(0),
        tool_schema_bytes: request
            .tools
            .as_ref()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::to_value(item).ok())
                    .map(|value| json_value_bytes(&value))
                    .sum()
            })
            .unwrap_or(0),
        ..RequestShape::default()
    };

    for message in &request.messages {
        shape.message_bytes += json_value_bytes(&message.content);
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for block in blocks {
            match block.get("type").and_then(serde_json::Value::as_str) {
                Some("image") => {
                    shape.image_count += 1;
                    shape.image_bytes += block
                        .pointer("/source/data")
                        .or_else(|| block.pointer("/source/url"))
                        .map(json_value_bytes)
                        .unwrap_or(0);
                }
                Some("tool_use") => {
                    shape.tool_use_count += 1;
                    shape.tool_use_bytes += block.get("input").map(json_value_bytes).unwrap_or(0);
                }
                Some("tool_result") => {
                    shape.tool_result_count += 1;
                    shape.tool_result_bytes +=
                        block.get("content").map(json_value_bytes).unwrap_or(0);
                }
                _ => {}
            }
        }
    }
    shape
}

fn json_value_bytes(value: &serde_json::Value) -> u64 {
    match value {
        serde_json::Value::Null => 0,
        serde_json::Value::Bool(value) => u64::from(*value),
        serde_json::Value::Number(value) => value.to_string().len() as u64,
        serde_json::Value::String(value) => value.len() as u64,
        serde_json::Value::Array(values) => values.iter().map(json_value_bytes).sum(),
        serde_json::Value::Object(values) => values
            .iter()
            .map(|(key, value)| key.len() as u64 + json_value_bytes(value))
            .sum(),
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
            .is_some_and(|value| !value.is_empty()),
        Some("thinking_delta") => event
            .data
            .pointer("/delta/thinking")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty()),
        _ => false,
    }
}

fn optional_atomic(value: &AtomicU64) -> Option<u64> {
    let value = value.load(Ordering::Relaxed);
    (value != UNKNOWN_U64).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::{
        CompactionDiagnostics, CompactionFinalize, DiagnosisFacts, classify_diagnosis,
        known_third_party_autocompact_regression_possible,
    };
    use crate::anthropic::types::MessagesRequest;
    use axum::http::{HeaderMap, HeaderValue, header};
    use serde_json::json;

    fn request() -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "max_tokens": 1024,
            "stream": true,
            "metadata": {"user_id": "secret-session-id"},
            "system": [{"type": "text", "text": "secret-system-text"}],
            "tools": [{
                "name": "secret_tool_name",
                "description": "secret-tool-description",
                "input_schema": {"type": "object", "properties": {"token": {"type": "string"}}}
            }],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "secret-user-text"}]},
                {"role": "assistant", "content": [{"type": "tool_use", "id": "tool-1", "name": "secret_tool_name", "input": {"token": "secret-token-value"}}]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool-1", "content": "secret-result-text"}]},
                {"role": "user", "content": [{"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "c2VjcmV0LWltYWdl"}}]}
            ]
        }))
        .unwrap()
    }

    fn finalize<'a>() -> CompactionFinalize<'a> {
        CompactionFinalize {
            final_status: "success",
            error_type: None,
            error_message: None,
            is_stream: true,
            usage_input_tokens: 0,
        }
    }

    #[test]
    fn disabled_diagnostics_short_circuit_without_snapshot() {
        let diagnostics = CompactionDiagnostics::new(false, &HeaderMap::new(), &request());
        assert!(!diagnostics.is_enabled());
        assert!(diagnostics.finalize(finalize()).is_none());
    }

    #[test]
    fn snapshot_contains_only_hashes_counts_and_safe_version() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from_static("3141592"));
        headers.insert(
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.220 (secret-extra-value)"),
        );
        let diagnostics = CompactionDiagnostics::new(true, &headers, &request());
        let snapshot = diagnostics.finalize(finalize()).unwrap();

        assert_eq!(snapshot.request_body_bytes, 3_141_592);
        assert_eq!(snapshot.client_version.as_deref(), Some("2.1.220"));
        assert_eq!(snapshot.session_hash.as_deref().map(str::len), Some(64));
        let json = snapshot.diagnostics_json;
        for secret in [
            "secret-session-id",
            "secret-system-text",
            "secret-user-text",
            "secret_tool_name",
            "secret-token-value",
            "secret-result-text",
            "secret-extra-value",
            "c2VjcmV0LWltYWdl",
        ] {
            assert!(!json.contains(secret), "diagnostic JSON leaked {secret}");
        }
        assert!(json.contains("\"messageCount\":4"));
        assert!(json.contains("\"toolUseCount\":1"));
        assert!(json.contains("\"toolResultCount\":1"));
        assert!(json.contains("\"imageCount\":1"));
    }

    #[test]
    fn non_stream_finalize_preserves_usage_already_observed_on_the_client_response() {
        let diagnostics = CompactionDiagnostics::new(true, &HeaderMap::new(), &request());
        diagnostics.observe_non_stream_client_usage(1_000);

        let snapshot = diagnostics
            .finalize(CompactionFinalize {
                final_status: "success",
                error_type: None,
                error_message: None,
                is_stream: false,
                usage_input_tokens: 0,
            })
            .unwrap();

        assert_eq!(snapshot.client_reported_tokens, Some(1_000));
    }

    #[test]
    fn snapshot_records_whether_context_window_exceeded_reached_the_client() {
        let diagnostics = CompactionDiagnostics::new(true, &HeaderMap::new(), &request());
        diagnostics.observe_client_event_enqueued(&crate::anthropic::stream::SseEvent::new(
            "message_delta",
            json!({
                "type": "message_delta",
                "delta": {"stop_reason": "model_context_window_exceeded"},
                "usage": {"output_tokens": 1}
            }),
        ));

        let snapshot = diagnostics.finalize(finalize()).unwrap();
        assert!(
            snapshot
                .diagnostics_json
                .contains("\"contextWindowExceededEnqueued\":true")
        );
    }

    #[test]
    fn known_client_regression_range_is_inclusive() {
        assert!(!known_third_party_autocompact_regression_possible(Some(
            "2.1.160"
        )));
        assert!(known_third_party_autocompact_regression_possible(Some(
            "2.1.161"
        )));
        assert!(known_third_party_autocompact_regression_possible(Some(
            "2.1.220"
        )));
        assert!(!known_third_party_autocompact_regression_possible(Some(
            "2.1.221"
        )));
        assert!(!known_third_party_autocompact_regression_possible(Some(
            "invalid"
        )));
    }

    #[test]
    fn diagnosis_priority_distinguishes_signal_failures() {
        let base = DiagnosisFacts {
            request_body_bytes: 2_600_000,
            upstream_request_max_bytes: 2_700_000,
            upstream_context_tokens: Some(900_000),
            upstream_context_percentage: Some(90.0),
            client_reported_tokens: Some(900_000),
            message_start_enqueued: true,
            client_disconnected: false,
            payload_limit_observed: false,
        };

        assert_eq!(classify_diagnosis(&base), "context_signal_enqueued");
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                payload_limit_observed: true,
                ..base
            }),
            "payload_limit_preempted"
        );
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                client_disconnected: true,
                message_start_enqueued: false,
                client_reported_tokens: None,
                ..base
            }),
            "client_disconnected_before_signal"
        );
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                message_start_enqueued: false,
                client_reported_tokens: None,
                ..base
            }),
            "proxy_context_signal_not_exposed"
        );
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                client_reported_tokens: Some(100_000),
                ..base
            }),
            "client_usage_signal_incomplete"
        );
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                upstream_context_tokens: None,
                upstream_context_percentage: None,
                client_reported_tokens: None,
                message_start_enqueued: false,
                ..base
            }),
            "upstream_context_unknown"
        );
        assert_eq!(
            classify_diagnosis(&DiagnosisFacts {
                request_body_bytes: 10_000,
                upstream_request_max_bytes: 20_000,
                upstream_context_tokens: Some(10_000),
                upstream_context_percentage: Some(5.0),
                client_reported_tokens: Some(10_000),
                ..base
            }),
            "normal"
        );
    }
}
