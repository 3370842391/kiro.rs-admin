use std::io::Read as _;
use std::sync::atomic::{AtomicBool, Ordering};

use axum::http::HeaderMap;
use base64::Engine as _;
use parking_lot::Mutex;
use sha2::{Digest as _, Sha256};

use crate::admin::error_snapshot_db::{
    CaptureMode, InsertOutcome, SharedErrorSnapshotStore, SnapshotSeverity, SnapshotWrite,
};
use crate::admin::trace_db::TraceKeySource;

pub use crate::common::error_snapshot::{EncodedPayloadPart, SnapshotPayloadKind};

pub const MAX_UNCOMPRESSED_PART_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_DECOMPRESSED_PAYLOAD_BYTES: usize = 128 * 1024 * 1024;
const LONG_BASE64_THRESHOLD: usize = 4096;
const ZSTD_LEVEL: i32 = 3;

pub fn sanitize_json(mut value: serde_json::Value) -> serde_json::Value {
    sanitize_value(&mut value);
    value
}

fn sanitize_value(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(object) => {
            let parent_is_base64 = object
                .get("type")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|kind| kind.eq_ignore_ascii_case("base64"));
            for (name, child) in object.iter_mut() {
                if is_secret_field(name) {
                    *child = serde_json::Value::String("[REDACTED]".to_string());
                    continue;
                }
                if let Some(text) = child.as_str()
                    && let Some(decoded) = binary_bytes(name, text, parent_is_base64)
                {
                    *child = binary_digest(&decoded);
                    continue;
                }
                sanitize_value(child);
            }
        }
        serde_json::Value::Array(values) => {
            for child in values {
                sanitize_value(child);
            }
        }
        _ => {}
    }
}

fn is_secret_field(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().replace(['-', '_'], "").as_str(),
        "authorization"
            | "proxyauthorization"
            | "xapikey"
            | "apikey"
            | "adminapikey"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "clientsecret"
            | "cookie"
            | "setcookie"
            | "password"
            | "credential"
            | "credentials"
            | "secret"
    )
}

fn binary_bytes(name: &str, text: &str, parent_is_base64: bool) -> Option<Vec<u8>> {
    if parent_is_base64 && name.eq_ignore_ascii_case("data") {
        return base64::engine::general_purpose::STANDARD.decode(text).ok();
    }
    if text.starts_with("data:")
        && let Some((metadata, encoded)) = text.split_once(',')
        && metadata.to_ascii_lowercase().ends_with(";base64")
    {
        return base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .ok();
    }
    if text.len() >= LONG_BASE64_THRESHOLD && text.len().is_multiple_of(4) {
        return base64::engine::general_purpose::STANDARD.decode(text).ok();
    }
    None
}

fn binary_digest(decoded: &[u8]) -> serde_json::Value {
    serde_json::json!({
        "redacted_base64": true,
        "original_bytes": decoded.len(),
        "sha256": hex::encode(Sha256::digest(decoded)),
    })
}

pub fn split_utf8(input: &[u8], limit: usize) -> Vec<Vec<u8>> {
    if input.is_empty() {
        return vec![Vec::new()];
    }
    let limit = limit.max(1);
    let is_utf8 = std::str::from_utf8(input).is_ok();
    let mut parts = Vec::new();
    let mut start = 0;
    while start < input.len() {
        let mut end = (start + limit).min(input.len());
        if is_utf8 && end < input.len() {
            while end > start && std::str::from_utf8(&input[start..end]).is_err() {
                end -= 1;
            }
            if end == start {
                end = (start + limit).min(input.len());
                while end < input.len() && !is_utf8_boundary(input[end]) {
                    end += 1;
                }
            }
        }
        parts.push(input[start..end].to_vec());
        start = end;
    }
    parts
}

fn is_utf8_boundary(byte: u8) -> bool {
    byte as i8 >= -0x40
}

pub fn encode_payload(
    kind: SnapshotPayloadKind,
    attempt: Option<u32>,
    content_type: &str,
    input: &[u8],
) -> anyhow::Result<Vec<EncodedPayloadPart>> {
    let chunks = split_utf8(input, MAX_UNCOMPRESSED_PART_BYTES);
    let part_count = u32::try_from(chunks.len())?;
    let sha256 = hex::encode(Sha256::digest(input));
    chunks
        .into_iter()
        .enumerate()
        .map(|(index, chunk)| {
            let data = zstd::stream::encode_all(chunk.as_slice(), ZSTD_LEVEL)?;
            Ok(EncodedPayloadPart {
                seq: u32::try_from(index)?,
                kind,
                attempt,
                codec: "zstd".to_string(),
                content_type: content_type.to_string(),
                part_index: u32::try_from(index)?,
                part_count,
                original_bytes: u64::try_from(chunk.len())?,
                sha256: sha256.clone(),
                data,
            })
        })
        .collect()
}

