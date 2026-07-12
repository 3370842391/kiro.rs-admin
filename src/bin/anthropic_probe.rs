use std::path::PathBuf;

use base64::Engine as _;
use futures::{StreamExt, future::join_all};
use serde_json::{Value, json};
use uuid::Uuid;

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
    Skip(String),
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
    let has = response["content"].as_array().is_some_and(|blocks| {
        blocks
            .iter()
            .any(|block| block["type"] == "tool_use" && block["name"].as_str() == Some(name))
    });
    if has && response["stop_reason"] == "tool_use" {
        ProbeResult::Pass
    } else {
        ProbeResult::Fail(format!("response did not call required tool {name}"))
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

async fn pdf_probe(client: &reqwest::Client, args: &Args, key: &str) -> ProbeResult {
    let Some(path) = &args.pdf else {
        return ProbeResult::Skip("--pdf was not provided".into());
    };
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(error) => return ProbeResult::Fail(error.to_string()),
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
                {"type": "text", "text": "Return the exact verification token printed in the document."}
            ]}]
        }),
    )
    .await
    {
        Ok(value) if value["content"].as_array().is_some_and(|blocks| !blocks.is_empty()) => {
            ProbeResult::Pass
        }
        Ok(_) => ProbeResult::Fail("PDF request returned empty content".into()),
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
    let response = match client
        .post(format!(
            "{}/v1/messages",
            args.base_url.trim_end_matches('/')
        ))
        .header("x-api-key", key)
        .header("authorization", format!("Bearer {key}"))
        .header("anthropic-version", "2023-06-01")
        .json(&json!({
            "model": args.model,
            "max_tokens": 64,
            "stream": true,
            "messages": [{"role": "user", "content": "Reply with OK."}]
        }))
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => return ProbeResult::Fail(format!("HTTP {}", response.status())),
        Err(error) => return ProbeResult::Fail(error.to_string()),
    };

    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut events = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => return ProbeResult::Fail(error.to_string()),
        };
        buffer.push_str(&String::from_utf8_lossy(&chunk));
        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer.drain(..pos + 2);
            for line in frame.lines() {
                if let Some(data) = line.strip_prefix("data: ")
                    && let Ok(value) = serde_json::from_str::<Value>(data)
                {
                    events.push(value);
                }
            }
        }
    }
    classify_sse(&events)
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
}
