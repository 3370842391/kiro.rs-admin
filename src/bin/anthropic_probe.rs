use std::path::PathBuf;
use std::time::Instant;

use base64::Engine as _;
use futures::{StreamExt, future::join_all};
use serde_json::{Value, json};
use uuid::Uuid;

const BUILTIN_TEXT_PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvUmVzb3VyY2VzIDw8IC9Gb250IDw8IC9GMSA0IDAgUiA+PiA+PiAvQ29udGVudHMgNSAwIFIgPj4KZW5kb2JqCjQgMCBvYmoKPDwgL1R5cGUgL0ZvbnQgL1N1YnR5cGUgL1R5cGUxIC9CYXNlRm9udCAvSGVsdmV0aWNhID4+CmVuZG9iago1IDAgb2JqCjw8IC9MZW5ndGggNTQgPj4Kc3RyZWFtCkJUIC9GMSAxMiBUZiA3MiA3MjAgVGQgKFBERi1DT01QQVRJQklMSVRZLVRPS0VOKSBUaiBFVAplbmRzdHJlYW0KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzExIDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDE1CiUlRU9GCg==";

#[derive(Debug)]
struct Args {
    base_url: String,
    model: String,
    pdf: Option<PathBuf>,
    parallel: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeResult {
    Pass,
    Fail(String),
}

fn parse_args_from<I, S>(args: I) -> Result<Args, String>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut iter = args.into_iter().map(Into::into).skip(1);
    let mut base_url = None;
    let mut model = None;
    let mut pdf = None;
    let mut parallel = 16usize;
    while let Some(flag) = iter.next() {
        match flag.as_str() {
            "--base-url" => base_url = iter.next(),
            "--model" => model = iter.next(),
            "--pdf" => pdf = iter.next().map(PathBuf::from),
            "--parallel" => {
                parallel = iter
                    .next()
                    .ok_or("--parallel requires a value")?
                    .parse()
                    .map_err(|_| "--parallel must be a positive integer")?;
                if parallel == 0 {
                    return Err("--parallel must be a positive integer".into());
                }
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Args {
        base_url: base_url.ok_or("--base-url is required")?,
        model: model.ok_or("--model is required")?,
        pdf,
        parallel,
    })
}

fn classify_thinking(response: &Value) -> ProbeResult {
    let has = response["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        })
    });
    if has {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail("response contains no thinking block".into())
    }
}

fn classify_required_tool(response: &Value, name: &str) -> ProbeResult {
    let first_non_thinking = response["content"].as_array().and_then(|blocks| {
        blocks.iter().find(|block| {
            !matches!(
                block["type"].as_str(),
                Some("thinking" | "redacted_thinking")
            )
        })
    });
    let first_is_required_tool = first_non_thinking
        .is_some_and(|block| block["type"] == "tool_use" && block["name"].as_str() == Some(name));
    if first_is_required_tool && response["stop_reason"] == "tool_use" {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!(
            "required tool {name} was not the first non-thinking content block"
        ))
    }
}

fn classify_exact_text(response: &Value, expected: &str) -> ProbeResult {
    let Some(blocks) = response["content"].as_array() else {
        return ProbeResult::Fail("response content is not an array".into());
    };
    if blocks.len() == 1
        && blocks[0]["type"] == "text"
        && blocks[0]["text"].as_str() == Some(expected)
    {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!("response was not exactly {expected:?}"))
    }
}

fn classify_strict_json(response: &Value) -> ProbeResult {
    let Some(text) = response["content"]
        .as_array()
        .filter(|blocks| blocks.len() == 1)
        .and_then(|blocks| blocks[0]["text"].as_str())
    else {
        return ProbeResult::Fail("strict JSON response was not one text block".into());
    };
    if text.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return ProbeResult::Fail("strict JSON response was not minified".into());
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return ProbeResult::Fail("strict JSON response was not valid JSON".into());
    };
    if value["a"] == "ztset"
        && value["b"] == 37
        && value["c"] == "PROBE-JSON"
        && value.as_object().is_some_and(|object| object.len() == 3)
    {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail("strict JSON response did not satisfy expected fields".into())
    }
}