pub fn decode_payload_parts(
    parts: &[EncodedPayloadPart],
    max_output: usize,
) -> anyhow::Result<Vec<u8>> {
    if parts.is_empty() {
        return Ok(Vec::new());
    }
    let limit = max_output.min(MAX_DECOMPRESSED_PAYLOAD_BYTES);
    let expected_count = usize::try_from(parts[0].part_count)?;
    if expected_count != parts.len() {
        anyhow::bail!("快照分片数量不一致");
    }
    let declared_total = parts.iter().try_fold(0usize, |total, part| {
        let size = usize::try_from(part.original_bytes)?;
        if size > MAX_UNCOMPRESSED_PART_BYTES {
            anyhow::bail!("快照分片超过解压上限");
        }
        total
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("快照解压上限溢出"))
    })?;
    if declared_total > limit {
        anyhow::bail!("快照超过解压上限");
    }

    let mut ordered = parts.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|part| part.part_index);
    let expected_sha = &parts[0].sha256;
    let mut output = Vec::with_capacity(declared_total);
    for (expected_index, part) in ordered.into_iter().enumerate() {
        if part.codec != "zstd"
            || usize::try_from(part.part_index)? != expected_index
            || part.sha256 != *expected_sha
            || usize::try_from(part.part_count)? != expected_count
        {
            anyhow::bail!("快照分片元数据不一致");
        }
        let declared = usize::try_from(part.original_bytes)?;
        let decoder = zstd::stream::read::Decoder::new(part.data.as_slice())?;
        let mut decoded = Vec::with_capacity(declared);
        decoder
            .take(u64::try_from(declared)? + 1)
            .read_to_end(&mut decoded)?;
        if decoded.len() != declared {
            anyhow::bail!("快照分片解压长度与声明不一致或超过解压上限");
        }
        output.extend_from_slice(&decoded);
    }
    if output.len() > limit {
        anyhow::bail!("快照超过解压上限");
    }
    let actual_sha = hex::encode(Sha256::digest(&output));
    if &actual_sha != expected_sha {
        anyhow::bail!("快照哈希校验失败");
    }
    Ok(output)
}

const STREAM_TAIL_MAX_BYTES: usize = 256 * 1024;

pub struct ErrorSnapshotContext {
    store: SharedErrorSnapshotStore,
    trace_id: String,
    snapshot_id: String,
    key_id: u64,
    key_source: TraceKeySource,
    is_stream: bool,
    ts: String,
    ts_epoch: i64,
    draft: Mutex<SnapshotDraft>,
    finalized: AtomicBool,
}

struct SnapshotDraft {
    headers: serde_json::Value,
    client_request: serde_json::Value,
    payloads: Vec<RawSnapshotPayload>,
    attempts: Vec<AttemptObservation>,
    upstream_diagnostics: Vec<UpstreamDiagnosticObservation>,
    protocol_errors: Vec<(String, String)>,
    stream_tail: StreamTail,
    final_credential_id: u64,
    model: String,
    endpoint: Option<String>,
    tool_diagnostics: ToolDiagnostics,
    /// Schema 失败只允许持久化键名/类型/违规项摘要，禁止落盘客户请求、工具值和流尾。
    tool_schema_safe_only: bool,
}

#[derive(Clone)]
struct RawSnapshotPayload {
    kind: SnapshotPayloadKind,
    attempt: Option<u32>,
    content_type: String,
    data: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize)]
struct AttemptObservation {
    attempt: u32,
    http_status: Option<u16>,
    outcome: String,
}

#[derive(Debug, Clone, serde::Serialize)]
struct UpstreamDiagnosticObservation {
    event: String,
    attempt: u32,
    credential_id: u64,
    endpoint: String,
    http_status: Option<u16>,
    message: Option<String>,
}

#[derive(Default)]
struct StreamTail {
    bytes: Vec<u8>,
}

impl StreamTail {
    fn push(&mut self, chunk: &[u8]) {
        self.bytes.extend_from_slice(chunk);
        let excess = self.bytes.len().saturating_sub(STREAM_TAIL_MAX_BYTES);
        if excess == 0 {
            return;
        }
        let mut start = excess;
        while start < self.bytes.len() && std::str::from_utf8(&self.bytes[start..]).is_err() {
            start += 1;
        }
        if start < self.bytes.len() {
            self.bytes.drain(..start);
        } else {
            self.bytes.drain(..excess);
        }
    }

    fn snapshot_bytes(&self) -> Vec<u8> {
        if std::str::from_utf8(&self.bytes).is_ok() {
            return self.bytes.clone();
        }
        serde_json::to_vec(&serde_json::json!({
            "invalid_utf8": true,
            "original_bytes": self.bytes.len(),
            "sha256": hex::encode(Sha256::digest(&self.bytes)),
        }))
        .unwrap_or_default()
    }
}

pub struct SnapshotFinalState {
    pub final_status: String,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub http_status: Option<u16>,
    pub interrupted_after_bytes: Option<u64>,
}

