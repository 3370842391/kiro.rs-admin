//! WebSearch 工具处理模块
//!
//! 实现 Anthropic WebSearch 请求到 Kiro MCP 的转换和响应生成

use std::convert::Infallible;

use axum::{
    body::Body,
    http::StatusCode,
    response::{IntoResponse, Json, Response},
};
use bytes::Bytes;
use futures::{Stream, stream};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use super::stream::SseEvent;
use super::types::{ErrorResponse, MessagesRequest};

/// MCP 请求
#[derive(Debug, Serialize)]
pub struct McpRequest {
    pub id: String,
    pub jsonrpc: String,
    pub method: String,
    pub params: McpParams,
}

/// MCP 请求参数
#[derive(Debug, Serialize)]
pub struct McpParams {
    pub name: String,
    pub arguments: McpArguments,
}

/// MCP 参数
#[derive(Debug, Serialize)]
pub struct McpArguments {
    pub query: String,
}

/// MCP 响应
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResponse {
    pub error: Option<McpError>,
    pub id: String,
    pub jsonrpc: String,
    pub result: Option<McpResult>,
}

/// MCP 错误
#[derive(Debug, Deserialize)]
pub struct McpError {
    pub code: Option<i32>,
    pub message: Option<String>,
}

/// MCP 结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct McpResult {
    pub content: Vec<McpContent>,
    #[serde(rename = "isError")]
    pub is_error: bool,
}

/// MCP 内容
#[derive(Debug, Deserialize)]
pub struct McpContent {
    #[serde(rename = "type")]
    pub content_type: String,
    pub text: String,
}

/// WebSearch 搜索结果
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct WebSearchResults {
    pub results: Vec<WebSearchResult>,
    #[serde(rename = "totalResults")]
    pub total_results: Option<i32>,
    pub query: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchError {
    pub error_code: String,
    pub message: Option<String>,
}

/// 单个搜索结果
#[derive(Debug, Deserialize, Clone)]
#[allow(dead_code)]
pub struct WebSearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    #[serde(rename = "publishedDate")]
    pub published_date: Option<i64>,
    pub id: Option<String>,
    pub domain: Option<String>,
    #[serde(rename = "maxVerbatimWordLimit")]
    pub max_verbatim_word_limit: Option<i32>,
    #[serde(rename = "publicDomain")]
    pub public_domain: Option<bool>,
}

fn is_native_web_search_tool(t: &crate::anthropic::types::Tool) -> bool {
    t.name == "web_search"
        && t.tool_type
            .as_deref()
            .is_some_and(|typ| typ.starts_with("web_search_"))
}

fn web_search_tool_choice_allows(req: &MessagesRequest, tool_name: &str) -> bool {
    match req.tool_choice.as_ref() {
        None
        | Some(crate::anthropic::types::ToolChoice::Auto { .. })
        | Some(crate::anthropic::types::ToolChoice::Any { .. }) => true,
        Some(crate::anthropic::types::ToolChoice::Tool { name, .. }) => name == tool_name,
        Some(crate::anthropic::types::ToolChoice::None { .. }) => false,
    }
}

/// 原生 WebSearch 都进入模型工具循环。仅声明这个工具不等于客户端已经要求调用它；
/// 先让模型决定是否搜索及搜索词，避免把整条用户正文直接当成 MCP query。
pub(crate) fn has_web_search_among_tools(req: &MessagesRequest) -> bool {
    req.tools.as_ref().is_some_and(|tools| {
        let has_native = tools.iter().any(is_native_web_search_tool);
        has_native && web_search_tool_choice_allows(req, "web_search")
    })
}