fn classify_sse(events: &[Value]) -> ProbeResult {
    let start = events
        .iter()
        .position(|event| event["type"] == "message_start");
    let delta = events
        .iter()
        .rposition(|event| event["type"] == "message_delta");
    let stop = events
        .iter()
        .rposition(|event| event["type"] == "message_stop");
    match (start, delta, stop) {
        (Some(start), Some(delta), Some(stop)) if start < delta && delta < stop => {
            ProbeResult::Pass
        }
        _ => ProbeResult::Fail("SSE event order is incomplete or invalid".into()),
    }
}

fn classify_local_text_sse(events: &[Value], expected: &str) -> ProbeResult {
    let event_types = events
        .iter()
        .filter_map(|event| event["type"].as_str())
        .collect::<Vec<_>>();
    let expected_types = [
        "message_start",
        "content_block_start",
        "content_block_delta",
        "content_block_stop",
        "message_delta",
        "message_stop",
    ];
    if event_types != expected_types {
        return ProbeResult::Fail(
            "local SSE did not contain the complete six-event sequence".into(),
        );
    }
    let text = events
        .iter()
        .filter(|event| event["type"] == "content_block_delta")
        .filter(|event| event["delta"]["type"] == "text_delta")
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect::<String>();
    if text == expected && events[4]["delta"]["stop_reason"] == "end_turn" {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!("local SSE text mismatch: expected {expected:?}"))
    }
}

fn classify_strict_json_sse(events: &[Value]) -> ProbeResult {
    let text = events
        .iter()
        .filter(|event| event["type"] == "content_block_delta")
        .filter(|event| event["delta"]["type"] == "text_delta")
        .filter_map(|event| event["delta"]["text"].as_str())
        .collect::<String>();
    if !matches!(classify_local_text_sse(events, &text), ProbeResult::Pass) {
        return ProbeResult::Fail(
            "strict JSON stream was missing the complete local SSE sequence".into(),
        );
    }
    classify_strict_json(&json!({"content": [{"type": "text", "text": text}]}))
}

#[derive(Debug)]
struct StreamCapture {
    events: Vec<Value>,
    transport_yields: usize,
}

fn classify_strict_json_stream_capture(capture: &StreamCapture) -> ProbeResult {
    let classified = classify_strict_json_sse(&capture.events);
    if !matches!(classified, ProbeResult::Pass) {
        return classified;
    }
    if capture.transport_yields < 2 {
        return ProbeResult::Fail(
            "strict JSON SSE arrived in one transport yield; intermediary buffering is suspected"
                .into(),
        );
    }
    ProbeResult::Pass
}

fn coefficient_of_variation(samples: &[f64]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if mean <= f64::EPSILON {
        return 0.0;
    }
    let variance = samples
        .iter()
        .map(|sample| {
            let delta = sample - mean;
            delta * delta
        })
        .sum::<f64>()
        / samples.len() as f64;
    variance.sqrt() / mean
}

fn passive_tools_system_request(model: &str, expected: &str) -> Value {
    json!({
        "model": model,
        "max_tokens": 64,
        "system": format!(
            "Respond to every user message with exactly the single word '{expected}' and nothing else. No explanation or markdown."
        ),
        "messages": [{"role": "user", "content": "hello"}],
        "tools": [{
            "name": "passive_probe_tool",
            "description": "An optional probe tool. Do not call it unless needed.",
            "input_schema": {"type": "object", "properties": {}}
        }]
    })
}

fn identity_passive_tools_system_request(model: &str, expected: &str) -> Value {
    let mut request = passive_tools_system_request(model, expected);
    request["system"] = json!([
        {
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude."
        },
        {
            "type": "text",
            "text": format!(
                "Respond to every user message with exactly the single word '{expected}' and nothing else. No explanation or markdown."
            )
        }
    ]);
    request
}

