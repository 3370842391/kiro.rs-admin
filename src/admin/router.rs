//! Admin API 路由配置

use std::sync::Arc;

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
        get_log_governance_config, get_model_profiles, get_profit_config, get_proxy_balancing_mode,
        get_proxy_pool, get_retry_policy, get_update_config, list_client_keys,
        list_error_snapshots, list_groups, list_model_mappings, list_traces, patch_model_profile,
        pin_error_snapshot, poll_idc_login, poll_idc_relogin, poll_social_login,
        poll_social_relogin, preview_model_profiles, profit_report, pull_update_image,
        replace_model_mappings, reset_all_success_count, reset_client_key_stats,
        reset_failure_count, reset_success_count, rollback_image_update, rotate_client_key,
        set_account_throttle_config, set_cache_hit_rate, set_cache_policy, set_client_key_disabled,
        set_compatibility_config, set_credential_disabled, set_credential_overage,
        set_credential_priority, set_endpoint_chains, set_endpoint_mode, set_global_proxy,
        set_image_budget, set_load_balancing_mode, set_log_governance_config,
        set_model_profile_settings, set_profit_config, set_proxy_balancing_mode, set_proxy_enabled,
        set_retry_policy, set_update_config, start_idc_login, start_idc_relogin,
        start_social_login, start_social_relogin, stats_by_credential, stats_by_model,
        stats_overview, stats_timeseries, sync_model_profiles, test_credential_response,
        trace_failure_stats, unpin_error_snapshot, update_admin_key, update_client_key,
        update_credential, update_group, update_refresh_token, upsert_model_mapping,
    },
    key_supplier::handlers::{
        get_config as get_key_supplier_config, list_events as list_key_supplier_events,
        mark_events_read, overview as key_supplier_overview, purchase as key_supplier_purchase,
        put_config as put_key_supplier_config, register_webhook as register_key_supplier_webhook,
        retry_event as retry_key_supplier_event, test_webhook as test_key_supplier_webhook,
        webhook_router,
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
            "/config/profit",
            get(get_profit_config).put(set_profit_config),
        )
        .route("/profit/report", post(profit_report))
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
        .route(
            "/config/key-supplier",
            get(get_key_supplier_config).put(put_key_supplier_config),
        )
        .route("/key-supplier/overview", get(key_supplier_overview))
        .route("/key-supplier/purchase", post(key_supplier_purchase))
        .route(
            "/key-supplier/webhook/register",
            post(register_key_supplier_webhook),
        )
        .route(
            "/key-supplier/webhook/test",
            post(test_key_supplier_webhook),
        )
        .route("/key-supplier/events", get(list_key_supplier_events))
        .route("/key-supplier/events/read", post(mark_events_read))
        .route(
            "/key-supplier/events/{id}/retry",
            post(retry_key_supplier_event),
        )
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

    authenticated
        .with_state(state.clone())
        .merge(webhook_router(state.key_supplier.clone()))
}