/// 从消息中提取搜索查询
///
/// 读取本轮最后一条 user 消息的文本内容
/// 并去除 "Perform a web search for the query: " 前缀
pub fn extract_search_query(req: &MessagesRequest) -> Option<String> {
    // 多轮请求中第一条消息通常是历史问题，必须取本轮最后一条 user 消息。
    let first_msg = req.messages.iter().rev().find(|msg| msg.role == "user")?;

    // 提取文本内容
    let text = match &first_msg.content {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(arr) => {
            // 内容块可能先出现图片等非文本块，取最后一个文本块。
            let text_block = arr
                .iter()
                .rev()
                .find(|block| block.get("type").and_then(|value| value.as_str()) == Some("text"))?;
            text_block.get("text")?.as_str()?.to_string()
        }
        _ => return None,
    };

    // 去除前缀 "Perform a web search for the query: "
    const PREFIX: &str = "Perform a web search for the query: ";
    let query = if text.starts_with(PREFIX) {
        text[PREFIX.len()..].to_string()
    } else {
        text
    };

    if query.is_empty() { None } else { Some(query) }
}

/// 生成22位大小写字母和数字的随机字符串
fn generate_random_id_22() -> String {
    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    (0..22)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 生成8位小写字母和数字的随机字符串
fn generate_random_id_8() -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    (0..8)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

/// 创建 MCP 请求
///
/// ID 格式: web_search_tooluse_{22位随机}_{毫秒时间戳}_{8位随机}
pub fn create_mcp_request(query: &str) -> (String, McpRequest) {
    let random_22 = generate_random_id_22();
    let timestamp = chrono::Utc::now().timestamp_millis();
    let random_8 = generate_random_id_8();

    let request_id = format!(
        "web_search_tooluse_{}_{}_{}",
        random_22, timestamp, random_8
    );

    // tool_use_id 使用相同格式
    let tool_use_id = format!(
        "srvtoolu_{}",
        Uuid::new_v4().to_string().replace('-', "")[..32].to_string()
    );

    let request = McpRequest {
        id: request_id,
        jsonrpc: "2.0".to_string(),
        method: "tools/call".to_string(),
        params: McpParams {
            name: "web_search".to_string(),
            arguments: McpArguments {
                query: query.to_string(),
            },
        },
    };

    (tool_use_id, request)
}

/// 解析 MCP 响应中的搜索结果
pub fn parse_search_results_checked(
    mcp_response: &McpResponse,
) -> Result<WebSearchResults, WebSearchError> {
    let result = mcp_response.result.as_ref().ok_or_else(|| WebSearchError {
        error_code: "unavailable".to_string(),
        message: Some("web search returned no result".to_string()),
    })?;
    let content = result.content.first().ok_or_else(|| WebSearchError {
        error_code: "unavailable".to_string(),
        message: Some("web search returned empty content".to_string()),
    })?;

    if content.content_type != "text" {
        return Err(WebSearchError {
            error_code: "unavailable".to_string(),
            message: Some("web search returned unsupported content".to_string()),
        });
    }

    let value: serde_json::Value =
        serde_json::from_str(&content.text).map_err(|_| WebSearchError {
            error_code: "unavailable".to_string(),
            message: Some("web search returned malformed content".to_string()),
        })?;
    if value.get("type").and_then(|v| v.as_str()) == Some("web_search_tool_result_error") {
        return Err(WebSearchError {
            error_code: value
                .get("error_code")
                .and_then(|v| v.as_str())
                .unwrap_or("unavailable")
                .to_string(),
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        });
    }

    if result.is_error {
        return Err(WebSearchError {
            error_code: "unavailable".to_string(),
            message: value
                .get("message")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| Some("web search provider returned an error".to_string())),
        });
    }

    let parsed: WebSearchResults = serde_json::from_value(value).map_err(|_| WebSearchError {
        error_code: "unavailable".to_string(),
        message: Some("web search returned an invalid result shape".to_string()),
    })?;
    if let Some(error) = parsed.error.clone() {
        return Err(WebSearchError {
            error_code: error,
            message: None,
        });
    }
    Ok(parsed)
}

