//! Anthropic API 路由配置
use std::sync::Arc;

use axum::{
    Router,
    extract::DefaultBodyLimit,
    middleware,
    routing::{get, post},
};

use crate::admin::client_keys::SharedClientKeyManager;
use crate::admin::error_snapshot_db::SharedErrorSnapshotStore;
use crate::admin::trace_db::SharedTraceStore;
use crate::admin::usage_stats::{SharedAggregator, SharedRecorder};
use crate::kiro::provider::KiroProvider;
use crate::model::config::ToolCompatibilityMode;

use super::{
    cache_metering::SharedCacheMeter,
    handlers::{count_tokens, get_models, post_messages, post_messages_cc},
    middleware::{AppState, auth_middleware, cors_layer},
};

/// 请求体最大大小限制 (50MB)
const MAX_BODY_SIZE: usize = 50 * 1024 * 1024;

/// 创建带有 KiroProvider 的 Anthropic API 路由
///
/// 给嵌入到其他 Rust 项目的下游使用者预留的扩展点。
#[allow(dead_code)]
pub fn create_router_with_provider(
    kiro_provider: Option<KiroProvider>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
) -> Router {
    create_router(
        kiro_provider.map(Arc::new),
        extract_thinking,
        tool_compatibility_mode,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    )
}

/// 创建 Anthropic API 路由（供 main.rs 使用）
#[allow(clippy::too_many_arguments)]
pub fn create_router(
    kiro_provider: Option<Arc<KiroProvider>>,
    extract_thinking: bool,
    tool_compatibility_mode: ToolCompatibilityMode,
    client_keys: Option<SharedClientKeyManager>,
    usage_recorder: Option<SharedRecorder>,
    usage_aggregator: Option<SharedAggregator>,
    cache_meter: Option<SharedCacheMeter>,
    trace_store: Option<SharedTraceStore>,
    error_snapshot_store: Option<SharedErrorSnapshotStore>,
    model_mappings: Option<crate::admin::SharedModelMappingManager>,
    model_profiles: Option<Arc<super::model_profile::ModelProfileStore>>,
) -> Router {
    let mut state = AppState::new(extract_thinking, tool_compatibility_mode);
    if let Some(provider) = kiro_provider {
        state = state.with_kiro_provider(provider);
    }
    state = state.with_usage(client_keys, usage_recorder, usage_aggregator);
    state = state.with_cache_meter(cache_meter);
    state = state.with_trace_store(trace_store);
    state = state.with_error_snapshot_store(error_snapshot_store);
    state = state.with_model_mappings(model_mappings);
    state = state.with_model_profiles(model_profiles);

    // 需要认证的 /v1 路由
    let v1_routes = Router::new()
        .route("/models", get(get_models))
        .route("/messages", post(post_messages))
        .route("/messages/count_tokens", post(count_tokens))
        .route(
            "/chat/completions",
            post(crate::openai::handlers::post_chat_completions),
        )
        .route("/responses", post(crate::openai::handlers::post_responses))
        .route(
            "/responses/{id}",
            get(crate::openai::handlers::get_response)
                .delete(crate::openai::handlers::delete_response),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    // 需要认证的 /cc/v1 路由（Claude Code 兼容端点）
    // 与 /v1 的区别：流式响应会等待 contextUsageEvent 后再发送 message_start
    let cc_v1_routes = Router::new()
        .route("/messages", post(post_messages_cc))
        .route("/messages/count_tokens", post(count_tokens))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth_middleware,
        ));

    Router::new()
        .nest("/v1", v1_routes)
        .nest("/cc/v1", cc_v1_routes)
        .layer(cors_layer())
        .layer(DefaultBodyLimit::max(MAX_BODY_SIZE))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn empty_user_message_returns_400_before_missing_provider_for_both_modes() {
        let keys = Arc::new(crate::admin::ClientKeyManager::new());
        keys.create_with_key(
            "test".to_string(),
            None,
            None,
            "csk_test-client-key".to_string(),
        );
        let app = create_router(
            None,
            true,
            ToolCompatibilityMode::ClaudeCode,
            Some(keys),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        );

        for stream in [false, true] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/messages")
                        .header("x-api-key", "csk_test-client-key")
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(
                            serde_json::json!({
                                "model": "claude-opus-4-8",
                                "max_tokens": 64,
                                "stream": stream,
                                "system": "Keep answers concise.",
                                "messages": [{"role": "user", "content": ""}]
                            })
                            .to_string(),
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(json["error"]["type"], "invalid_request_error");
            assert_eq!(
                json["error"]["message"],
                super::super::handlers::EMPTY_USER_MESSAGE_ERROR
            );
        }
    }
}