impl SnapshotFinalState {
    pub fn success() -> Self {
        Self {
            final_status: "success".to_string(),
            error_type: None,
            error_message: None,
            http_status: Some(200),
            interrupted_after_bytes: None,
        }
    }

    pub fn error(error_type: &str, http_status: Option<u16>) -> Self {
        Self {
            final_status: "error".to_string(),
            error_type: Some(error_type.to_string()),
            error_message: None,
            http_status,
            interrupted_after_bytes: None,
        }
    }

    pub fn interrupted(error_type: &str, sent_bytes: u64) -> Self {
        Self {
            final_status: "interrupted".to_string(),
            error_type: Some(error_type.to_string()),
            error_message: None,
            http_status: None,
            interrupted_after_bytes: Some(sent_bytes),
        }
    }
}

impl ErrorSnapshotContext {
    pub fn new_if_enabled(
        store: SharedErrorSnapshotStore,
        trace_id: String,
        key: &super::middleware::KeyContext,
        headers: &HeaderMap,
        request: &super::types::MessagesRequest,
    ) -> Option<Self> {
        store
            .policy()
            .enabled
            .then(|| Self::new(store, trace_id, key, headers, request))
    }

    pub fn new(
        store: SharedErrorSnapshotStore,
        trace_id: String,
        key: &super::middleware::KeyContext,
        headers: &HeaderMap,
        request: &super::types::MessagesRequest,
    ) -> Self {
        let now = chrono::Utc::now();
        let headers = sanitize_headers(headers);
        let client_request = serde_json::to_value(request).unwrap_or_else(|error| {
            serde_json::json!({
                "serialization_error": error.to_string(),
                "model": request.model,
                "message_count": request.messages.len(),
            })
        });
        let tool_diagnostics = analyze_tool_links(request);
        Self {
            store,
            trace_id,
            snapshot_id: format!("snap_{}", uuid::Uuid::new_v4().simple()),
            key_id: key.key_id,
            key_source: key.key_source,
            is_stream: request.stream,
            ts: now.to_rfc3339(),
            ts_epoch: now.timestamp(),
            draft: Mutex::new(SnapshotDraft {
                headers,
                client_request,
                payloads: Vec::new(),
                attempts: Vec::new(),
                upstream_diagnostics: Vec::new(),
                protocol_errors: Vec::new(),
                stream_tail: StreamTail::default(),
                final_credential_id: 0,
                model: request.model.clone(),
                endpoint: None,
                tool_diagnostics,
                tool_schema_safe_only: false,
            }),
            finalized: AtomicBool::new(false),
        }
    }

    pub fn record_kiro_request(
        &self,
        attempt: u32,
        credential_id: u64,
        endpoint: &str,
        body: &str,
    ) {
        let mut draft = self.draft.lock();
        draft.final_credential_id = credential_id;
        draft.endpoint = Some(endpoint.to_string());
        draft
            .upstream_diagnostics
            .push(UpstreamDiagnosticObservation {
                event: "request".to_string(),
                attempt,
                credential_id,
                endpoint: endpoint.to_string(),
                http_status: None,
                message: None,
            });
        draft.payloads.push(RawSnapshotPayload {
            kind: SnapshotPayloadKind::KiroRequest,
            attempt: Some(attempt),
            content_type: "application/json".to_string(),
            data: body.as_bytes().to_vec(),
        });
    }

    pub fn record_upstream_response(
        &self,
        attempt: u32,
        credential_id: u64,
        endpoint: &str,
        status: u16,
        body: &str,
    ) {
        let mut draft = self.draft.lock();
        draft.final_credential_id = credential_id;
        draft.endpoint = Some(endpoint.to_string());
        draft
            .upstream_diagnostics
            .push(UpstreamDiagnosticObservation {
                event: "response".to_string(),
                attempt,
                credential_id,
                endpoint: endpoint.to_string(),
                http_status: Some(status),
                message: body
                    .is_empty()
                    .then(|| "response body was not consumed".to_string()),
            });
        draft.payloads.push(RawSnapshotPayload {
            kind: SnapshotPayloadKind::UpstreamResponse,
            attempt: Some(attempt),
            content_type: "application/json".to_string(),
            data: body.as_bytes().to_vec(),
        });
    }

    pub fn record_upstream_body(&self, attempt: u32, body: &[u8]) {
        self.draft.lock().payloads.push(RawSnapshotPayload {
            kind: SnapshotPayloadKind::UpstreamResponse,
            attempt: Some(attempt),
            content_type: "application/octet-stream".to_string(),
            data: body.to_vec(),
        });
    }