/// 兼容内部 agentic loop 的历史 Option 接口；新路由必须使用 checked 版本，
/// 不能把 MCP 错误降级成“没有搜索结果”。
pub fn parse_search_results(mcp_response: &McpResponse) -> Option<WebSearchResults> {
    parse_search_results_checked(mcp_response).ok()
}

/// 生成 WebSearch SSE 响应流
pub fn create_websearch_sse_stream(
    model: String,
    query: String,
    tool_use_id: String,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> impl Stream<Item = Result<Bytes, Infallible>> {
    let events =
        generate_websearch_events(&model, &query, &tool_use_id, search_results, input_tokens);

    stream::iter(
        events
            .into_iter()
            .map(|e| Ok(Bytes::from(e.to_sse_string()))),
    )
}

pub fn create_websearch_json_body(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: &Option<WebSearchResults>,
    input_tokens: i32,
) -> serde_json::Value {
    let summary = generate_search_summary(query, search_results);
    let output_tokens = (summary.len() as i32 + 3) / 4;
    let result_content = search_result_blocks(search_results);
    json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [
            {"type": "text", "text": format!("I'll search for \"{}\".", query)},
            {"type": "server_tool_use", "id": tool_use_id, "name": "web_search", "input": {"query": query}},
            {"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": result_content},
            {"type": "text", "text": summary}
        ],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "server_tool_use": {"web_search_requests": 1}
        }
    })
}

fn create_websearch_error_body(
    model: &str,
    tool_use_id: &str,
    error: &WebSearchError,
    input_tokens: i32,
) -> serde_json::Value {
    json!({
        "id": format!("msg_{}", Uuid::new_v4().to_string().replace('-', "")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": [{
            "type": "web_search_tool_result_error",
            "tool_use_id": tool_use_id,
            "error_code": error.error_code,
            "message": error.message
        }],
        "stop_reason": "end_turn",
        "stop_sequence": null,
        "usage": {
            "input_tokens": input_tokens,
            "output_tokens": 0,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0,
            "server_tool_use": {"web_search_requests": 1}
        }
    })
}

