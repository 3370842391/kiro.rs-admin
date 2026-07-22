//! Admin API 路由配置

use axum::{
    Router, middleware,
    routing::{delete, get, post, put},
};

use super::{
    handlers::{
        add_credential, add_proxy, apply_image_update, apply_model_profile_preview,
        assign_proxies_round_robin, assign_proxy_to_credential, batch_add_proxies,
        batch_import_credentials, batch_update_credentials, cancel_idc_login, cancel_social_login,
        check_all_proxies, check_proxy, check_proxy_url, check_rate_limit, check_update,
        cleanup_error_snapshots, clear_cache_policy_entries, clear_throttle, clear_traces,
        complete_social_login, complete_social_relogin, create_client_key, create_group,
        delete_client_key, delete_credential, delete_error_snapshot, delete_group,
        delete_model_mapping, delete_model_profile_entry, delete_proxy, disable_quota_exceeded,
        download_error_snapshot, enable_overage_all, error_snapshot_storage, export_credentials,
        fetch_model_profile, force_refresh_token, get_account_throttle_config, get_all_credentials,
        get_cache_hit_rate, get_cache_policy, get_compatibility_config, get_credential_balance,
        get_credential_models, get_endpoint_chains, get_endpoint_mode, get_error_snapshot,
        get_error_snapshot_payload, get_global_proxy, get_image_budget, get_load_balancing_mode,
        get_log_governance_config, get_model_profiles, get_proxy_balancing_mode, get_proxy_pool,
        get_retry_policy, get_update_config, list_client_keys, list_error_snapshots, list_groups,
        list_model_mappings, list_traces, patch_model_profile, pin_error_snapshot, poll_idc_login,
        poll_idc_relogin, poll_social_login, poll_social_relogin, preview_model_profiles,
        pull_update_image, replace_model_mappings, reset_all_success_count, reset_client_key_stats,
        reset_failure_count, reset_success_count, rollback_image_update, rotate_client_key,
        set_account_throttle_config, set_cache_hit_rate, set_cache_policy, set_client_key_disabled,
        set_compatibility_config, set_credential_disabled, set_credential_overage,
        set_credential_priority, set_endpoint_chains, set_endpoint_mode, set_global_proxy,
        set_image_budget, set_load_balancing_mode, set_log_governance_config,
        set_model_profile_settings, set_proxy_balancing_mode, set_proxy_enabled, set_retry_policy,
        set_update_config, start_idc_login, start_idc_relogin, start_social_login,
        start_social_relogin, stats_by_credential, stats_by_model, stats_overview,
        stats_timeseries, sync_model_profiles, test_credential_response, trace_failure_stats,
        unpin_error_snapshot, update_admin_key, update_client_key, update_credential, update_group,
        update_refresh_token, upsert_model_mapping,
    },
    middleware::{AdminState, admin_auth_middleware},
};