    pub fn record_network_error(
        &self,
        attempt: u32,
        credential_id: u64,
        endpoint: &str,
        message: &str,
    ) {
        let mut draft = self.draft.lock();
        draft.final_credential_id = credential_id;
        draft.endpoint = Some(endpoint.to_string());
        draft
            .upstream_diagnostics
            .push(UpstreamDiagnosticObservation {
                event: "network_error".to_string(),
                attempt,
                credential_id,
                endpoint: endpoint.to_string(),
                http_status: None,
                message: Some(message.to_string()),
            });
        draft.protocol_errors.push((
            "network_error".to_string(),
            format!("attempt {attempt}: {message}"),
        ));
    }

    pub fn record_internal_error(&self, error_type: &str, message: &str) {
        self.draft
            .lock()
            .protocol_errors
            .push((error_type.to_string(), message.to_string()));
    }

    /// 将本次快照降级为工具 Schema 安全摘要模式。
    ///
    /// 失败发生前可能已经采集了 Kiro 请求、响应或流尾，因此这里同时清理已采集内容；
    /// finalize 还会再次按标志过滤，避免重试后的新 chunk 被落盘。
    pub fn record_tool_schema_failure(&self, safe_summary: &str) {
        let mut draft = self.draft.lock();
        draft.tool_schema_safe_only = true;
        draft.payloads.retain(|payload| {
            !matches!(
                payload.kind,
                SnapshotPayloadKind::KiroRequest | SnapshotPayloadKind::UpstreamResponse
            )
        });
        draft.stream_tail.bytes.clear();
        draft.protocol_errors.push((
            "upstream_tool_schema_error".to_string(),
            safe_summary.to_string(),
        ));
    }

    pub fn record_stream_chunk(&self, chunk: &[u8]) {
        let mut draft = self.draft.lock();
        draft.stream_tail.push(chunk);
    }

    pub fn record_attempt_status(&self, attempt: u32, status: Option<u16>, outcome: &str) {
        self.draft.lock().attempts.push(AttemptObservation {
            attempt,
            http_status: status,
            outcome: outcome.to_string(),
        });
    }

    pub fn finalize(&self, state: SnapshotFinalState) -> anyhow::Result<Option<String>> {
        if self.finalized.swap(true, Ordering::AcqRel) {
            return Ok(None);
        }
        let policy = self.store.policy();
        if !policy.enabled {
            return Ok(None);
        }
        let draft = self.draft.lock();
        let failed_attempt = draft.attempts.iter().any(|attempt| {
            attempt
                .http_status
                .is_none_or(|status| !(200..400).contains(&status))
                || attempt.outcome != "success"
        });
        let has_internal_error = !draft.protocol_errors.is_empty();
        let final_success = state.final_status == "success";
        let recovered = final_success && (failed_attempt || has_internal_error);
        if final_success && !recovered {
            return Ok(None);
        }
        if recovered && !policy.capture_recovered {
            return Ok(None);
        }
        let critical_protocol_error = draft.protocol_errors.iter().rev().find(|(error_type, _)| {
            classify_severity(error_type, false) == SnapshotSeverity::Critical
        });
        let error_type = critical_protocol_error
            .map(|entry| entry.0.clone())
            .or(state.error_type.clone())
            .or_else(|| draft.protocol_errors.last().map(|entry| entry.0.clone()))
            .or_else(|| {
                draft
                    .attempts
                    .iter()
                    .find(|attempt| attempt.outcome != "success")
                    .map(|attempt| attempt.outcome.clone())
            })
            .unwrap_or_else(|| "recovered_request".to_string());
        let severity = classify_severity(&error_type, recovered);
        let retention_exempt = severity == SnapshotSeverity::Critical;
        let metadata_only = self.store.capture_mode() == CaptureMode::MetadataOnly;
        let tool_schema_safe_only = draft.tool_schema_safe_only;
        let include_bodies = policy.capture_bodies && !metadata_only && !tool_schema_safe_only;
        let mut raw_payloads = Vec::new();
        if include_bodies {
            raw_payloads.push(RawSnapshotPayload {
                kind: SnapshotPayloadKind::ClientRequest,
                attempt: None,
                content_type: "application/json".to_string(),
                data: serde_json::to_vec(&sanitize_json(serde_json::json!({
                    "headers": draft.headers,
                    "request": draft.client_request,
                })))?,
            });
        }
        let request_bytes = serde_json::to_vec(&draft.client_request)?;
        raw_payloads.push(RawSnapshotPayload {
            kind: SnapshotPayloadKind::ToolDiagnostics,
            attempt: None,
            content_type: "application/json".to_string(),
            data: serde_json::to_vec(&serde_json::json!({
                "request_bytes": request_bytes.len(),
                "request_sha256": hex::encode(Sha256::digest(&request_bytes)),
                "tool_links": draft.tool_diagnostics,
                "attempts": draft.attempts,
                "upstream_diagnostics": draft.upstream_diagnostics,
                "interrupted_after_bytes": state.interrupted_after_bytes,
            }))?,
        });
        for payload in &draft.payloads {
            if tool_schema_safe_only
                && matches!(
                    payload.kind,
                    SnapshotPayloadKind::KiroRequest | SnapshotPayloadKind::UpstreamResponse
                )
            {
                continue;
            }
            if include_bodies || payload.kind != SnapshotPayloadKind::KiroRequest {
                raw_payloads.push(payload.clone());
            }
        }
        if !draft.protocol_errors.is_empty() {
            raw_payloads.push(RawSnapshotPayload {
                kind: SnapshotPayloadKind::InternalError,
                attempt: None,
                content_type: "application/json".to_string(),
                data: serde_json::to_vec(&draft.protocol_errors)?,
            });
        }
        if !tool_schema_safe_only && !draft.stream_tail.bytes.is_empty() {
            raw_payloads.push(RawSnapshotPayload {
                kind: SnapshotPayloadKind::StreamTail,
                attempt: None,
                content_type: "application/octet-stream".to_string(),
                data: draft.stream_tail.snapshot_bytes(),
            });
        }
        let mut payloads = Vec::new();
        for (seq, raw) in raw_payloads.into_iter().enumerate() {
            let data = sanitize_payload_data(&raw.content_type, raw.data);
            let mut encoded = encode_payload(raw.kind, raw.attempt, &raw.content_type, &data)?;
            for part in &mut encoded {
                part.seq = u32::try_from(seq)?;
            }
            payloads.extend(encoded);
        }
        let error_message = critical_protocol_error
            .map(|entry| entry.1.clone())
            .or(state.error_message)
            .or_else(|| draft.protocol_errors.last().map(|entry| entry.1.clone()));
        let http_status = state.http_status.or_else(|| {
            draft
                .attempts
                .iter()
                .rev()
                .find_map(|attempt| attempt.http_status)
        });
        let write = SnapshotWrite {
            snapshot_id: self.snapshot_id.clone(),
            trace_id: self.trace_id.clone(),
            ts: self.ts.clone(),
            ts_epoch: self.ts_epoch,
            model: draft.model.clone(),
            is_stream: self.is_stream,
            key_id: self.key_id,
            key_source: self.key_source,
            final_credential_id: draft.final_credential_id,
            endpoint: draft.endpoint.clone(),
            http_status,
            final_status: state.final_status,
            error_type,
            severity,
            error_message,
            recovered,
            pinned: false,
            retention_exempt,
            omitted_due_to_disk_pressure: metadata_only,
            payloads,
        };
        let id = match self.store.insert_with_fallback(&write)? {
            InsertOutcome::Inserted(id)
            | InsertOutcome::Existing(id)
            | InsertOutcome::Fallback(id) => id,
        };
        Ok(Some(id))
    }
}