fn search_result_blocks(search_results: &Option<WebSearchResults>) -> Vec<serde_json::Value> {
    search_results
        .as_ref()
        .map(|results| {
            results
                .results
                .iter()
                .map(|r| {
                    let page_age = r.published_date.and_then(|ms| {
                        chrono::DateTime::from_timestamp_millis(ms)
                            .map(|dt| dt.format("%B %-d, %Y").to_string())
                    });
                    json!({
                        "type": "web_search_result",
                        "title": r.title,
                        "url": r.url,
                        "encrypted_content": r.snippet.clone().unwrap_or_default(),
                        "page_age": page_age
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// 生成 WebSearch SSE 事件序列
fn generate_websearch_events(
    model: &str,
    query: &str,
    tool_use_id: &str,
    search_results: Option<WebSearchResults>,
    input_tokens: i32,
) -> Vec<SseEvent> {
    let mut events = Vec::new();
    let message_id = format!(
        "msg_{}",
        Uuid::new_v4().to_string().replace('-', "")[..24].to_string()
    );

    // 1. message_start
    events.push(SseEvent::new(
        "message_start",
        json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "usage": {
                    "input_tokens": input_tokens,
                    "output_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0
                }
            }
        }),
    ));

    // 2. content_block_start (text - 搜索决策说明, index 0)
    let decision_text = format!("I'll search for \"{}\".", query);
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {
                "type": "text_delta",
                "text": decision_text
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 0
        }),
    ));

    // 3. content_block_start (server_tool_use, index 1)
    // Anthropic 流式协议要求 start 只携带 id/type/name，输入通过
    // input_json_delta 传输；把完整 input 塞进 start 会导致 streaming_shape 失败。
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {
                "id": tool_use_id,
                "type": "server_tool_use",
                "name": "web_search"
            }
        }),
    ));

    events.push(SseEvent::new(
        "content_block_delta",
        json!({
            "type": "content_block_delta",
            "index": 1,
            "delta": {
                "type": "input_json_delta",
                "partial_json": serde_json::to_string(&json!({"query": query})).unwrap_or_else(|_| "{}".to_string())
            }
        }),
    ));

    // 4. content_block_stop (server_tool_use)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 1
        }),
    ));

    // 5. content_block_start (web_search_tool_result, index 2)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 2,
            "content_block": {
                "type": "web_search_tool_result",
                "tool_use_id": tool_use_id,
                "content": search_result_blocks(&search_results)
            }
        }),
    ));

    // 6. content_block_stop (web_search_tool_result)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 2
        }),
    ));

    // 7. content_block_start (text, index 3)
    events.push(SseEvent::new(
        "content_block_start",
        json!({
            "type": "content_block_start",
            "index": 3,
            "content_block": {
                "type": "text",
                "text": ""
            }
        }),
    ));

    // 8. content_block_delta (text_delta) - 生成搜索结果摘要
    let summary = generate_search_summary(query, &search_results);

    // 分块发送文本
    let chunk_size = 100;
    for chunk in summary.chars().collect::<Vec<_>>().chunks(chunk_size) {
        let text: String = chunk.iter().collect();
        events.push(SseEvent::new(
            "content_block_delta",
            json!({
                "type": "content_block_delta",
                "index": 3,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            }),
        ));
    }

    // 9. content_block_stop (text)
    events.push(SseEvent::new(
        "content_block_stop",
        json!({
            "type": "content_block_stop",
            "index": 3
        }),
    ));

    // 10. message_delta
    // 官方 API 的 message_delta.delta 中没有 stop_sequence 字段
    let output_tokens = (summary.len() as i32 + 3) / 4; // 简单估算
    events.push(SseEvent::new(
        "message_delta",
        json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "end_turn"
            },
            "usage": {
                "output_tokens": output_tokens,
                "server_tool_use": {
                    "web_search_requests": 1
                }
            }
        }),
    ));

    // 11. message_stop
    events.push(SseEvent::new(
        "message_stop",
        json!({
            "type": "message_stop"
        }),
    ));

    events
}

/// 生成搜索结果摘要
pub(crate) fn generate_search_summary(query: &str, results: &Option<WebSearchResults>) -> String {
    let mut summary = format!("Here are the search results for \"{}\":\n\n", query);

    if let Some(results) = results {
        for (i, result) in results.results.iter().enumerate() {
            summary.push_str(&format!("{}. **{}**\n", i + 1, result.title));
            if let Some(ref snippet) = result.snippet {
                // 截断过长的摘要（安全处理 UTF-8 多字节字符）
                let truncated = match snippet.char_indices().nth(200) {
                    Some((idx, _)) => format!("{}...", &snippet[..idx]),
                    None => snippet.clone(),
                };
                summary.push_str(&format!("   {}\n", truncated));
            }
            summary.push_str(&format!("   Source: {}\n\n", result.url));
        }
    } else {
        summary.push_str("No results found.\n");
    }

    summary.push_str("\nPlease note that these are web search results and may not be fully accurate or up-to-date.");

    summary
}

