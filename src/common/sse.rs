//! SSE 响应构造
//!
//! 所有流式端点共用同一组响应头，避免「主路径设对了、旁路端点漏了」这种只在客户
//! 侧反代后面才暴露的问题：
//!
//! - `x-accel-buffering: no` —— nginx 收到后关闭 proxy_buffering，字节到即转发；
//!   漏掉时 nginx 会等 `proxy_buffer_size` 填满或上游结束才吐第一个字节。
//! - `no-transform` —— 禁止中间层压缩/改写响应体，压缩会天然攒包。
//! - 响应头一旦构造完成 axum 就会立刻下发，不等第一个 body chunk。
//!
//! 本地直连测不出这两个头的缺失，所以统一从这里构造，不在各 handler 里手写。

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

/// 构造 200 的 SSE 流式响应。
pub fn sse_response(body: Body) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache, no-transform")
        .header("x-accel-buffering", "no")
        .header(header::CONNECTION, "keep-alive")
        .body(body)
        .expect("SSE 响应头为静态常量，构造不会失败")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 反代穿透三件套缺任何一项都会让客户端看到「整段涌出」，且本地直连测不出来。
    #[test]
    fn sse_response_carries_proxy_passthrough_headers() {
        let response = sse_response(Body::empty());

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/event-stream");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "no-cache, no-transform"
        );
        assert_eq!(response.headers()["x-accel-buffering"], "no");
        assert_eq!(response.headers()[header::CONNECTION], "keep-alive");
        // 流式响应不能带 Content-Length，否则下游会等满长度。
        assert!(response.headers().get(header::CONTENT_LENGTH).is_none());
    }
}