fn sanitize_payload_data(content_type: &str, data: Vec<u8>) -> Vec<u8> {
    if content_type.contains("json")
        && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&data)
        && let Ok(encoded) = serde_json::to_vec(&sanitize_json(value))
    {
        return encoded;
    }
    data
}

fn sanitize_headers(headers: &HeaderMap) -> serde_json::Value {
    let mut output = serde_json::Map::new();
    for (name, value) in headers {
        let value = value.to_str().map(str::to_owned).unwrap_or_else(|_| {
            let bytes = value.as_bytes();
            format!(
                "[NON_UTF8 length={} sha256={}]",
                bytes.len(),
                hex::encode(Sha256::digest(bytes))
            )
        });
        output.insert(name.to_string(), serde_json::Value::String(value));
    }
    sanitize_json(serde_json::Value::Object(output))
}

fn classify_severity(error_type: &str, recovered: bool) -> SnapshotSeverity {
    if matches!(
        error_type,
        "tool_use_truncated"
            | "tool_result_mismatch"
            | "upstream_tool_protocol_error"
            | "upstream_thinking_protocol_error"
            | "sse_state_error"
            | "utf8_decode_error"
            | "snapshot_integrity_error"
    ) {
        SnapshotSeverity::Critical
    } else if recovered {
        SnapshotSeverity::Warning
    } else {
        SnapshotSeverity::Error
    }
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub(crate) struct ToolDiagnostics {
    pub invalid_ids: Vec<String>,
    pub duplicate_tool_use_ids: Vec<String>,
    pub unmatched_tool_results: Vec<String>,
    pub missing_tool_results: Vec<String>,
    pub block_order: Vec<serde_json::Value>,
}

pub(crate) fn analyze_tool_links(request: &super::types::MessagesRequest) -> ToolDiagnostics {
    let mut diagnostics = ToolDiagnostics::default();
    let mut tool_uses = std::collections::HashSet::new();
    let mut tool_results = std::collections::HashSet::new();
    let mut duplicates = std::collections::HashSet::new();
    for (message_index, message) in request.messages.iter().enumerate() {
        let Some(blocks) = message.content.as_array() else {
            continue;
        };
        for (block_index, block) in blocks.iter().enumerate() {
            match (
                message.role.as_str(),
                block.get("type").and_then(serde_json::Value::as_str),
            ) {
                ("assistant", Some("tool_use")) => {
                    let id = block
                        .get("id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if !valid_tool_id(&id) {
                        diagnostics.invalid_ids.push(id.clone());
                    }
                    if !tool_uses.insert(id.clone()) && duplicates.insert(id.clone()) {
                        diagnostics.duplicate_tool_use_ids.push(id.clone());
                    }
                    diagnostics.block_order.push(serde_json::json!({
                        "message": message_index,
                        "block": block_index,
                        "role": message.role,
                        "type": "tool_use",
                        "id": id,
                        "name": block.get("name").and_then(serde_json::Value::as_str),
                    }));
                }
                ("user", Some("tool_result")) => {
                    let id = block
                        .get("tool_use_id")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    tool_results.insert(id.clone());
                    diagnostics.block_order.push(serde_json::json!({
                        "message": message_index,
                        "block": block_index,
                        "role": message.role,
                        "type": "tool_result",
                        "tool_use_id": id,
                    }));
                }
                _ => {}
            }
        }
    }
    diagnostics.unmatched_tool_results = tool_results.difference(&tool_uses).cloned().collect();
    diagnostics.missing_tool_results = tool_uses.difference(&tool_results).cloned().collect();
    diagnostics.invalid_ids.sort();
    diagnostics.duplicate_tool_use_ids.sort();
    diagnostics.unmatched_tool_results.sort();
    diagnostics.missing_tool_results.sort();
    diagnostics
}

fn valid_tool_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_store() -> crate::admin::error_snapshot_db::SharedErrorSnapshotStore {
        std::sync::Arc::new(
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
        )
    }

    fn sample_context(
        store: crate::admin::error_snapshot_db::SharedErrorSnapshotStore,
        capture_recovered: bool,
        capture_bodies: bool,
    ) -> ErrorSnapshotContext {
        let mut policy = store.policy();
        policy.capture_recovered = capture_recovered;
        policy.capture_bodies = capture_bodies;
        store.set_policy(policy);
        let key = crate::anthropic::middleware::KeyContext {
            key_id: 7,
            group: None,
            key_source: crate::admin::trace_db::TraceKeySource::ClientKey,
        };
        let request: crate::anthropic::types::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hello"}],
                "stream": false
            }))
            .unwrap();
        ErrorSnapshotContext::new(
            store,
            "trace-test".to_string(),
            &key,
            &axum::http::HeaderMap::new(),
            &request,
        )
    }

    #[test]
    fn redacts_auth_fields_but_preserves_customer_text_and_tool_json() {
        let value = serde_json::json!({
            "headers": {"Authorization": "Bearer secret", "anthropic-version": "2023-06-01"},
            "refreshToken": "refresh-secret",
            "messages": [{"role": "user", "content": "explain token and key rotation"}],
            "tool": {"name": "lookup", "input": {"key": "ordinary-business-key"}}
        });
        let sanitized = sanitize_json(value);
        assert_eq!(sanitized["headers"]["Authorization"], "[REDACTED]");
        assert_eq!(sanitized["refreshToken"], "[REDACTED]");
        assert_eq!(
            sanitized["messages"][0]["content"],
            "explain token and key rotation"
        );
        assert_eq!(sanitized["tool"]["input"]["key"], "ordinary-business-key");
    }

    #[test]
    fn replaces_known_binary_and_long_strict_base64_with_digest() {
        let raw = vec![0x5a; 8192];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let sanitized = sanitize_json(serde_json::json!({
            "source": {"type": "base64", "media_type": "application/pdf", "data": encoded},
            "shortToolValue": "YWJj"
        }));
        assert_eq!(sanitized["source"]["data"]["redacted_base64"], true);
        assert_eq!(sanitized["source"]["data"]["original_bytes"], 8192);
        assert_eq!(
            sanitized["source"]["data"]["sha256"]
                .as_str()
                .unwrap()
                .len(),
            64
        );
        assert_eq!(sanitized["shortToolValue"], "YWJj");
    }

    #[test]
    fn replaces_large_data_uri_even_when_the_whole_uri_length_is_base64_aligned() {
        let raw = vec![0x33; 4096];
        let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
        let data_uri = format!("data:abc;base64,{encoded}");
        assert!(data_uri.len().is_multiple_of(4));

        let sanitized = sanitize_json(serde_json::json!({"image": data_uri}));

        assert_eq!(sanitized["image"]["redacted_base64"], true);
        assert_eq!(sanitized["image"]["original_bytes"], 4096);
    }

    #[test]
    fn chunks_utf8_without_cutting_characters_and_round_trips_zstd() {
        let input = "错误现场-".repeat(2_000_000);
        let chunks = split_utf8(input.as_bytes(), MAX_UNCOMPRESSED_PART_BYTES);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|part| std::str::from_utf8(part).is_ok()));
        let rebuilt = chunks.concat();
        assert_eq!(rebuilt, input.as_bytes());

        let encoded = encode_payload(
            SnapshotPayloadKind::ClientRequest,
            None,
            "application/json",
            input.as_bytes(),
        )
        .unwrap();
        let decoded = decode_payload_parts(&encoded, input.len()).unwrap();
        assert_eq!(decoded, input.as_bytes());
    }

    #[test]
    fn rejects_decompression_larger_than_declared_limit() {
        let input = vec![b'x'; 1024];
        let encoded = encode_payload(
            SnapshotPayloadKind::InternalError,
            None,
            "text/plain",
            &input,
        )
        .unwrap();
        let error = decode_payload_parts(&encoded, 128).unwrap_err();
        assert!(error.to_string().contains("解压上限"));
    }

    #[test]
    fn pure_success_does_not_create_snapshot() {
        let ctx = sample_context(test_store(), true, true);
        ctx.record_attempt_status(0, Some(200), "success");
        assert_eq!(ctx.finalize(SnapshotFinalState::success()).unwrap(), None);
    }

    #[test]
    fn failed_request_creates_error_snapshot() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        ctx.record_internal_error("upstream_tool_protocol_error", "tool JSON truncated");
        let id = ctx
            .finalize(SnapshotFinalState::error(
                "upstream_tool_protocol_error",
                Some(502),
            ))
            .unwrap()
            .unwrap();
        assert!(store.get(&id).unwrap().is_some());
    }

    #[test]
    fn tool_schema_failure_snapshot_keeps_only_safe_summary() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        ctx.record_kiro_request(
            0,
            7,
            "https://example.invalid/generate",
            r#"{"conversationState":{"currentMessage":{"userInputMessage":{"content":"private customer prompt"}}}}"#,
        );
        ctx.record_upstream_body(
            0,
            br#"{"tool":"get_weather","input":{"city":"private customer value"}}"#,
        );
        ctx.record_stream_chunk(b"private streamed customer value");

        ctx.record_tool_schema_failure(
            r#"{"attempt":1,"tool":"get_weather","input":{"keys":["city"],"types":{"city":"string"}},"violations":["missing required $.unit"]}"#,
        );

        let id = ctx
            .finalize(SnapshotFinalState::error(
                "upstream_tool_schema_error",
                Some(502),
            ))
            .unwrap()
            .unwrap();
        let detail = store.get(&id).unwrap().unwrap();
        let kinds = detail
            .payloads
            .iter()
            .map(|payload| payload.kind)
            .collect::<Vec<_>>();

        assert!(!kinds.contains(&SnapshotPayloadKind::ClientRequest));
        assert!(!kinds.contains(&SnapshotPayloadKind::KiroRequest));
        assert!(!kinds.contains(&SnapshotPayloadKind::UpstreamResponse));
        assert!(!kinds.contains(&SnapshotPayloadKind::StreamTail));

        let internal = detail
            .payloads
            .iter()
            .find(|payload| payload.kind == SnapshotPayloadKind::InternalError)
            .expect("safe schema summary payload");
        let payload = store.read_payload(&id, internal.seq).unwrap().unwrap();
        let text = String::from_utf8(payload.data).unwrap();
        assert!(text.contains("get_weather"));
        assert!(text.contains("city"));
        assert!(!text.contains("private customer"));
    }

    #[test]
    fn recovered_request_is_warning_when_capture_recovered_is_enabled() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        ctx.record_attempt_status(0, Some(500), "transient");
        ctx.record_attempt_status(1, Some(200), "success");
        let id = ctx
            .finalize(SnapshotFinalState::success())
            .unwrap()
            .unwrap();
        let detail = store.get(&id).unwrap().unwrap();
        assert!(detail.summary.recovered);
        assert_eq!(
            detail.summary.severity,
            crate::admin::error_snapshot_db::SnapshotSeverity::Warning
        );
    }

    #[test]
    fn critical_protocol_error_is_retention_exempt() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        ctx.record_internal_error("tool_use_truncated", "incomplete JSON");
        let id = ctx
            .finalize(SnapshotFinalState::error("tool_use_truncated", Some(502)))
            .unwrap()
            .unwrap();
        let detail = store.get(&id).unwrap().unwrap();
        assert_eq!(
            detail.summary.severity,
            crate::admin::error_snapshot_db::SnapshotSeverity::Critical
        );
        assert!(detail.summary.retention_exempt);
    }

    #[test]
    fn critical_protocol_error_is_not_downgraded_by_generic_final_error() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        ctx.record_internal_error("sse_state_error", "decoder overflow");

        let id = ctx
            .finalize(SnapshotFinalState::error("bad_request", Some(502)))
            .unwrap()
            .unwrap();
        let detail = store.get(&id).unwrap().unwrap();

        assert_eq!(detail.summary.error_type, "sse_state_error");
        assert_eq!(detail.summary.severity, SnapshotSeverity::Critical);
        assert!(detail.summary.retention_exempt);
    }

    #[test]
    fn disabled_policy_skips_snapshot_context_creation() {
        let store = test_store();
        let mut policy = store.policy();
        policy.enabled = false;
        store.set_policy(policy);
        let key = crate::anthropic::middleware::KeyContext {
            key_id: 7,
            group: None,
            key_source: crate::admin::trace_db::TraceKeySource::ClientKey,
        };
        let request: crate::anthropic::types::MessagesRequest =
            serde_json::from_value(serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "must not be copied"}]
            }))
            .unwrap();

        assert!(
            ErrorSnapshotContext::new_if_enabled(
                store,
                "trace-disabled".to_string(),
                &key,
                &axum::http::HeaderMap::new(),
                &request,
            )
            .is_none()
        );
    }

    #[test]
    fn interrupted_byte_count_is_preserved_in_diagnostics() {
        let store = test_store();
        let ctx = sample_context(store.clone(), true, true);
        let id = ctx
            .finalize(SnapshotFinalState::interrupted("client_closed", 1234))
            .unwrap()
            .unwrap();
        let detail = store.get(&id).unwrap().unwrap();
        let seq = detail
            .payloads
            .iter()
            .find(|payload| payload.kind == SnapshotPayloadKind::ToolDiagnostics)
            .unwrap()
            .seq;
        let payload = store.read_payload(&id, seq).unwrap().unwrap();
        let diagnostics: serde_json::Value = serde_json::from_slice(&payload.data).unwrap();

        assert_eq!(diagnostics["interrupted_after_bytes"], 1234);
    }

    #[test]
    fn stream_tail_keeps_latest_256_kib_on_utf8_boundary() {
        let mut tail = StreamTail::default();
        tail.push("开头".repeat(100_000).as_bytes());
        tail.push(b"FINAL_EVENT");
        let bytes = tail.snapshot_bytes();

        assert!(bytes.len() <= STREAM_TAIL_MAX_BYTES);
        assert!(std::str::from_utf8(&bytes).is_ok());
        assert!(bytes.ends_with(b"FINAL_EVENT"));
    }

    #[test]
    fn stream_tail_replaces_invalid_utf8_with_length_and_digest() {
        let mut tail = StreamTail::default();
        tail.push(&[0xff, 0xfe, 0xfd]);
        let value: serde_json::Value = serde_json::from_slice(&tail.snapshot_bytes()).unwrap();

        assert_eq!(value["invalid_utf8"], true);
        assert_eq!(value["original_bytes"], 3);
        assert_eq!(value["sha256"].as_str().unwrap().len(), 64);
    }

    #[test]
    fn tool_diagnostics_reports_invalid_duplicate_and_unmatched_ids() {
        let request: crate::anthropic::types::MessagesRequest = serde_json::from_value(
            serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": "tool/get_weather/1", "name": "get_weather", "input": {}},
                        {"type": "tool_use", "id": "duplicate", "name": "get_weather", "input": {}},
                        {"type": "tool_use", "id": "duplicate", "name": "get_weather", "input": {}}
                    ]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": "missing-result", "content": "none"}
                    ]}
                ]
            }),
        )
        .unwrap();

        let diagnostics = analyze_tool_links(&request);

        assert_eq!(diagnostics.invalid_ids, vec!["tool/get_weather/1"]);
        assert_eq!(diagnostics.duplicate_tool_use_ids, vec!["duplicate"]);
        assert_eq!(diagnostics.unmatched_tool_results, vec!["missing-result"]);
        assert!(
            diagnostics
                .missing_tool_results
                .contains(&"tool/get_weather/1".to_string())
        );
    }

    #[test]
    fn tool_diagnostics_only_links_protocol_valid_roles() {
        let long_valid_id = "a".repeat(64);
        let request: crate::anthropic::types::MessagesRequest = serde_json::from_value(
            serde_json::json!({
                "model": "claude-opus-4-8",
                "max_tokens": 64,
                "messages": [
                    {"role": "user", "content": [
                        {"type": "tool_use", "id": "wrong-role-use", "name": "ignored", "input": {}},
                        {"type": "tool_result", "tool_use_id": long_valid_id, "content": "ok"}
                    ]},
                    {"role": "assistant", "content": [
                        {"type": "tool_use", "id": long_valid_id, "name": "valid", "input": {}},
                        {"type": "tool_result", "tool_use_id": "wrong-role-result", "content": "ignored"}
                    ]}
                ]
            }),
        )
        .unwrap();

        let diagnostics = analyze_tool_links(&request);

        assert!(diagnostics.invalid_ids.is_empty());
        assert!(diagnostics.unmatched_tool_results.is_empty());
        assert!(diagnostics.missing_tool_results.is_empty());
        assert_eq!(diagnostics.block_order.len(), 2);
    }
}