/// 处理 WebSearch 请求
pub async fn handle_websearch_request(
    provider: std::sync::Arc<crate::kiro::provider::KiroProvider>,
    payload: &MessagesRequest,
    input_tokens: i32,
    stream_client: bool,
    sink: Option<&dyn crate::admin::trace_db::TraceSink>,
    group: Option<&str>,
) -> Response {
    // 1. 提取搜索查询
    let query = match extract_search_query(payload) {
        Some(q) => q,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse::new(
                    "invalid_request_error",
                    "无法从消息中提取搜索查询",
                )),
            )
                .into_response();
        }
    };

    tracing::info!(query_chars = query.chars().count(), "处理 WebSearch 请求");

    // 2. 创建 MCP 请求
    let (tool_use_id, mcp_request) = create_mcp_request(&query);

    // 3. max_uses=0 明确禁止搜索；本快速路径一次请求只会发起一次 MCP 调用。
    if payload
        .tools
        .as_ref()
        .and_then(|tools| tools.first())
        .and_then(|tool| tool.max_uses)
        .is_some_and(|max_uses| max_uses <= 0)
    {
        let error = WebSearchError {
            error_code: "max_uses_exceeded".to_string(),
            message: Some("web search max_uses must be at least 1".to_string()),
        };
        let body = create_websearch_error_body(&payload.model, &tool_use_id, &error, input_tokens);
        return (StatusCode::OK, Json(body)).into_response();
    }

    // 4. 调用 Kiro MCP API
    let search_results = match call_mcp_api(&provider, &mcp_request, sink, group).await {
        Ok(response) => match parse_search_results_checked(&response) {
            Ok(results) => Some(results),
            Err(error) => {
                if stream_client {
                    let body = create_websearch_error_body(
                        &payload.model,
                        &tool_use_id,
                        &error,
                        input_tokens,
                    );
                    return (StatusCode::OK, Json(body)).into_response();
                }
                let body =
                    create_websearch_error_body(&payload.model, &tool_use_id, &error, input_tokens);
                return (StatusCode::OK, Json(body)).into_response();
            }
        },
        Err(e) => {
            tracing::warn!("MCP API 调用失败: {}", e);
            let error = WebSearchError {
                error_code: "unavailable".to_string(),
                message: Some(e.to_string()),
            };
            let body =
                create_websearch_error_body(&payload.model, &tool_use_id, &error, input_tokens);
            return (StatusCode::OK, Json(body)).into_response();
        }
    };

    // 4. 按客户端 stream 形态返回响应；不能把非流请求强制变成 SSE。
    let model = payload.model.clone();
    if stream_client {
        let stream =
            create_websearch_sse_stream(model, query, tool_use_id, search_results, input_tokens);
        crate::common::sse::sse_response(Body::from_stream(stream))
    } else {
        let body =
            create_websearch_json_body(&model, &query, &tool_use_id, &search_results, input_tokens);
        (StatusCode::OK, Json(body)).into_response()
    }
}

