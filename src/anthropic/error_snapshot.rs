use std::io::Read as _;

use base64::Engine as _;
use sha2::{Digest as _, Sha256};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