async fn post_message(
    client: &reqwest::Client,
    args: &Args,
    api_key: &str,
    body: Value,
) -> Result<Value, String> {
    let response = client
        .post(format!(
            "{}/v1/messages",
            args.base_url.trim_end_matches('/')
        ))
        .header("x-api-key", api_key)
        .header("authorization", format!("Bearer {api_key}"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let value: Value = response.json().await.map_err(|error| error.to_string())?;
    if status.is_success() {
        Ok(value)
    } else {
        Err(format!("HTTP {}: {}", status.as_u16(), value))
    }
}

async fn post_stream_events(
    client: &reqwest::Client,
    args: &Args,
    api_key: &str,
    body: Value,
) -> Result<StreamCapture, String> {
    let response = client
        .post(format!(
            "{}/v1/messages",
            args.base_url.trim_end_matches('/')
        ))
        .header("x-api-key", api_key)
        .header("authorization", format!("Bearer {api_key}"))
        .header("anthropic-version", "2023-06-01")
        .json(&body)
        .send()
        .await
        .map_err(|error| error.to_string())?;
    if !response.status().is_success() {
        return Err(format!("HTTP {}", response.status()));
    }

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    let mut transport_yields = 0usize;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| error.to_string())?;
        transport_yields += 1;
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer.drain(..pos + 2);
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    let value = serde_json::from_str::<Value>(data)
                        .map_err(|error| format!("invalid SSE data: {error}"))?;
                    events.push(value);
                }
            }
        }
    }
    if !buffer.trim().is_empty() {
        return Err("SSE response ended with an incomplete frame".into());
    }
    Ok(StreamCapture {
        events,
        transport_yields,
    })
}

async fn thinking_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 256,
            "thinking": {"type": "enabled", "budget_tokens": 1024},
            "messages": [{"role": "user", "content": "Reply with a short answer."}]
        }),
    )
    .await
    {
        Ok(value) => classify_thinking(&value),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn tool_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let name = "probe_echo";
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": "Call the provided tool with value local-check."}],
            "tools": [{
                "name": name,
                "description": "Return the provided value.",
                "input_schema": {
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"]
                }
            }],
            "tool_choice": {"type": "tool", "name": name}
        }),
    )
    .await
    {
        Ok(value) => classify_required_tool(&value, name),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn exact_system_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let expected = "SYSTEM_EXACT_42";
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 64,
            "system": format!(
                "Respond to every user message with exactly the single word '{expected}' and nothing else. No explanation or markdown."
            ),
            "messages": [{"role": "user", "content": "hello"}]
        }),
    )
    .await
    {
        Ok(value) => classify_exact_text(&value, expected),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn passive_tools_system_probe(
    client: &reqwest::Client,
    args: &Args,
    key: &str,
) -> ProbeResult {
    let expected = format!("PASSIVE_{}", Uuid::new_v4().simple());
    match post_message(
        client,
        args,
        key,
        passive_tools_system_request(&args.model, &expected),
    )
    .await
    {
        Ok(value) => classify_exact_text(&value, &expected),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn identity_passive_tools_system_probe(
    client: &reqwest::Client,
    args: &Args,
    key: &str,
) -> ProbeResult {
    let expected = format!("IDENTITY_{}", Uuid::new_v4().simple());
    match post_message(
        client,
        args,
        key,
        identity_passive_tools_system_request(&args.model, &expected),
    )
    .await
    {
        Ok(value) => classify_exact_text(&value, &expected),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn echo_token_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let expected = format!("ECHO_{}", Uuid::new_v4().simple());
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 64,
            "messages": [{
                "role": "user",
                "content": format!("Echo this token exactly: {expected}")
            }]
        }),
    )
    .await
    {
        Ok(value) => classify_exact_text(&value, &expected),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn strict_json_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 128,
            "messages": [{
                "role": "user",
                "content": "You must reply with exactly one minified JSON object and no markdown, no explanation. Schema: {\"a\": string, \"b\": number, \"c\": string}. Set a to the reverse of 'testz'. Set b to 29 + 8. Set c to 'PROBE-JSON'."
            }]
        }),
    )
    .await
    {
        Ok(value) => classify_strict_json(&value),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn strict_json_stream_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    match post_stream_events(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 128,
            "stream": true,
            "messages": [{
                "role": "user",
                "content": "You must reply with exactly one minified JSON object and no markdown, no explanation. Schema: {\"a\": string, \"b\": number, \"c\": string}. Set a to the reverse of 'testz'. Set b to 29 + 8. Set c to 'PROBE-JSON'."
            }]
        }),
    )
    .await
    {
        Ok(capture) => classify_strict_json_stream_capture(&capture),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn ping_health_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let mut latencies_ms = Vec::with_capacity(20);
    for _ in 0..20 {
        let started_at = Instant::now();
        let response = post_message(
            client,
            args,
            key,
            json!({
                "model": args.model,
                "max_tokens": 16,
                "messages": [{"role": "user", "content": "ping"}]
            }),
        )
        .await;
        latencies_ms.push(started_at.elapsed().as_secs_f64() * 1000.0);
        match response {
            Ok(value) if classify_exact_text(&value, "pong") == ProbeResult::Pass => {}
            Ok(value) => {
                return ProbeResult::Fail(format!(
                    "ping health response was not exactly pong: {value}"
                ));
            }
            Err(error) => return ProbeResult::Fail(error),
        }
    }

    let mean_ms = latencies_ms.iter().sum::<f64>() / latencies_ms.len() as f64;
    let cv = coefficient_of_variation(&latencies_ms);
    if cv > 0.25 {
        ProbeResult::Fail(format!(
            "ping health latency was unstable: mean_ms={mean_ms:.3} cv={cv:.3}"
        ))
    } else {
        ProbeResult::Pass
    }
}