/// 调用 Kiro MCP API
pub(crate) async fn call_mcp_api(
    provider: &crate::kiro::provider::KiroProvider,
    request: &McpRequest,
    sink: Option<&dyn crate::admin::trace_db::TraceSink>,
    group: Option<&str>,
) -> anyhow::Result<McpResponse> {
    let request_body = serde_json::to_string(request)?;

    tracing::debug!(request_bytes = request_body.len(), "MCP request prepared");

    let response = provider.call_mcp_traced(&request_body, sink, group).await?;

    let body = String::from_utf8(response.collect_bytes().await?.to_vec())?;
    tracing::debug!(response_bytes = body.len(), "MCP response received");

    let mcp_response: McpResponse = serde_json::from_str(&body)?;

    if let Some(ref error) = mcp_response.error {
        anyhow::bail!(
            "MCP error: {} - {}",
            error.code.unwrap_or(-1),
            error.message.as_deref().unwrap_or("Unknown error")
        );
    }

    Ok(mcp_response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_native_websearch_routes_through_model_loop() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(8),
                cache_control: None,
            }]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        assert!(has_web_search_among_tools(&req));
    }

    #[test]
    fn web_search_tool_choice_is_respected() {
        use crate::anthropic::types::{Message, Tool, ToolChoice};

        let mut req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![Tool {
                tool_type: Some("web_search_20250305".to_string()),
                name: "web_search".to_string(),
                description: String::new(),
                input_schema: Default::default(),
                max_uses: Some(1),
                cache_control: None,
            }]),
            tool_choice: Some(ToolChoice::None {
                disable_parallel_tool_use: false,
            }),
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert!(!has_web_search_among_tools(&req));
        req.tool_choice = Some(ToolChoice::Tool {
            name: "other".to_string(),
            disable_parallel_tool_use: false,
        });
        assert!(!has_web_search_among_tools(&req));
        req.tool_choice = Some(ToolChoice::Tool {
            name: "web_search".to_string(),
            disable_parallel_tool_use: false,
        });
        assert!(has_web_search_among_tools(&req));
    }

    #[test]
    fn non_stream_websearch_body_uses_anthropic_message_envelope() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Rust".to_string(),
                url: "https://www.rust-lang.org".to_string(),
                snippet: Some("safe systems language".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("rust".to_string()),
            error: None,
        };
        let body = create_websearch_json_body(
            "claude-opus-5",
            "rust latest version",
            "srvtoolu_test",
            &Some(results),
            12,
        );

        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert_eq!(body["stop_reason"], "end_turn");
        let content = body["content"].as_array().unwrap();
        assert_eq!(content[1]["type"], "server_tool_use");
        assert_eq!(content[1]["id"], "srvtoolu_test");
        assert_eq!(content[2]["type"], "web_search_tool_result");
        assert_eq!(content[2]["tool_use_id"], "srvtoolu_test");
        assert!(
            content
                .iter()
                .any(|block| { block["type"] == "text" && block["text"].as_str().is_some() })
        );
        assert_eq!(body["usage"]["server_tool_use"]["web_search_requests"], 1);
    }

    #[test]
    fn test_has_web_search_tool_multiple_tools() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: Some("web_search_20250305".to_string()),
                    name: "web_search".to_string(),
                    description: String::new(),
                    input_schema: Default::default(),
                    max_uses: Some(8),
                    cache_control: None,
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // 多个工具时同样走模型工具循环。
        assert!(has_web_search_among_tools(&req));
    }

    #[test]
    fn test_regular_tool_named_web_search_does_not_trigger_native_websearch() {
        use crate::anthropic::types::{Message, Tool};

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("test"),
            }],
            stream: true,
            system: None,
            tools: Some(vec![
                Tool {
                    tool_type: None,
                    name: "web_search".to_string(),
                    description: "Regular client-side search tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
                Tool {
                    tool_type: None,
                    name: "other_tool".to_string(),
                    description: "Other tool".to_string(),
                    input_schema: Default::default(),
                    max_uses: None,
                    cache_control: None,
                },
            ]),
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        // A regular client-defined tool can be named web_search, but only
        // Anthropic's native web search tool has type=web_search_*.
        assert!(!has_web_search_among_tools(&req));
    }

    #[test]
    fn test_extract_search_query_with_prefix() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!([{
                    "type": "text",
                    "text": "Perform a web search for the query: rust latest version 2026"
                }]),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        // 前缀应该被去除
        assert_eq!(query, Some("rust latest version 2026".to_string()));
    }

    #[test]
    fn test_extract_search_query_plain_text() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![Message {
                role: "user".to_string(),
                content: serde_json::json!("What is the weather today?"),
            }],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };

        let query = extract_search_query(&req);
        assert_eq!(query, Some("What is the weather today?".to_string()));
    }

    #[test]
    fn extract_search_query_uses_latest_user_text_block() {
        use crate::anthropic::types::Message;

        let req = MessagesRequest {
            force_web_search_loop: false,
            model: "claude-sonnet-4".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!("old question"),
                },
                Message {
                    role: "assistant".to_string(),
                    content: serde_json::json!("old answer"),
                },
                Message {
                    role: "user".to_string(),
                    content: serde_json::json!([
                        {"type": "image", "source": {"type": "base64"}},
                        {"type": "text", "text": "latest question"}
                    ]),
                },
            ],
            stream: true,
            system: None,
            tools: None,
            tool_choice: None,
            thinking: None,
            output_config: None,
            metadata: None,
        };
        assert_eq!(extract_search_query(&req), Some("latest question".to_string()));
    }

    #[test]
    fn web_search_stream_uses_input_delta_and_result_tool_id() {
        let events = generate_websearch_events(
            "claude-opus-5",
            "rust",
            "srvtoolu_test",
            None,
            1,
        );
        let start = events
            .iter()
            .find(|event| event.event == "content_block_start" && event.data["index"] == 1)
            .unwrap();
        assert!(start.data["content_block"]["input"].is_null());
        let delta = events
            .iter()
            .find(|event| event.event == "content_block_delta" && event.data["index"] == 1)
            .unwrap();
        assert_eq!(delta.data["delta"]["type"], "input_json_delta");
        let result = events
            .iter()
            .find(|event| event.event == "content_block_start" && event.data["index"] == 2)
            .unwrap();
        assert_eq!(result.data["content_block"]["tool_use_id"], "srvtoolu_test");
    }

    #[test]
    fn test_create_mcp_request() {
        let (tool_use_id, request) = create_mcp_request("test query");

        assert!(tool_use_id.starts_with("srvtoolu_"));
        assert_eq!(request.jsonrpc, "2.0");
        assert_eq!(request.method, "tools/call");
        assert_eq!(request.params.name, "web_search");
        assert_eq!(request.params.arguments.query, "test query");

        // 验证 ID 格式: web_search_tooluse_{22位}_{时间戳}_{8位}
        assert!(request.id.starts_with("web_search_tooluse_"));
    }

    #[test]
    fn test_mcp_request_id_format() {
        let (_, request) = create_mcp_request("test");

        // 格式: web_search_tooluse_{22位}_{毫秒时间戳}_{8位}
        let id = &request.id;
        assert!(id.starts_with("web_search_tooluse_"));

        let suffix = &id["web_search_tooluse_".len()..];
        let parts: Vec<&str> = suffix.split('_').collect();
        assert_eq!(parts.len(), 3, "应该有3个部分: 22位随机_时间戳_8位随机");

        // 第一部分: 22位大小写字母和数字
        assert_eq!(parts[0].len(), 22);
        assert!(parts[0].chars().all(|c| c.is_ascii_alphanumeric()));

        // 第二部分: 毫秒时间戳
        assert!(parts[1].parse::<i64>().is_ok());

        // 第三部分: 8位小写字母和数字
        assert_eq!(parts[2].len(), 8);
        assert!(
            parts[2]
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        );
    }

    #[test]
    fn test_parse_search_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"results":[{"title":"Test","url":"https://example.com","snippet":"Test snippet"}],"totalResults":1}"#.to_string(),
                }],
                is_error: false,
            }),
        };

        let results = parse_search_results_checked(&response).unwrap();
        assert_eq!(results.results.len(), 1);
        assert_eq!(results.results[0].title, "Test");
    }

    #[test]
    fn search_errors_are_not_silently_mapped_to_empty_results() {
        let response = McpResponse {
            error: None,
            id: "test_id".to_string(),
            jsonrpc: "2.0".to_string(),
            result: Some(McpResult {
                content: vec![McpContent {
                    content_type: "text".to_string(),
                    text: r#"{"type":"web_search_tool_result_error","error_code":"max_uses_exceeded"}"#.to_string(),
                }],
                is_error: true,
            }),
        };

        let error = parse_search_results_checked(&response).unwrap_err();
        assert_eq!(error.error_code, "max_uses_exceeded");
    }

    #[test]
    fn test_generate_search_summary() {
        let results = WebSearchResults {
            results: vec![WebSearchResult {
                title: "Test Result".to_string(),
                url: "https://example.com".to_string(),
                snippet: Some("This is a test snippet".to_string()),
                published_date: None,
                id: None,
                domain: None,
                max_verbatim_word_limit: None,
                public_domain: None,
            }],
            total_results: Some(1),
            query: Some("test".to_string()),
            error: None,
        };

        let summary = generate_search_summary("test", &Some(results));

        assert!(summary.contains("Test Result"));
        assert!(summary.contains("https://example.com"));
        assert!(summary.contains("This is a test snippet"));
    }
}