/// 公开 webhook 子路由。用于未配置 Admin API Key 时仍可接收供应商回调。
pub fn create_key_supplier_webhook_router(
    service: Option<Arc<super::key_supplier::service::KeySupplierService>>,
) -> Router {
    webhook_router(service)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::{future::Future, pin::Pin};

    use axum::{
        Json,
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use crate::{
        admin::{
            AdminService, ClientKeyManager, ErrorSnapshotStore, GroupManager, ModelMappingManager,
            TraceStore, UsageAggregator,
            error_snapshot_db::ErrorSnapshotPolicy,
            key_supplier::{
                config::SupplierRuntimeConfig,
                service::{CredentialImporter, KeySupplierService},
                store::{IncomingSupplierEvent, SupplierEventStore},
            },
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
            None,
        );

        create_admin_router(state)
    }

    fn key_supplier_test_app() -> (Router, String, Arc<KeySupplierService>) {
        let token = "a".repeat(64);
        let runtime = SupplierRuntimeConfig {
            base_url: String::new(),
            api_key: "supplier-api-key-canary".to_string(),
            public_base_url: "https://public.example".to_string(),
            webhook_token: token.clone(),
            auto_purchase: false,
            min_purchase: 1,
            max_purchase: 10,
            api_region: "us-east-1".to_string(),
            rpm_limit: 0,
            priority: 0,
            groups: Vec::new(),
            source_channel: "supplier".to_string(),
            nickname_prefix: "supplier".to_string(),
        };
        let supplier = Arc::new(KeySupplierService::new(
            Arc::new(SupplierEventStore::open_in_memory().unwrap()),
            runtime,
        ));

        let app = key_supplier_admin_app(supplier.clone());
        (app, token, supplier)
    }

    fn key_supplier_admin_app(supplier: Arc<KeySupplierService>) -> Router {
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
            Some(supplier.clone()),
        );
        Router::new().nest("/api/admin", create_admin_router(state))
    }

    struct AcceptingImporter;

    impl CredentialImporter for AcceptingImporter {
        fn import(
            &self,
            _: KiroCredentials,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
            Box::pin(async { Ok(()) })
        }
    }

    async fn purchase_supplier_server() -> String {
        let app = Router::new().route(
            "/api/my/purchase",
            post(|Json(request): Json<serde_json::Value>| async move {
                Json(serde_json::json!({
                    "client_order_id": request["client_order_id"],
                    "purchased": 1,
                    "remaining": 4,
                    "keys": [{"key": "ksk_purchase_canary"}],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{address}")
    }

    fn webhook_body(event_id: &str) -> String {
        format!(
            r#"{{"event":"new_keys_available","event_id":"{event_id}","purchase_order_id":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","message":"available","new_keys":1}}"#
        )
    }

    #[tokio::test]
    async fn key_supplier_router_webhook_is_public_and_deduplicates() {
        let (app, token, _) = key_supplier_test_app();
        let path = format!("/api/admin/key-supplier/webhook/{token}");
        let body = webhook_body("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        let accepted = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::CONTENT_TYPE, "application/json; charset=utf-8")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::ACCEPTED);

        let duplicate = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn key_supplier_router_rejects_invalid_webhook_requests() {
        let (app, token, _) = key_supplier_test_app();
        let path = format!("/api/admin/key-supplier/webhook/{token}");

        let wrong_token = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/key-supplier/webhook/not-the-real-token")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(webhook_body("cccccccccccccccccccccccccccccccc")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            wrong_token.status(),
            StatusCode::UNAUTHORIZED | StatusCode::NOT_FOUND
        ));

        let malformed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);

        let wrong_content_type = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(&path)
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from(webhook_body("dddddddddddddddddddddddddddddddd")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            wrong_content_type.status(),
            StatusCode::UNSUPPORTED_MEDIA_TYPE
        );

        let too_large = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(vec![b'x'; 64 * 1024 + 1]))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(too_large.status(), StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn key_supplier_admin_routes_require_admin_key() {
        let (app, _, _) = key_supplier_test_app();
        for (method, path) in [
            ("GET", "/api/admin/config/key-supplier"),
            ("PUT", "/api/admin/config/key-supplier"),
            ("GET", "/api/admin/key-supplier/overview"),
            ("POST", "/api/admin/key-supplier/purchase"),
            ("POST", "/api/admin/key-supplier/webhook/register"),
            ("POST", "/api/admin/key-supplier/webhook/test"),
            ("GET", "/api/admin/key-supplier/events"),
            ("POST", "/api/admin/key-supplier/events/read"),
            ("POST", "/api/admin/key-supplier/events/1/retry"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::UNAUTHORIZED,
                "{method} {path}"
            );
        }
    }

    #[tokio::test]
    async fn key_supplier_admin_routes_map_config_events_read_retry_and_purchase() {
        let (app, token, supplier) = key_supplier_test_app();
        let authorized = |method: &str, path: &str, body: Body| {
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-api-key", "test-admin-key")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body)
                .unwrap()
        };

        let config = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/api/admin/config/key-supplier",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(config.status(), StatusCode::OK);
        let config_body = to_bytes(config.into_body(), usize::MAX).await.unwrap();
        let config_json: serde_json::Value = serde_json::from_slice(&config_body).unwrap();
        assert_eq!(config_json["apiKeyConfigured"], true);
        assert!(config_json.get("apiKey").is_none());
        assert!(config_json.get("webhookToken").is_none());

        let events = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/api/admin/key-supplier/events?limit=1",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(events.status(), StatusCode::OK);

        let webhook = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/api/admin/key-supplier/webhook/{token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(webhook_body("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(webhook.status(), StatusCode::ACCEPTED);

        let events = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/api/admin/key-supplier/events?limit=1",
                Body::empty(),
            ))
            .await
            .unwrap();
        let events_body = to_bytes(events.into_body(), usize::MAX).await.unwrap();
        let event_id = serde_json::from_slice::<serde_json::Value>(&events_body).unwrap()["items"]
            [0]["id"]
            .as_i64()
            .unwrap();
        let read = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/api/admin/key-supplier/events/read",
                Body::from(format!(r#"{{"ids":[{event_id}],"markAll":false}}"#)),
            ))
            .await
            .unwrap();
        assert_eq!(read.status(), StatusCode::OK);

        let failed = supplier
            .store()
            .insert_event(IncomingSupplierEvent {
                event_id: "ffffffffffffffffffffffffffffffff".to_string(),
                event_type: "new_keys_available".to_string(),
                purchase_order_id: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
                message: None,
                quantity: 1,
            })
            .unwrap();
        let failed_id = match failed {
            crate::admin::key_supplier::store::InsertOutcome::Inserted(event) => event.id,
            crate::admin::key_supplier::store::InsertOutcome::Duplicate(_) => unreachable!(),
        };
        let claimed = supplier
            .store()
            .claim_by_event_id("ffffffffffffffffffffffffffffffff")
            .unwrap()
            .unwrap();
        supplier
            .store()
            .fail(claimed.id, "temporary failure")
            .unwrap();
        assert_eq!(claimed.id, failed_id);
        let retry = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/api/admin/key-supplier/events/{failed_id}/retry"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);

        let invalid_read = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/api/admin/key-supplier/events/read",
                Body::from(r#"{"ids":[],"markAll":false}"#),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_read.status(), StatusCode::BAD_REQUEST);

        let invalid_retry = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/api/admin/key-supplier/events/999/retry",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_retry.status(), StatusCode::NOT_FOUND);

        let invalid_purchase = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/api/admin/key-supplier/purchase",
                Body::from(r#"{"count":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_purchase.status(), StatusCode::BAD_REQUEST);

        let invalid_config = app
            .clone()
            .oneshot(authorized(
                "PUT",
                "/api/admin/config/key-supplier",
                Body::from(r#"{"baseUrl":"","publicBaseUrl":"","autoPurchase":false,"minPurchase":1,"maxPurchase":10,"apiRegion":"us-east-1","rpmLimit":0,"priority":0,"groups":[],"sourceChannel":"supplier","nicknamePrefix":"supplier"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_config.status(), StatusCode::SERVICE_UNAVAILABLE);

        let invalid_events = app
            .oneshot(authorized(
                "GET",
                "/api/admin/key-supplier/events?limit=0",
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_events.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn key_supplier_purchase_route_returns_summary_without_supplier_keys() {
        let token = "c".repeat(64);
        let store = Arc::new(SupplierEventStore::open_in_memory().unwrap());
        let service = Arc::new(KeySupplierService::with_importer(
            store,
            SupplierRuntimeConfig {
                base_url: purchase_supplier_server().await,
                api_key: "supplier-api-key-canary".to_string(),
                public_base_url: "https://public.example".to_string(),
                webhook_token: token,
                auto_purchase: false,
                min_purchase: 1,
                max_purchase: 10,
                api_region: "us-east-1".to_string(),
                rpm_limit: 0,
                priority: 0,
                groups: Vec::new(),
                source_channel: "supplier".to_string(),
                nickname_prefix: "supplier".to_string(),
            },
            Arc::new(AcceptingImporter),
        ));
        let response = key_supplier_admin_app(service)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/admin/key-supplier/purchase")
                    .header("x-api-key", "test-admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"count":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let encoded = String::from_utf8(body.to_vec()).unwrap();
        assert!(encoded.contains("\"purchased\":1"));
        assert!(!encoded.contains("ksk_purchase_canary"));
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
    async fn profit_report_route_rejects_out_of_range_minutes() {
        let response = batch_update_test_router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/profit/report")
                    .header("x-api-key", "test-admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"minutes":0}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn profit_report_route_rejects_missing_newapi_settings() {
        let response = batch_update_test_router()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/profit/report")
                    .header("x-api-key", "test-admin-key")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"minutes":30}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
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