async fn pdf_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let bytes = match &args.pdf {
        Some(path) => match tokio::fs::read(path).await {
            Ok(bytes) => bytes,
            Err(error) => return ProbeResult::Fail(error.to_string()),
        },
        None => match base64::engine::general_purpose::STANDARD.decode(BUILTIN_TEXT_PDF_B64) {
            Ok(bytes) => bytes,
            Err(error) => return ProbeResult::Fail(error.to_string()),
        },
    };
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    match post_message(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 256,
            "messages": [{"role": "user", "content": [
                {"type": "document", "source": {
                    "type": "base64", "media_type": "application/pdf", "data": encoded
                }},
                {"type": "text", "text": "Extract the identifier formatted like 'PDF-COMPATIBILITY-xxxxx' and reply with ONLY the identifier, no explanation."}
            ]}]
        }),
    )
    .await
    {
        Ok(value) => classify_pdf(&value),
        Err(error) => ProbeResult::Fail(error),
    }
}

async fn parallel_canary_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let jobs = (0..args.parallel).map(|_| {
        let client = client.clone();
        let key = key.to_string();
        let base_url = args.base_url.clone();
        let model = args.model.clone();
        async move {
            let canary = format!("CANARY_{}", Uuid::new_v4().simple());
            let local_args = Args {
                base_url,
                model,
                pdf: None,
                parallel: 1,
            };
            let response = post_message(
                &client,
                &local_args,
                &key,
                json!({
                    "model": local_args.model,
                    "max_tokens": 64,
                    "system": format!("Reply with exactly {canary} and no other text."),
                    "messages": [{"role": "user", "content": "Follow the system instruction."}]
                }),
            )
            .await?;
            let text = response["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|block| block["text"].as_str())
                .collect::<String>();
            Ok::<_, String>((canary, text))
        }
    });
    for result in join_all(jobs).await {
        match result {
            Ok((canary, text)) if text.trim() == canary => {}
            Ok((canary, text)) => {
                return ProbeResult::Fail(format!(
                    "canary mismatch: expected {canary}, got {text:?}"
                ));
            }
            Err(error) => return ProbeResult::Fail(error),
        }
    }
    ProbeResult::Pass
}