/// 创建 Admin API 路由
///
/// # 端点
/// - `GET /credentials` - 获取所有凭据状态
/// - `POST /credentials` - 添加新凭据
/// - `PUT /credentials/batch` - 批量更新凭据 RPM、分组与来源渠道
/// - `DELETE /credentials/:id` - 删除凭据
/// - `PUT /credentials/:id` - 更新凭据可编辑字段（email、proxy 等）
/// - `POST /credentials/:id/disabled` - 设置凭据禁用状态
/// - `POST /credentials/:id/priority` - 设置凭据优先级
/// - `POST /credentials/:id/reset` - 重置失败计数
/// - `POST /credentials/:id/refresh` - 强制刷新 Token
/// - `GET /credentials/:id/balance` - 获取凭据余额
/// - `GET /config/load-balancing` - 获取负载均衡模式
/// - `PUT /config/load-balancing` - 设置负载均衡模式
///
/// # 认证
/// 需要登录API密钥认证，支持：
/// - `x-api-key` header
/// - `Authorization: Bearer <token>` header
pub fn create_admin_router(state: AdminState) -> Router {
    // 需要登录API密钥认证的路由
    let authenticated = Router::new()
        .route("/model-profiles", get(get_model_profiles))
        .route(
            "/model-profiles/{model_id}",
            axum::routing::patch(patch_model_profile).delete(delete_model_profile_entry),
        )
        .route(
            "/model-profiles/{model_id}/fetch",
            post(fetch_model_profile),
        )
        .route("/model-profiles/sync", post(sync_model_profiles))
        .route("/model-profiles/preview", post(preview_model_profiles))
        .route("/model-profiles/apply", post(apply_model_profile_preview))
        .route("/model-profiles/settings", put(set_model_profile_settings))
        .route(
            "/credentials",
            get(get_all_credentials).post(add_credential),
        )
        .route("/credentials/export", get(export_credentials))
        .route("/credentials/batch", put(batch_update_credentials))
        .route(
            "/credentials/{id}",
            delete(delete_credential).put(update_credential),
        )
        .route("/credentials/{id}/disabled", post(set_credential_disabled))
        .route("/credentials/{id}/priority", post(set_credential_priority))
        .route("/credentials/{id}/reset", post(reset_failure_count))
        .route("/credentials/{id}/clear-throttle", post(clear_throttle))
        .route("/credentials/{id}/reset-stats", post(reset_success_count))
        .route("/credentials/reset-stats", post(reset_all_success_count))
        .route("/credentials/batch-import", post(batch_import_credentials))
        .route(
            "/credentials/disable-quota-exceeded",
            post(disable_quota_exceeded),
        )
        .route("/credentials/overage/enable-all", post(enable_overage_all))
        .route("/credentials/{id}/overage", post(set_credential_overage))
        .route("/credentials/{id}/refresh", post(force_refresh_token))
        .route("/credentials/{id}/refresh-token", put(update_refresh_token))
        .route("/credentials/{id}/balance", get(get_credential_balance))
        .route("/credentials/{id}/models", get(get_credential_models))
        .route("/credentials/{id}/test", post(test_credential_response))
        .route("/credentials/{id}/proxy", post(assign_proxy_to_credential))
        .route("/proxy-pool", get(get_proxy_pool).post(add_proxy))
        .route("/proxy-pool/batch", post(batch_add_proxies))
        .route("/proxy-pool/check-url", post(check_proxy_url))
        .route("/proxy-pool/check-all", post(check_all_proxies))
        .route(
            "/proxy-pool/assign-round-robin",
            post(assign_proxies_round_robin),
        )
        .route("/proxy-pool/{id}", delete(delete_proxy))
        .route("/proxy-pool/{id}/enabled", post(set_proxy_enabled))
        .route("/proxy-pool/{id}/check", post(check_proxy))
        .route(
            "/config/load-balancing",
            get(get_load_balancing_mode).put(set_load_balancing_mode),
        )
        .route(
            "/config/proxy-balancing",
            get(get_proxy_balancing_mode).put(set_proxy_balancing_mode),
        )
        .route(
            "/config/account-throttle",
            get(get_account_throttle_config).put(set_account_throttle_config),
        )
        .route(
            "/config/compatibility",
            get(get_compatibility_config).put(set_compatibility_config),
        )
        .route(
            "/config/retry-policy",
            get(get_retry_policy).put(set_retry_policy),
        )
        .route(
            "/config/endpoint-chains",
            get(get_endpoint_chains).put(set_endpoint_chains),
        )
        .route(
            "/config/endpoint-mode",
            get(get_endpoint_mode).put(set_endpoint_mode),
        )
        .route(
            "/config/cache-hit-rate",
            get(get_cache_hit_rate).put(set_cache_hit_rate),
        )
        .route(
            "/config/image-budget",
            get(get_image_budget).put(set_image_budget),
        )
        .route(
            "/config/cache-policy",
            get(get_cache_policy).put(set_cache_policy),
        )
        .route(
            "/config/cache-policy/clear",
            post(clear_cache_policy_entries),
        )
        .route(
            "/config/log-governance",
            get(get_log_governance_config).put(set_log_governance_config),
        )
        .route(
            "/config/global-proxy",
            get(get_global_proxy).put(set_global_proxy),
        )
        .route(
            "/config/update",
            get(get_update_config).put(set_update_config),
        )
        .route("/config/admin-key", put(update_admin_key))
        .route("/system/update/pull", post(pull_update_image))
        .route("/system/update/apply", post(apply_image_update))
        .route("/system/update/rollback", post(rollback_image_update))
        .route("/system/update/check", get(check_update))
        .route("/system/update/rate-limit", post(check_rate_limit))
        .route("/auth/idc/start", post(start_idc_login))
        .route("/auth/idc/poll/{session_id}", post(poll_idc_login))
        .route("/auth/idc/{session_id}", delete(cancel_idc_login))
        .route("/auth/social/start", post(start_social_login))
        .route("/auth/social/poll/{session_id}", post(poll_social_login))
        .route("/auth/social/{session_id}", delete(cancel_social_login))
        .route(
            "/auth/social/complete/{session_id}",
            post(complete_social_login),
        )
        .route(
            "/credentials/{id}/relogin/social/start",
            post(start_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/social/poll/{session_id}",
            post(poll_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/social/complete/{session_id}",
            post(complete_social_relogin),
        )
        .route(
            "/credentials/{id}/relogin/idc/start",
            post(start_idc_relogin),
        )
        .route(
            "/credentials/{id}/relogin/idc/poll/{session_id}",
            post(poll_idc_relogin),
        )
        .route(
            "/client-keys",
            get(list_client_keys).post(create_client_key),
        )
        .route(
            "/client-keys/{id}",
            delete(delete_client_key).put(update_client_key),
        )
        .route("/client-keys/{id}/disabled", post(set_client_key_disabled))
        .route(
            "/client-keys/{id}/reset-stats",
            post(reset_client_key_stats),
        )
        .route("/client-keys/{id}/rotate", post(rotate_client_key))
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{name}", delete(delete_group).patch(update_group))
        .route(
            "/model-mappings",
            get(list_model_mappings)
                .post(upsert_model_mapping)
                .put(replace_model_mappings),
        )
        .route("/model-mappings/{source}", delete(delete_model_mapping))
        .route("/stats/overview", get(stats_overview))
        .route("/stats/timeseries", get(stats_timeseries))
        .route("/stats/by-model", get(stats_by_model))
        .route("/stats/by-credential", get(stats_by_credential))
        .route("/traces/failure-stats", get(trace_failure_stats))
        .route("/traces", get(list_traces).delete(clear_traces))
        .route("/error-snapshots", get(list_error_snapshots))
        .route("/error-snapshots/storage", get(error_snapshot_storage))
        .route("/error-snapshots/cleanup", post(cleanup_error_snapshots))
        .route(
            "/error-snapshots/{id}",
            get(get_error_snapshot).delete(delete_error_snapshot),
        )
        .route(
            "/error-snapshots/{id}/payload/{seq}",
            get(get_error_snapshot_payload),
        )
        .route(
            "/error-snapshots/{id}/download",
            get(download_error_snapshot),
        )
        .route("/error-snapshots/{id}/pin", post(pin_error_snapshot))
        .route("/error-snapshots/{id}/unpin", post(unpin_error_snapshot))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            admin_auth_middleware,
        ));

    Router::new().merge(authenticated).with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use crate::{
        admin::{
            AdminService, ClientKeyManager, ErrorSnapshotStore, GroupManager, ModelMappingManager,
            TraceStore, UsageAggregator, error_snapshot_db::ErrorSnapshotPolicy,
            proxy_pool::ProxyPoolManager,
        },
        kiro::{model::credentials::KiroCredentials, token_manager::MultiTokenManager},
        model::config::{Config, TlsBackend},
    };

    fn batch_update_test_router() -> Router {
        let credentials = vec![KiroCredentials {
            id: Some(1),
            rpm_limit: 10,
            ..Default::default()
        }];
        let token_manager = Arc::new(
            MultiTokenManager::new(Config::default(), credentials, None, None, true).unwrap(),
        );
        let service = AdminService::new(
            token_manager,
            Vec::new(),
            Arc::new(ProxyPoolManager::new(None, TlsBackend::Rustls)),
        );
        let config = Config::default();
        let state = AdminState::new(
            "test-admin-key",
            service,
            Arc::new(ClientKeyManager::new()),
            Arc::new(UsageAggregator::new()),
            Arc::new(TraceStore::open_in_memory().unwrap()),
            Arc::new(
                ErrorSnapshotStore::open_in_memory(ErrorSnapshotPolicy::from_config(&config))
                    .unwrap(),
            ),
            Arc::new(GroupManager::new()),
            Arc::new(ModelMappingManager::new()),
        );

        create_admin_router(state)
    }

    #[tokio::test]
    async fn batch_update_credentials_route_returns_updated_summary() {
        let response = batch_update_test_router()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/credentials/batch")
                    .header("x-api-key", "test-admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"ids":[1],"rpmLimit":4}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["selected"], 1);
        assert_eq!(json["updated"], 1);
        assert_eq!(json["rpmSummary"]["limitedCapacity"], 4);
    }

    #[tokio::test]
    async fn batch_update_credentials_route_rejects_missing_or_invalid_admin_key() {
        for admin_key in [None, Some("wrong-admin-key")] {
            let mut request = Request::builder()
                .method("PUT")
                .uri("/credentials/batch")
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(admin_key) = admin_key {
                request = request.header("x-api-key", admin_key);
            }

            let response = batch_update_test_router()
                .oneshot(
                    request
                        .body(Body::from(r#"{"ids":[1],"rpmLimit":4}"#))
                        .unwrap(),
                )
                .await
                .unwrap();

            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }
    }

    #[tokio::test]
    async fn compatibility_config_route_reads_and_updates_empty_user_message_flag() {
        let app = batch_update_test_router();
        let get = || {
            Request::builder()
                .method("GET")
                .uri("/config/compatibility")
                .header("x-api-key", "test-admin-key")
                .body(Body::empty())
                .unwrap()
        };

        let response = app.clone().oneshot(get()).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["emptyUserMessageCompat"], false);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/config/compatibility")
                    .header("x-api-key", "test-admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"emptyUserMessageCompat":true}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let response = app.oneshot(get()).await.unwrap();
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["emptyUserMessageCompat"], true);
    }

    #[tokio::test]
    async fn cancel_auth_sessions_require_admin_key_and_are_idempotent() {
        for path in ["/auth/idc/missing", "/auth/social/missing"] {
            let unauthorized = batch_update_test_router()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

            let authorized = batch_update_test_router()
                .oneshot(
                    Request::builder()
                        .method("DELETE")
                        .uri(path)
                        .header("x-api-key", "test-admin-key")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(authorized.status(), StatusCode::OK);
            let body = to_bytes(authorized.into_body(), usize::MAX).await.unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&body).unwrap()["cancelled"],
                false
            );
        }
    }
}