async fn stream_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    match post_stream_events(
        client,
        args,
        key,
        json!({
            "model": args.model,
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }),
    )
    .await
    {
        Ok(capture) => classify_sse(&capture.events),
        Err(error) => ProbeResult::Fail(error),
    }
}

#[tokio::main]
async fn main() {
    let args = match parse_args_from(std::env::args()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let api_key =
        std::env::var("ANTHROPIC_API_KEY").or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN"));
    let api_key = match api_key {
        Ok(key) if !key.trim().is_empty() => key,
        _ => {
            eprintln!("ANTHROPIC_API_KEY or ANTHROPIC_AUTH_TOKEN is required");
            std::process::exit(2);
        }
    };

    let client = reqwest::Client::new();
    let results = [
        ("thinking", thinking_probe(&client, &args, &api_key).await),
        ("tool_choice", tool_probe(&client, &args, &api_key).await),
        (
            "system_exact",
            exact_system_probe(&client, &args, &api_key).await,
        ),
        (
            "system_passive_tools",
            passive_tools_system_probe(&client, &args, &api_key).await,
        ),
        (
            "system_identity_passive_tools",
            identity_passive_tools_system_probe(&client, &args, &api_key).await,
        ),
        (
            "echo_token",
            echo_token_probe(&client, &args, &api_key).await,
        ),
        (
            "strict_json",
            strict_json_probe(&client, &args, &api_key).await,
        ),
        (
            "strict_json_stream",
            strict_json_stream_probe(&client, &args, &api_key).await,
        ),
        (
            "ping_health",
            ping_health_probe(&client, &args, &api_key).await,
        ),
        ("pdf", pdf_probe(&client, &args, &api_key).await),
        (
            "parallel_canary",
            parallel_canary_probe(&client, &args, &api_key).await,
        ),
        ("stream", stream_probe(&client, &args, &api_key).await),
    ];
    let mut failed = false;
    for (name, result) in results {
        println!("{name}: {result:?}");
        failed |= matches!(result, ProbeResult::Fail(_));
    }
    if failed {
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_args_requires_base_url_and_model() {
        let args = parse_args_from([
            "anthropic_probe",
            "--base-url",
            "http://127.0.0.1:8990",
            "--model",
            "claude-opus-4-8",
        ])
        .unwrap();
        assert_eq!(args.base_url, "http://127.0.0.1:8990");
        assert_eq!(args.model, "claude-opus-4-8");
        assert_eq!(args.parallel, 16);
    }

    #[test]
    fn classify_thinking_requires_reasoning_block() {
        let response = json!({"content": [{"type": "text", "text": "plain"}]});
        assert!(matches!(classify_thinking(&response), ProbeResult::Fail(_)));
    }

    #[test]
    fn classify_required_tool_checks_name() {
        let response = json!({
            "content": [{
                "type": "tool_use",
                "name": "probe_echo",
                "input": {"value": "x"}
            }],
            "stop_reason": "tool_use"
        });
        assert_eq!(
            classify_required_tool(&response, "probe_echo"),
            ProbeResult::Pass
        );

        let text_first = json!({
            "content": [
                {"type": "text", "text": "I will call it."},
                {"type": "tool_use", "name": "probe_echo", "input": {"value": "x"}}
            ],
            "stop_reason": "tool_use"
        });
        assert!(matches!(
            classify_required_tool(&text_first, "probe_echo"),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn exact_system_classifier_requires_only_the_expected_text() {
        let correct = json!({"content": [{"type":"text","text":"SYSTEM_EXACT_42"}]});
        let explanatory = json!({"content": [{"type":"text","text":"Answer: SYSTEM_EXACT_42"}]});
        assert_eq!(
            classify_exact_text(&correct, "SYSTEM_EXACT_42"),
            ProbeResult::Pass
        );
        assert!(matches!(
            classify_exact_text(&explanatory, "SYSTEM_EXACT_42"),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn passive_tools_system_request_keeps_the_tool_optional() {
        let body = passive_tools_system_request("claude-opus-4-8", "PASSIVE_42");
        assert_eq!(
            body["system"].as_str().unwrap().contains("PASSIVE_42"),
            true
        );
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn identity_passive_tools_request_keeps_identity_and_exact_contract_separate() {
        let body = identity_passive_tools_system_request("claude-opus-4-8", "IDENTITY_42");
        let system = body["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(
            system[0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
        assert!(system[1]["text"].as_str().unwrap().contains("IDENTITY_42"));
        assert!(body["tools"].is_array());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn local_text_sse_classifier_requires_six_events_and_exact_echo() {
        let events = vec![
            json!({"type": "message_start"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ECHO_42"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ];
        assert_eq!(
            classify_local_text_sse(&events, "ECHO_42"),
            ProbeResult::Pass
        );
        assert!(matches!(
            classify_local_text_sse(&events[..5], "ECHO_42"),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn strict_json_sse_classifier_aggregates_the_complete_local_sequence() {
        let events = vec![
            json!({"type": "message_start"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "{\"a\":\"ztset\",\"b\":37,\"c\":\"PROBE-JSON\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ];
        assert_eq!(classify_strict_json_sse(&events), ProbeResult::Pass);
        assert!(matches!(
            classify_strict_json_sse(&events[..5]),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn strict_json_stream_capture_requires_multiple_transport_yields() {
        let events = vec![
            json!({"type": "message_start"}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "{\"a\":\"ztset\",\"b\":37,\"c\":\"PROBE-JSON\"}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ];
        assert_eq!(
            classify_strict_json_stream_capture(&StreamCapture {
                events: events.clone(),
                transport_yields: 2,
            }),
            ProbeResult::Pass
        );
        assert!(matches!(
            classify_strict_json_stream_capture(&StreamCapture {
                events,
                transport_yields: 1,
            }),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn strict_json_classifier_checks_minified_schema_values() {
        let correct = json!({
            "content": [{"type":"text","text":"{\"a\":\"ztset\",\"b\":37,\"c\":\"PROBE-JSON\"}"}]
        });
        let markdown = json!({
            "content": [{"type":"text","text":"```json\n{\"a\":\"ztset\",\"b\":37,\"c\":\"PROBE-JSON\"}\n```"}]
        });
        assert_eq!(classify_strict_json(&correct), ProbeResult::Pass);
        assert!(matches!(
            classify_strict_json(&markdown),
            ProbeResult::Fail(_)
        ));
    }

    #[test]
    fn latency_cv_is_zero_for_stable_samples() {
        assert_eq!(coefficient_of_variation(&[10.0, 10.0, 10.0]), 0.0);
        let cv = coefficient_of_variation(&[10.0, 11.0, 9.0]);
        assert!(cv > 0.0 && cv < 0.1);
    }

    #[test]
    fn classify_sse_requires_ordered_terminal_events() {
        let events = vec![
            json!({"type": "message_start"}),
            json!({"type": "content_block_start", "content_block": {"type": "text"}}),
            json!({"type": "content_block_stop"}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}}),
            json!({"type": "message_stop"}),
        ];
        assert_eq!(classify_sse(&events), ProbeResult::Pass);
        assert!(matches!(classify_sse(&events[..4]), ProbeResult::Fail(_)));
    }

    #[test]
    fn pdf_probe_requires_the_exact_document_identifier() {
        let correct = json!({
            "content": [{"type": "text", "text": "PDF-COMPATIBILITY-TOKEN"}]
        });
        let explanatory = json!({
            "content": [{"type": "text", "text": "The token is PDF-COMPATIBILITY-TOKEN."}]
        });

        assert_eq!(classify_pdf(&correct), ProbeResult::Pass);
        assert!(matches!(classify_pdf(&explanatory), ProbeResult::Fail(_)));
    }
}

fn classify_pdf(response: &Value) -> ProbeResult {
    let text = response["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|block| block["text"].as_str())
        .collect::<String>();
    if text.trim() == "PDF-COMPATIBILITY-TOKEN" {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!(
            "PDF identifier mismatch: expected PDF-COMPATIBILITY-TOKEN, got {text:?}"
        ))
    }
}
