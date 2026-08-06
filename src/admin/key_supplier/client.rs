#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        routing::{get, post, put},
    };
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn server(router: Router) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(axum::serve(listener, router).into_future());
        format!("http://{address}/")
    }

    #[tokio::test]
    async fn reads_profile_stock_and_status_with_authentication() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_clone = seen.clone();
        let app = Router::new()
            .route("/api/my/profile", get(move |request: axum::http::Request<axum::body::Body>| {
                let seen = seen_clone.clone();
                async move {
                    assert_eq!(
                        request.headers().get("user-agent").unwrap(),
                        "kiro-rs-key-supplier/1.0"
                    );
                    seen.lock().unwrap().push((request.uri().path().to_owned(), request.headers().get("x-api-key").unwrap().to_str().unwrap().to_owned(), request.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned())));
                    axum::Json(serde_json::json!({"name":"demo","quota":10,"remaining":7,"used_quota":3,"webhook_url":"http://hook"}))
                }
            }))
            .route(
                "/api/my/stock",
                get(|request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                    axum::Json(serde_json::json!({"max": 9}))
                }),
            )
            .route(
                "/api/status",
                get(|request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                    axum::Json(serde_json::json!({"keys_active":2,"extra_state":"kept"}))
                }),
            );
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        assert_eq!(client.profile().await.unwrap().remaining, 7);
        assert_eq!(client.stock().await.unwrap().max, 9);
        let status = client.status().await.unwrap();
        assert_eq!(status.keys_active, 2);
        assert_eq!(status.keys_dead, 0);
        assert_eq!(status.extra["extra_state"], "kept");
        assert_eq!(seen.lock().unwrap()[0].1, "secret");
    }

    #[tokio::test]
    async fn retries_purchase_after_server_error_with_same_order_id() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let state = seen.clone();
        let app = Router::new().route("/api/my/purchase", post(move |request: axum::http::Request<axum::body::Body>| {
            let state = state.clone();
            async move {
                assert_eq!(request.headers().get("x-api-key").unwrap(), "secret");
                let content_type = request.headers().get("content-type").unwrap().to_str().unwrap().to_owned();
                let body = axum::body::to_bytes(request.into_body(), usize::MAX).await.unwrap();
                state.lock().unwrap().push((String::from_utf8(body.to_vec()).unwrap(), content_type));
                if state.lock().unwrap().len() < 2 { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "retry") } else { (axum::http::StatusCode::OK, r#"{"client_order_id":"0123456789abcdef0123456789abcdef","purchased":1,"remaining":2,"keys":[{"key":"ksk_good"}]}"#) }
            }
        }));
        // 这一个用例刻意保留真实的 `RETRY_BACKOFF`：重试之间必须真的隔开。
        // 供货商广播到货的那一瞬间它自己还没准备好，三连请求挤在几百毫秒里
        // 会一起撞进同一个坏窗口，重试就白做了（Kiro Drop 的生产实例）。
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        let started = std::time::Instant::now();
        let result = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        assert_eq!(result.purchased, 1);
        assert!(
            started.elapsed() >= RETRY_BACKOFF[0],
            "第一次重试前必须等 {:?}，实际只用了 {:?}",
            RETRY_BACKOFF[0],
            started.elapsed()
        );
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, requests[1].0);
        assert_eq!(requests[0].1, "application/json");
    }

    #[tokio::test]
    async fn does_not_retry_client_errors_and_validates_purchase_response() {
        let calls = Arc::new(Mutex::new(0));
        let calls_clone = calls.clone();
        let app = Router::new().route(
            "/api/my/purchase",
            post(move || {
                let calls = calls_clone.clone();
                async move {
                    *calls.lock().unwrap() += 1;
                    (axum::http::StatusCode::BAD_REQUEST, "ksk_bad secret")
                }
            }),
        );
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        let error = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1);
        let text = error.to_string();
        assert!(!text.contains("ksk_bad") && !text.contains("secret"));
    }

    #[tokio::test]
    async fn validates_purchase_order_and_keys_and_supports_webhooks() {
        let paths = Arc::new(Mutex::new(Vec::new()));
        let paths_clone = paths.clone();
        let paths_test = paths.clone();
        let app = Router::new()
            .route("/api/my/purchase", post(|| async { axum::Json(serde_json::json!({"client_order_id":"fedcba9876543210fedcba9876543210","purchased":1,"remaining":2,"keys":[{"key":"bad"}]})) }))
            .route("/api/my/webhook", put(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_clone.clone(); async move { assert_eq!(request.headers().get("x-api-key").unwrap(), "secret"); assert_eq!(request.headers().get("content-type").unwrap(), "application/json"); paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }))
            .route("/api/my/webhook/test", post(move |request: axum::http::Request<axum::body::Body>| { let paths = paths_test.clone(); async move { assert_eq!(request.headers().get("x-api-key").unwrap(), "secret"); assert!(request.headers().get("content-type").is_none()); paths.lock().unwrap().push(request.uri().path().to_owned()); axum::http::StatusCode::NO_CONTENT } }));
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        assert!(
            client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
        client.register_webhook("http://hook").await.unwrap();
        client.test_webhook().await.unwrap();
        assert_eq!(
            *paths.lock().unwrap(),
            vec!["/api/my/webhook", "/api/my/webhook/test"]
        );
    }

    #[tokio::test]
    async fn kiro_app_uses_bearer_auth_and_openapi_endpoints() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = Router::new()
            .route(
                "/openapi/stock",
                get(
                    |request: axum::http::Request<axum::body::Body>| async move {
                        assert_eq!(
                            request.headers().get("authorization").unwrap(),
                            "Bearer app-secret"
                        );
                        assert!(request.headers().get("x-api-key").is_none());
                        axum::Json(serde_json::json!({"availableKeys": 12, "keyPrice": 2.5}))
                    },
                ),
            )
            .route(
                "/openapi/balance",
                get(|| async { axum::Json(serde_json::json!({"balance": 480})) }),
            )
            .route(
                "/openapi/claim",
                post(move |body: axum::body::Bytes| {
                    let observed = observed.clone();
                    async move {
                        observed
                            .lock()
                            .unwrap()
                            .push(String::from_utf8(body.to_vec()).unwrap());
                        // 官方文档：claim 返回 {key, pointsCost, balance}。
                        axum::Json(serde_json::json!({
                            "key": "ksk_single", "pointsCost": 100, "balance": 900
                        }))
                    }
                }),
            );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.stock_available, Some(12));
        assert_eq!(snapshot.key_price, Some(2.5));
        assert_eq!(snapshot.balance, Some(480));
        // kiro-app 读不到 profile/status，也读不到对方登记的回调地址。
        assert!(snapshot.profile.is_none() && snapshot.status.is_none());
        assert!(snapshot.webhook_url.is_none());
        assert_eq!(client.purchase_quote().await.unwrap().stock, 12);

        // 取 1 个时对方返回 {key} 而不是 {keys:[...]}，也要能收下。
        let purchase = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        assert_eq!(purchase.purchased, 1);
        // remaining 是扣费后余额，points_cost 是本次花掉的积分。
        assert_eq!(purchase.remaining, 900);
        assert_eq!(purchase.points_cost, Some(100));
        assert_eq!(seen.lock().unwrap()[0], r#"{"count":1}"#);
        assert!(!format!("{purchase:?}").contains("ksk_single"));
    }

    #[tokio::test]
    async fn kiro_app_claim_is_never_retried_and_rejects_empty_or_bad_keys() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/openapi/claim",
            post(move || {
                let observed = observed.clone();
                async move {
                    let mut calls = observed.lock().unwrap();
                    *calls += 1;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        r#"{"error":{"type":"server_error","message":"boom"}}"#,
                    )
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        assert!(
            client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
        // claim 没有幂等键，5xx 也只能打一次。
        assert_eq!(*calls.lock().unwrap(), 1);

        // 一个 key 都没拿到、或者给多了，才算错。
        for body in [
            r#"{}"#,
            r#"{"keys":[]}"#,
            r#"{"keys":["  "]}"#,
            r#"{"keys":["ksk_a","ksk_b"]}"#,
        ] {
            let app = Router::new().route(
                "/openapi/claim",
                post(move || async move { (axum::http::StatusCode::OK, body) }),
            );
            let client =
                SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                    .unwrap();
            assert!(
                client
                    .purchase(1, "0123456789abcdef0123456789abcdef")
                    .await
                    .is_err(),
                "{body}"
            );
        }
    }

    #[tokio::test]
    async fn kiro_app_keeps_paid_keys_even_when_the_prefix_is_unexpected() {
        // 积分已经扣了。前缀不是 ksk_ 也必须收下，不能钱花了还把 key 扔掉。
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                axum::Json(serde_json::json!({
                    "keys": ["key-1", "key-2"], "pointsCost": 200, "balance": 800
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let purchase = client
            .purchase(2, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();

        assert_eq!(purchase.purchased, 2);
        assert_eq!(purchase.points_cost, Some(200));
        assert!(!format!("{purchase:?}").contains("key-1"));

        // kiro-rs 那边保持严格：它的响应格式已经验证过，非 ksk_ 就是拿错东西了。
        let strict = Router::new().route(
            "/api/my/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "client_order_id": "0123456789abcdef0123456789abcdef",
                    "purchased": 1, "remaining": 0, "keys": [{"key": "key-1"}]
                }))
            }),
        );
        let strict_client =
            SupplierClient::with_kind(server(strict).await, "secret", SupplierKind::KiroRs)
                .unwrap();
        assert!(
            strict_client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn out_of_stock_is_reported_as_its_own_outcome_not_a_failure() {
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    r#"{"error":{"type":"out_of_stock","message":"库存不足：需要 1 个，当前可售 0 个"}}"#,
                )
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        // 按 error.type 判定，不依赖中文文案。
        assert!(matches!(
            client.purchase(1, "0123456789abcdef0123456789abcdef").await,
            Err(SupplierError::OutOfStock)
        ));
    }

    #[tokio::test]
    async fn rate_limits_are_mapped_with_retry_after_and_not_retried() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/openapi/stock",
            get(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    (
                        axum::http::StatusCode::TOO_MANY_REQUESTS,
                        r#"{"error":{"type":"rate_limit_exceeded","message":"slow down","retryAfter":42}}"#,
                    )
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let error = client.purchase_quote().await.unwrap_err();

        assert!(matches!(
            error,
            SupplierError::RateLimited {
                retry_after: Some(42),
                ..
            }
        ));
        assert!(error.to_string().contains("retry after 42s"));
        assert_eq!(*calls.lock().unwrap(), 1, "429 不重试");
    }

    #[tokio::test]
    async fn kiro_app_cannot_register_or_test_webhooks() {
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().fallback(move || {
            let observed = observed.clone();
            async move {
                *observed.lock().unwrap() += 1;
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        assert!(matches!(
            client.register_webhook("https://admin.example/hook").await,
            Err(SupplierError::Unsupported(_))
        ));
        assert!(matches!(
            client.test_webhook().await,
            Err(SupplierError::Unsupported(_))
        ));
        // 不支持就不该发请求出去。
        assert_eq!(*calls.lock().unwrap(), 0);
        assert!(!SupplierKind::KiroApp.supports_webhook_registration());
        assert!(SupplierKind::KiroRs.supports_webhook_registration());
    }

    #[tokio::test]
    async fn kiroapp_io_reads_stock_with_the_tiered_price_range() {
        let app = Router::new().route(
            "/api/me/stock",
            get(
                |request: axum::http::Request<axum::body::Body>| async move {
                    assert_eq!(
                        request.headers().get("authorization").unwrap(),
                        "Bearer km_secret"
                    );
                    assert!(request.headers().get("x-api-key").is_none());
                    axum::Json(serde_json::json!({
                        "stock": 120, "price": 30, "price_min": 30, "price_max": 65, "balance": 2060
                    }))
                },
            ),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        let snapshot = client.snapshot().await.unwrap();

        assert_eq!(snapshot.stock_available, Some(120));
        // 阶梯定价：key_price 是最低价，key_price_max 是最高档。
        assert_eq!(snapshot.key_price, Some(30.0));
        assert_eq!(snapshot.key_price_max, Some(65.0));
        assert_eq!(snapshot.balance, Some(2060));
        // 一次 /api/me/stock 就够，不该再打 profile。
        assert!(snapshot.profile.is_none() && snapshot.status.is_none());
        assert_eq!(client.purchase_quote().await.unwrap().stock, 120);
    }

    #[tokio::test]
    async fn kiroapp_io_purchase_sends_the_idempotency_key_and_bills_by_total_debit() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    observed
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(body.to_vec()).unwrap());
                    axum::Json(serde_json::json!({
                        "purchased": 2, "requested": 2, "remaining": 115,
                        // 阶梯定价：本单两个 key 单价不同，只有 total_debit 是权威数字。
                        "unit_price": 38, "total_debit": 68, "order_id": "0d9f",
                        "keys": [
                            {"key": "ksk_a", "account": "user-a", "password": "pw",
                             "issuer_url": "https://idc.example", "price": 30},
                            {"key": "ksk_b", "account": "user-b", "password": "pw",
                             "issuer_url": "https://idc.example", "price": 38}
                        ],
                        "replayed": false
                    }))
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        let purchase = client
            .purchase(2, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();

        assert_eq!(purchase.purchased, 2);
        assert_eq!(purchase.remaining, 115);
        // 计费认 total_debit，不是 unit_price × count（那样会算出 76）。
        assert_eq!(purchase.points_cost, Some(68));
        // 均价、对方订单号、重放标记都必须带出来：前两个用于对账，后一个用于识别假失败。
        assert_eq!(purchase.unit_price, Some(38.0));
        assert_eq!(purchase.supplier_order_id.as_deref(), Some("0d9f"));
        assert!(!purchase.replayed);
        // 每个 key 的单价跟着 key 走。阶梯定价下按单摊会把 30 和 38 抹成同一个假均价，
        // 而「每存活小时成本」要的是单个号的真实成本。
        assert_eq!(
            purchase
                .keys
                .iter()
                .map(SupplierKey::price)
                .collect::<Vec<_>>(),
            vec![Some(30.0), Some(38.0)]
        );
        let request: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
        assert_eq!(request["count"], 2);
        assert_eq!(
            request["client_order_id"],
            "0123456789abcdef0123456789abcdef"
        );
        // 没有批次号时不该带 order_id 字段（带空值会被对方当成定向拉取）。
        assert!(request.get("order_id").is_none());
        assert!(!format!("{purchase:?}").contains("ksk_a"));
    }

    #[tokio::test]
    async fn kiroapp_io_purchase_targets_a_batch_and_accepts_partial_fills() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let observed = seen.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    observed
                        .lock()
                        .unwrap()
                        .push(String::from_utf8(body.to_vec()).unwrap());
                    // 余额不足时按买得起的数量成交：purchased < requested 是正常路径。
                    axum::Json(serde_json::json!({
                        "purchased": 1, "requested": 5, "remaining": 119,
                        "total_debit": 30, "keys": [{"key": "ksk_partial"}]
                    }))
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        let purchase = client
            .purchase_batch(
                5,
                "0123456789abcdef0123456789abcdef",
                Some(" batch-7 "),
                None,
            )
            .await
            .unwrap();

        // 部分成交不报错，按实际到手数量记账。
        assert_eq!(purchase.purchased, 1);
        assert_eq!(purchase.keys.len(), 1);
        let request: serde_json::Value = serde_json::from_str(&seen.lock().unwrap()[0]).unwrap();
        assert_eq!(request["order_id"], "batch-7");
    }

    #[tokio::test]
    async fn kiroapp_io_purchase_is_retried_because_the_order_id_makes_it_idempotent() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |body: axum::body::Bytes| {
                let observed = observed.clone();
                async move {
                    let mut calls = observed.lock().unwrap();
                    calls.push(String::from_utf8(body.to_vec()).unwrap());
                    if calls.len() < 2 {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            "boom".to_owned(),
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            r#"{"purchased":1,"remaining":9,"total_debit":30,
                                "keys":[{"key":"ksk_retried"}],"replayed":true}"#
                                .to_owned(),
                        )
                    }
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap()
                .without_backoff();

        let purchase = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();

        assert_eq!(purchase.purchased, 1);
        let calls = calls.lock().unwrap();
        // 幂等键在手，5xx 可以安全重试；两次请求体必须完全一致才算真幂等。
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0], calls[1]);
        assert!(SupplierKind::KiroAppIo.purchase_is_idempotent());
        assert!(!SupplierKind::KiroApp.purchase_is_idempotent());
    }

    #[tokio::test]
    async fn idempotent_protocols_map_409_to_order_conflict_and_never_retry_it() {
        // 同一 client_order_id 换了 count：对方文档说返 409。这不是「请求失败」，
        // 是原单已经成交——钱扣了、key 出货了、我们没拿到。必须能和普通 HTTP 错误分开，
        // 否则只会变成一条 failed 事件让人反复点 retry，每次都撞同一个 409。
        let calls = Arc::new(Mutex::new(0_usize));
        let observed = calls.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move || {
                let observed = observed.clone();
                async move {
                    *observed.lock().unwrap() += 1;
                    (
                        axum::http::StatusCode::CONFLICT,
                        r#"{"error":"该订单号已存在且参数不一致"}"#,
                    )
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        let error = client
            .purchase(3, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap_err();

        assert!(
            matches!(error, SupplierError::OrderConflict(_)),
            "{error:?}"
        );
        // 4xx 不重试：再打一次只会拿到同一个 409。
        assert_eq!(*calls.lock().unwrap(), 1);
        // 错误文本要能说清该去干什么，且不能泄露 api key。
        let rendered = error.to_string();
        assert!(rendered.contains("already settled"));
        assert!(!rendered.contains("km_secret"));
    }

    #[tokio::test]
    async fn non_idempotent_claim_keeps_409_as_a_plain_http_error() {
        // kiro-app 的 claim 没有幂等键，409 不代表「原单已成交」，不能套用那套语义。
        let app = Router::new().route(
            "/openapi/claim",
            post(|| async {
                (
                    axum::http::StatusCode::CONFLICT,
                    r#"{"error":{"type":"conflict"}}"#,
                )
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "app-secret", SupplierKind::KiroApp)
                .unwrap();

        let error = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap_err();

        assert!(
            matches!(error, SupplierError::Http { status: 409, .. }),
            "{error:?}"
        );
        assert!(!SupplierKind::KiroApp.purchase_is_idempotent());
    }

    #[tokio::test]
    async fn kiroapp_io_flat_error_envelope_maps_403_to_insufficient_balance() {
        // kiroapp.io 用扁平 {"error":"原因"}，没有 error.type 可判定。
        let app = Router::new().route(
            "/api/me/purchase",
            post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    r#"{"error":"余额不足，无法购买任何密钥"}"#,
                )
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        let error = client
            .purchase(1, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap_err();

        assert!(matches!(error, SupplierError::InsufficientBalance(_)));
        assert!(error.to_string().contains("balance is insufficient"));
    }

    #[tokio::test]
    async fn kiroapp_io_zero_purchased_is_treated_as_out_of_stock() {
        let app = Router::new().route(
            "/api/me/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "purchased": 0, "requested": 3, "remaining": 0, "keys": []
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();

        // 成交 0 个不能记成「成功买了 0 个」——那会让事件历史显示 succeeded。
        assert!(matches!(
            client.purchase(3, "0123456789abcdef0123456789abcdef").await,
            Err(SupplierError::OutOfStock)
        ));
    }

    #[tokio::test]
    async fn kiroapp_io_rejects_key_count_mismatch_and_cannot_register_webhooks() {
        let app = Router::new().route(
            "/api/me/purchase",
            post(|| async {
                // 说买了 2 个却只给 1 个 key：对不上就别入账。
                axum::Json(serde_json::json!({
                    "purchased": 2, "remaining": 0, "keys": [{"key": "ksk_only_one"}]
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "km_secret", SupplierKind::KiroAppIo)
                .unwrap();
        assert!(
            client
                .purchase(2, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );

        assert!(!SupplierKind::KiroAppIo.supports_webhook_registration());
        let client =
            SupplierClient::with_kind("https://kiroapp.io", "km_secret", SupplierKind::KiroAppIo)
                .unwrap();
        assert!(matches!(
            client.register_webhook("https://admin.example/hook").await,
            Err(SupplierError::Unsupported(_))
        ));
        assert!(matches!(
            client.test_webhook().await,
            Err(SupplierError::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn kiro_drop_reads_string_amounts_and_takes_stock_from_the_me_stock_endpoint() {
        // Drop 把金额编码成字符串（"884.400000"）。复用 kiro-rs 的 wire（u64 字段）
        // 会直接 Decode 失败，快照就只剩一个 API 错误。
        //
        // 可购买数量必须取 `/api/me/stock` 的 `stock`。`/api/status` 的 `keys_stock`
        // 跟着 `keys_active` 走，据它下单四次全拿到 500——这里刻意让两个数不相等。
        let seen = Arc::new(Mutex::new(Vec::new()));
        let profile_seen = seen.clone();
        let status_seen = seen.clone();
        let stock_seen = seen.clone();
        let app = Router::new()
            .route(
                "/api/my/profile",
                get(move |request: axum::http::Request<axum::body::Body>| {
                    let seen = profile_seen.clone();
                    async move {
                        // Drop 的令牌前缀是 usr-，但认证头与 kiro-rs 同为 X-API-Key。
                        assert_eq!(request.headers().get("x-api-key").unwrap(), "usr-secret");
                        assert!(request.headers().get("authorization").is_none());
                        seen.lock().unwrap().push(request.uri().path().to_owned());
                        axum::Json(serde_json::json!({
                            "name": "user@example.com",
                            "quota": "2000.000000",
                            "remaining": "884.400000",
                            "used_quota": "1115.600000",
                            "webhook_url": "https://your-server.example/hook"
                        }))
                    }
                }),
            )
            .route(
                "/api/status",
                get(move |request: axum::http::Request<axum::body::Body>| {
                    let seen = status_seen.clone();
                    async move {
                        seen.lock().unwrap().push(request.uri().path().to_owned());
                        axum::Json(serde_json::json!({
                            "keys_active": 5, "keys_dead": 0, "keys_stock": 25, "generating": false
                        }))
                    }
                }),
            )
            .route(
                "/api/me/stock",
                get(move |request: axum::http::Request<axum::body::Body>| {
                    let seen = stock_seen.clone();
                    async move {
                        seen.lock().unwrap().push(request.uri().path().to_owned());
                        axum::Json(serde_json::json!({
                            "stock": 3, "price": "2.20", "balance": "884.400000"
                        }))
                    }
                }),
            );
        let client =
            SupplierClient::with_kind(server(app).await, "usr-secret", SupplierKind::KiroDrop)
                .unwrap();

        let snapshot = client.snapshot().await.unwrap();
        // 3 而不是 25：可提取数量只认 /api/me/stock。
        assert_eq!(snapshot.stock_available, Some(3));
        assert_eq!(snapshot.key_price, Some(2.20));
        // 元转整数向下取整：额度用来判断「够不够买」，宁可少报不能多报。
        assert_eq!(snapshot.balance, Some(884));
        let profile = snapshot.profile.unwrap();
        assert_eq!(profile.quota, 2000);
        assert_eq!(profile.remaining, 884);
        assert_eq!(profile.used_quota, 1115);
        assert_eq!(
            snapshot.webhook_url.as_deref(),
            Some("https://your-server.example/hook")
        );
        // Drop 没有 /api/my/stock：真去打就是 404。
        assert!(!seen.lock().unwrap().iter().any(|p| p == "/api/my/stock"));
        assert_eq!(client.purchase_quote().await.unwrap().stock, 3);
    }

    /// 线上实测的缺货 409 原文（2026-08-07，订单 600b6fbd…）。
    ///
    /// 这条钉住两个曾经踩过的坑：
    /// 1. `STORE_INVENTORY_SHORTAGE` 不含 "insufficient stock" 这类英文短语，
    ///    纯词表匹配抓不到 → 必须先读 `error.code`。
    /// 2. 正文里的中文是 `\uXXXX` 转义（`response.text()` 给的是原始字节），
    ///    `contains("库存不足")` 永远不成立。
    ///
    /// 判不出缺货 → 不回退 → 「webhook 到了却一直买不到」的原始故障复现。
    #[test]
    fn real_world_store_inventory_shortage_is_recognised_as_out_of_stock() {
        let body = concat!(
            r#"{"error":{"code":"STORE_INVENTORY_SHORTAGE","details":{"available":0},"#,
            r#""message":"Store 库存不足","#,
            r#""request_id":"req_c5be2ed926434b6db19e6284a13eba5e"}}"#
        );
        assert_eq!(DropConflictKind::OutOfStock, classify_drop_conflict(body));

        // 去掉 code 只剩转义中文，文案回退路径也必须认出来
        let text_only = r#"{"error":{"message":"Store 库存不足"}}"#;
        assert_eq!(
            DropConflictKind::OutOfStock,
            classify_drop_conflict(text_only),
            "转义形态的中文必须能匹配上"
        );

        // 只剩 details.available == 0 也足以判缺货
        let details_only = r#"{"error":{"details":{"available":0}}}"#;
        assert_eq!(
            DropConflictKind::OutOfStock,
            classify_drop_conflict(details_only)
        );
    }

    #[test]
    fn drop_409_classifier_only_switches_region_on_an_explicit_stock_shortage() {
        // 缺货 → 可换区
        for body in [
            r#"{"error":"库存不足"}"#,
            r#"{"message":"当前区域没有库存"}"#,
            r#"{"detail":"insufficient stock for region us-east-1"}"#,
            r#"{"error":"Out Of Stock"}"#,
            r#"{"msg":"无可用 Key"}"#,
        ] {
            assert_eq!(
                DropConflictKind::OutOfStock,
                classify_drop_conflict(body),
                "应判为缺货: {body}"
            );
        }

        // 余额不足 → 换区照样买不起，必须单独归类
        for body in [
            r#"{"error":"余额不足"}"#,
            r#"{"message":"Insufficient balance"}"#,
            r#"{"detail":"not enough balance"}"#,
        ] {
            assert_eq!(
                DropConflictKind::InsufficientBalance,
                classify_drop_conflict(body),
                "应判为余额不足: {body}"
            );
        }

        // 读不出来 → 不换区（失败关闭）。把这些误判成缺货会白打一次欧区并掩盖真因。
        for body in [
            r#"{"error":"订单号冲突"}"#,
            r#"{"error":"价格超过 max_total_cny"}"#,
            r#"{"error":"order id conflict"}"#,
            "",
            "{}",
        ] {
            assert_eq!(
                DropConflictKind::Indeterminate,
                classify_drop_conflict(body),
                "应判为无法确定: {body}"
            );
        }
    }

    #[test]
    fn drop_409_balance_shortage_is_not_mistaken_for_stock() {
        // 「余额不足」与「库存不足」都含「不足」，只看子串会互相误命中。
        // 同时提到两者时必须判成余额不足——那才是真正的阻塞原因。
        assert_eq!(
            DropConflictKind::InsufficientBalance,
            classify_drop_conflict(r#"{"error":"余额不足，无法购买当前库存"}"#)
        );
    }

    #[test]
    fn region_fallback_order_id_is_derived_stable_and_distinct() {
        let base = "0123456789abcdef0123456789abcdef";
        let eu = derive_region_order_id(base, SupplierRegion::Eu);

        // 32 位十六进制：purchase() 入口会校验这个格式
        assert_eq!(32, eu.len(), "幂等号必须是 32 位");
        assert!(eu.chars().all(|c| c.is_ascii_hexdigit()), "必须是十六进制");
        // 不能与美区那个号相同，否则可能被当成重放美区单
        assert_ne!(base, eu.as_str());
        // 同一事件重试要得到同一个号，幂等性才成立
        assert_eq!(eu, derive_region_order_id(base, SupplierRegion::Eu));
        // 两个区互不相同
        assert_ne!(eu, derive_region_order_id(base, SupplierRegion::Us));
    }

    #[tokio::test]
    async fn kiro_drop_falls_back_to_eu_when_the_default_region_is_out_of_stock() {
        // 线上表现：webhook 到了却一直买不到——默认只打美区，美区空了就 409，
        // 而欧区其实有货。这条钉住「缺货自动改打欧区」且「换区换了幂等号」。
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let observed = bodies.clone();
        let app = Router::new().route(
            "/api/my/purchase",
            post(move |request: axum::http::Request<axum::body::Body>| {
                let observed = observed.clone();
                async move {
                    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let is_eu = body.get("region").and_then(|v| v.as_str()) == Some("eu");
                    observed.lock().unwrap().push(body);
                    if is_eu {
                        return axum::http::Response::builder()
                            .status(200)
                            .header("content-type", "application/json")
                            .body(axum::body::Body::from(
                                serde_json::json!({
                                    "client_order_id": "",
                                    "purchased": 1,
                                    "remaining": "884.400000",
                                    "region": "eu-central-1",
                                    "order_id": "store_eu_1",
                                    "keys": [{"key": "ksk_eu_one", "region": "eu-central-1"}]
                                })
                                .to_string(),
                            ))
                            .unwrap();
                    }
                    axum::http::Response::builder()
                        .status(409)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::json!({"error": "库存不足"}).to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "usr-secret", SupplierKind::KiroDrop)
                .unwrap();

        let base = "0123456789abcdef0123456789abcdef";
        let purchase = client.purchase(1, base).await.unwrap();

        assert_eq!(purchase.purchased, 1);
        // 实际出货区域取自响应，不再一路按美区落库
        assert_eq!(purchase.actual_region, Some(SupplierRegion::Eu));
        assert_eq!(purchase.region_source, Some(RegionSource::PurchaseResponse));
        assert_eq!(purchase.supplier_order_id.as_deref(), Some("store_eu_1"));

        let sent = bodies.lock().unwrap().clone();
        assert_eq!(2, sent.len(), "应当只回退一次：默认区 + 欧区");
        // 第一发不带 region（走对方默认美区）
        assert!(sent[0].get("region").is_none(), "首发不该带 region");
        assert_eq!(sent[0]["client_order_id"], base);
        // 第二发带 eu，且换了幂等号
        assert_eq!(sent[1]["region"], "eu");
        assert_ne!(
            sent[1]["client_order_id"], sent[0]["client_order_id"],
            "换区必须换幂等号，否则可能被当成重放美区那单"
        );
    }

    #[tokio::test]
    async fn kiro_drop_does_not_fall_back_when_the_shortage_reason_is_unreadable() {
        // 409 是多义的。读不出缺货就不该换区——否则余额不足也会白打一次欧区。
        let hits = Arc::new(Mutex::new(0usize));
        let counter = hits.clone();
        let app = Router::new().route(
            "/api/my/purchase",
            post(move || {
                let counter = counter.clone();
                async move {
                    *counter.lock().unwrap() += 1;
                    axum::http::Response::builder()
                        .status(409)
                        .header("content-type", "application/json")
                        .body(axum::body::Body::from(
                            serde_json::json!({"error": "订单号冲突"}).to_string(),
                        ))
                        .unwrap()
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "usr-secret", SupplierKind::KiroDrop)
                .unwrap();

        let result = client.purchase(1, "0123456789abcdef0123456789abcdef").await;
        assert!(result.is_err(), "无法判定的 409 不该被当成成功");
        assert_eq!(1, *hits.lock().unwrap(), "不该发生换区重试");
    }

    #[tokio::test]
    async fn kiro_drop_purchase_parses_the_string_remaining_and_keeps_the_order_id() {
        // 采购响应的 remaining 也是字符串。解析失败意味着钱已经扣了却拿不到 key。
        let body = Arc::new(Mutex::new(String::new()));
        let observed = body.clone();
        let app = Router::new().route(
            "/api/my/purchase",
            post(move |request: axum::http::Request<axum::body::Body>| {
                let observed = observed.clone();
                async move {
                    assert_eq!(request.headers().get("x-api-key").unwrap(), "usr-secret");
                    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    *observed.lock().unwrap() = String::from_utf8(bytes.to_vec()).unwrap();
                    axum::Json(serde_json::json!({
                        "client_order_id": "0123456789abcdef0123456789abcdef",
                        "purchased": 2,
                        "remaining": "884.400000",
                        "keys": [{"key": "ksk_one"}, {"key": "ksk_two"}]
                    }))
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "usr-secret", SupplierKind::KiroDrop)
                .unwrap();

        let purchase = client
            .purchase(2, "0123456789abcdef0123456789abcdef")
            .await
            .unwrap();
        assert_eq!(purchase.purchased, 2);
        assert_eq!(purchase.remaining, 884);
        assert_eq!(purchase.keys.len(), 2);
        // Drop 只报扣完的余额，不报本单扣费额；靠余额差反推不可靠，所以留空不猜。
        assert_eq!(purchase.points_cost, None);
        assert_eq!(purchase.unit_price, None);

        let request: serde_json::Value =
            serde_json::from_str(&body.lock().unwrap().clone()).unwrap();
        assert_eq!(request["count"], 2);
        assert_eq!(
            request["client_order_id"],
            "0123456789abcdef0123456789abcdef"
        );
        // 不发 max_total_cny：没有金额预算能力，凭空填一个数会在涨价时挡掉正常采购。
        assert!(request.get("max_total_cny").is_none());
    }

    #[tokio::test]
    async fn kiro_ceo_overview_never_touches_the_missing_status_endpoint() {
        // kiro.ceo 是 SPA：未命中的路径落到前端兜底路由，返回 200 + HTML。
        // 所以「顺手打一发 /api/status」不会报 404，而是在 JSON 反序列化上炸掉，
        // 界面上只剩一句「请求失败」——这正是按 kiro-rs 协议接会失败的原因。
        let app = Router::new()
            .route(
                "/api/my/profile",
                get(
                    |request: axum::http::Request<axum::body::Body>| async move {
                        assert_eq!(request.headers().get("x-api-key").unwrap(), "ceo-secret");
                        assert!(request.headers().get("authorization").is_none());
                        axum::Json(serde_json::json!({
                            "name": "codekjie", "quota": 6000, "remaining": 4500,
                            "used_quota": 1500, "webhook_url": "https://admin.example/hook"
                        }))
                    },
                ),
            )
            .route(
                "/api/my/stock",
                get(|| async {
                    // 顶层 max 是跨区合计；能买到的是各区自己的 available。
                    axum::Json(serde_json::json!({
                        "max": 12,
                        "zones": [
                            {"zone": "us", "enabled": true, "available": 0, "max": 0, "unit_price": 20},
                            {"zone": "eu", "enabled": true, "available": 12, "max": 0, "unit_price": 15}
                        ]
                    }))
                }),
            )
            .fallback(|| async {
                (
                    axum::http::StatusCode::OK,
                    [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
                    "<!DOCTYPE html><html><body><div id=\"app\"></div></body></html>",
                )
            });
        let base = server(app).await;
        let client = SupplierClient::with_kind(&base, "ceo-secret", SupplierKind::KiroCeo).unwrap();

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.stock_available, Some(12));
        // 单价跟着真正能买到的那个区走：美国区空了，报的必须是欧洲区的 15，
        // 而不是美国区的 20——否则单价上限会拿一个买不到的价去判定。
        assert_eq!(snapshot.key_price, Some(15.0));
        // 积分余额：字段名没变，数字含义从「还能提几个号」变成积分。
        assert_eq!(snapshot.balance, Some(4500));
        // 没有 status 接口就别假装有一个。
        assert!(snapshot.status.is_none());
        assert_eq!(
            snapshot.webhook_url.as_deref(),
            Some("https://admin.example/hook")
        );
        assert_eq!(client.purchase_quote().await.unwrap().stock, 12);

        // 反证：同一个站点用 kiro-rs 协议接就是这么坏的——它会去打 /api/status，
        // 拿回 200 + HTML 然后在解析上失败。这是「协议选 kiro-rs 为什么失败」的原因。
        let legacy = SupplierClient::with_kind(&base, "ceo-secret", SupplierKind::KiroRs).unwrap();
        assert!(matches!(
            legacy.snapshot().await,
            Err(SupplierError::Decode(_))
        ));
    }

    #[tokio::test]
    async fn kiro_ceo_purchase_accepts_both_the_documented_and_the_real_key_shape() {
        // 线上真实响应（用幂等重放取到的）：`keys` 是**对象数组**，且没有 `details`。
        // 对方文档写的却是纯字符串数组 + 独立的 details。照文档接的代价是
        // `invalid type: map, expected a string at line 1 column 62`——响应键名按字母序
        // 排列，`"keys":[` 的 `[` 正好落在第 62 列——而积分已经扣了，线上因此连丢 7 单。
        let real = serde_json::json!({
            "client_order_id": "0123456789abcdef0123456789abcdef",
            "keys": [
                {"key": "kiro-aaa", "account": "user-a", "password": "pw",
                 "issuer_url": "https://idc", "zone": "us", "aws_region": "us-east-1",
                 "status": "sold", "created_at": "2026-08-01 03:39:04"},
                {"key": "kiro-bbb", "account": "user-b", "password": "pw",
                 "issuer_url": "https://idc", "zone": "us", "aws_region": "us-east-1",
                 "status": "sold", "created_at": "2026-08-01 03:39:04"}
            ],
            "order_id": "9f0370b1d6dd32abcb1176303b81502d",
            "purchased": 2, "remaining": 17, "replayed": true,
            "total_credits": 30, "unit_price": 15, "zone": "us"
        });
        // 文档描述的形状也必须继续能接：哪天对方改回去，不能又是一次丢单。
        let documented = serde_json::json!({
            "client_order_id": "0123456789abcdef0123456789abcdef",
            "purchased": 2, "remaining": 17,
            "keys": ["kiro-aaa", "kiro-bbb"],
            "zone": "us", "unit_price": 15, "total_credits": 30,
            "order_id": "9f0370b1d6dd32abcb1176303b81502d"
        });

        for (label, body, replayed) in [("real", real, true), ("documented", documented, false)] {
            let payload = body.clone();
            let app = Router::new().route(
                "/api/my/purchase",
                post(move |request: axum::http::Request<axum::body::Body>| {
                    let payload = payload.clone();
                    async move {
                        let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                            .await
                            .unwrap();
                        let sent: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                        assert_eq!(sent["count"], 3);
                        // 不发 zone：对方默认美国区，配置里还没有区域字段，乱填会买错区。
                        assert!(sent.get("zone").is_none());
                        axum::Json(payload)
                    }
                }),
            );
            let client =
                SupplierClient::with_kind(server(app).await, "ceo-secret", SupplierKind::KiroCeo)
                    .unwrap();

            // 申请 3 个拿到 2 个是正常竞争结果，按 purchased 处理而不是按 count。
            let purchase = client
                .purchase(3, "0123456789abcdef0123456789abcdef")
                .await
                .unwrap();
            assert_eq!(purchase.purchased, 2, "{label}");
            assert_eq!(purchase.keys.len(), 2, "{label}");
            assert_eq!(purchase.remaining, 17, "{label}");
            // total_credits 是本单权威扣费额，落库记账靠它。
            assert_eq!(purchase.points_cost, Some(30), "{label}");
            assert_eq!(purchase.unit_price, Some(15.0), "{label}");
            assert_eq!(
                purchase.supplier_order_id.as_deref(),
                Some("9f0370b1d6dd32abcb1176303b81502d"),
                "{label}"
            );
            // 幂等重放标记必须透传：钱是上一单扣的，不能记成又买了一次。
            assert_eq!(purchase.replayed, replayed, "{label}");
            assert!(!format!("{purchase:?}").contains("kiro-aaa"), "{label}");
        }
    }

    #[tokio::test]
    async fn kiro_ceo_fixed_us_sends_zone_and_reports_actual_region() {
        let app = Router::new().route(
            "/api/my/purchase",
            post(
                |request: axum::http::Request<axum::body::Body>| async move {
                    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    let sent: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    assert_eq!(sent["zone"], "us");
                    axum::Json(serde_json::json!({
                        "client_order_id": "0123456789abcdef0123456789abcdef",
                        "purchased": 1,
                        "remaining": 2,
                        "keys": ["kiro-us"],
                        "zone": "us"
                    }))
                },
            ),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "secret", SupplierKind::KiroCeo).unwrap();
        let purchase = client
            .purchase_with_context(
                1,
                "0123456789abcdef0123456789abcdef",
                PurchaseContext {
                    supplier_batch_id: None,
                    requested_region: Some(SupplierRegion::Us),
                    region_source: Some(RegionSource::Request),
                },
            )
            .await
            .unwrap();
        assert_eq!(purchase.actual_region, Some(SupplierRegion::Us));
        assert_eq!(purchase.region_source, Some(RegionSource::PurchaseResponse));
    }

    #[tokio::test]
    async fn kiro_ceo_fixed_us_quote_ignores_europe_stock_and_price() {
        let app = Router::new().route(
            "/api/my/stock",
            get(|| async {
                axum::Json(serde_json::json!({
                    "max": 12,
                    "zones": [
                        {"zone": "us", "enabled": true, "available": 2, "max": 0, "unit_price": 20},
                        {"zone": "eu", "enabled": true, "available": 10, "max": 0, "unit_price": 1}
                    ]
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "secret", SupplierKind::KiroCeo).unwrap();
        let quote = client
            .purchase_quote_for(Some(SupplierRegion::Us))
            .await
            .unwrap();
        assert_eq!(quote.zone.as_deref(), Some("us"));
        assert_eq!(quote.stock, 2);
        assert_eq!(quote.unit_price, Some(20.0));
    }

    #[tokio::test]
    async fn kiroapp_io_region_is_sent_only_without_batch_id() {
        let bodies = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
        let state = bodies.clone();
        let app = Router::new().route(
            "/api/me/purchase",
            post(move |request: axum::http::Request<axum::body::Body>| {
                let state = state.clone();
                async move {
                    let bytes = axum::body::to_bytes(request.into_body(), usize::MAX)
                        .await
                        .unwrap();
                    state
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice(&bytes).unwrap());
                    axum::Json(serde_json::json!({
                        "purchased": 1, "remaining": 2,
                        "keys": [{"key": "ksk-io"}]
                    }))
                }
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "secret", SupplierKind::KiroAppIo)
                .unwrap();
        let order = "0123456789abcdef0123456789abcdef";
        client
            .purchase_with_context(
                1,
                order,
                PurchaseContext {
                    supplier_batch_id: None,
                    requested_region: Some(SupplierRegion::Eu),
                    region_source: Some(RegionSource::Request),
                },
            )
            .await
            .unwrap();
        client
            .purchase_with_context(
                1,
                "fedcba9876543210fedcba9876543210",
                PurchaseContext {
                    supplier_batch_id: Some("batch-1"),
                    requested_region: Some(SupplierRegion::Eu),
                    region_source: Some(RegionSource::Webhook),
                },
            )
            .await
            .unwrap();

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies[0]["region"], "eu");
        assert!(bodies[0].get("order_id").is_none());
        assert_eq!(bodies[1]["order_id"], "batch-1");
        assert!(bodies[1].get("region").is_none());
    }

    #[tokio::test]
    async fn each_protocol_maps_its_own_balance_and_stock_status_codes() {
        // 同一个状态码在各家含义不同。接错的后果是把「该去充钱」记成故障（运维照着
        // 排查网络），或把停用账号记成余额不足（去充钱但根本充不进去）。
        for (kind, status, expected) in [
            // kiroapp.io：403 = 有货但一个都买不起。
            (SupplierKind::KiroAppIo, 403u16, "balance"),
            // Kiro Drop：403 = 余额不足，404 = 库存不足无可用 Key。
            (SupplierKind::KiroDrop, 403, "balance"),
            (SupplierKind::KiroDrop, 404, "stock"),
            // kiro.ceo：402 = 积分不足；403 是账号被停用，绝不能当成余额问题。
            (SupplierKind::KiroCeo, 402, "balance"),
            (SupplierKind::KiroCeo, 403, "other"),
            // kiro-rs 没有这套语义，保持原样不特判。
            (SupplierKind::KiroRs, 402, "other"),
            (SupplierKind::KiroRs, 403, "other"),
        ] {
            let code = axum::http::StatusCode::from_u16(status).unwrap();
            let app = Router::new()
                .route(
                    "/api/my/purchase",
                    post(move || async move { (code, r#"{"error":"reason"}"#) }),
                )
                .route(
                    "/api/me/purchase",
                    post(move || async move { (code, r#"{"error":"reason"}"#) }),
                );
            let client = SupplierClient::with_kind(server(app).await, "secret", kind).unwrap();
            let error = client
                .purchase(1, "0123456789abcdef0123456789abcdef")
                .await
                .unwrap_err();
            let actual = match error {
                SupplierError::InsufficientBalance(_) => "balance",
                SupplierError::OutOfStock => "stock",
                _ => "other",
            };
            assert_eq!(actual, expected, "{kind} {status}");
        }
    }

    #[tokio::test]
    async fn kiro_ceo_overview_prices_the_zone_it_would_actually_buy_from() {
        // 各区单价独立。展示的必须是**真正会成交的那个区**的价：欧洲区更便宜但一个
        // 都没有，显示 10 会让人以为买得更划算，而实际成交价是美国区的 15。
        // 也不能反过来固定显示美国区——美国区空了的时候那个数字同样买不到。
        let app = Router::new()
            .route(
                "/api/my/profile",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max_purchase": 10, "min_purchase": 1, "name": "codekjie",
                        "quota": 167, "remaining": 167, "used_quota": 15, "webhook_url": ""
                    }))
                }),
            )
            .route(
                "/api/my/stock",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max": 10, "max_purchase": 10, "min": 1, "quota": 167, "reserved": 0,
                        "zones": [
                            {"available": 11, "enabled": true, "label": "us zone",
                             "max": 10, "stock": 11, "unit_price": 15, "zone": "us"},
                            {"available": 0, "enabled": true, "label": "eu zone",
                             "max": 0, "stock": 0, "unit_price": 10, "zone": "eu"}
                        ]
                    }))
                }),
            );
        let client =
            SupplierClient::with_kind(server(app).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();

        let snapshot = client.snapshot().await.unwrap();
        assert_eq!(snapshot.stock_available, Some(10));
        // 欧洲区 10 更便宜但库存 0，所以报美国区的 15。
        assert_eq!(snapshot.key_price, Some(15.0));
        assert_eq!(snapshot.balance, Some(167));
        // 可购量按区算：available 11 被本区单笔上限 10 夹住。
        let quote = client.purchase_quote().await.unwrap();
        assert_eq!(quote.stock, 10);
        assert_eq!(quote.zone.as_deref(), Some("us"));
        assert_eq!(quote.unit_price, Some(15.0));

        // 两个区都有货时选便宜的那个，并且下单要打的就是这个区。
        let cheaper = Router::new().route(
            "/api/my/stock",
            get(|| async {
                axum::Json(serde_json::json!({
                    "max": 14,
                    "zones": [
                        {"zone": "us", "enabled": true, "available": 10, "max": 0, "unit_price": 20},
                        {"zone": "eu", "enabled": true, "available": 4, "max": 0, "unit_price": 15}
                    ]
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(cheaper).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();
        let quote = client.purchase_quote().await.unwrap();
        assert_eq!(quote.zone.as_deref(), Some("eu"));
        assert_eq!(quote.stock, 4);
        assert_eq!(quote.unit_price, Some(15.0));

        // 关闭的区不算，全区都空时报 0 而不是拿跨区合计去撞一个注定 409 的下单。
        let empty = Router::new().route(
            "/api/my/stock",
            get(|| async {
                axum::Json(serde_json::json!({
                    "max": 7,
                    "zones": [
                        {"zone": "us", "enabled": true, "available": 0, "max": 0, "unit_price": 20},
                        {"zone": "eu", "enabled": false, "available": 7, "max": 0, "unit_price": 15}
                    ]
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(empty).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();
        let quote = client.purchase_quote().await.unwrap();
        assert_eq!(quote.stock, 0);
        assert_eq!(quote.zone, None);
    }

    #[tokio::test]
    async fn kiro_ceo_fixed_us_overview_ignores_cheaper_eu_zone() {
        let app = Router::new()
            .route(
                "/api/my/profile",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max_purchase": 10, "min_purchase": 1, "name": "ceo",
                        "quota": 200, "remaining": 180, "used_quota": 20,
                        "webhook_url": ""
                    }))
                }),
            )
            .route(
                "/api/my/stock",
                get(|| async {
                    axum::Json(serde_json::json!({
                        "max": 20,
                        "zones": [
                            {"zone": "us", "enabled": true, "available": 9,
                             "max": 0, "unit_price": 20},
                            {"zone": "eu", "enabled": true, "available": 11,
                             "max": 0, "unit_price": 1}
                        ]
                    }))
                }),
            );
        let client =
            SupplierClient::with_kind(server(app).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();

        let snapshot = client.snapshot_for(Some(SupplierRegion::Us)).await.unwrap();

        assert_eq!(snapshot.stock_available, Some(9));
        assert_eq!(snapshot.key_price, Some(20.0));
    }

    #[tokio::test]
    async fn kiro_ceo_zero_purchased_is_out_of_stock_and_key_count_must_match() {
        let app = Router::new().route(
            "/api/my/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "purchased": 0, "remaining": 4500, "keys": []
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(app).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();
        // 一个都没成交不能记成 succeeded，那会让事件历史看着像买到了。
        assert!(matches!(
            client.purchase(3, "0123456789abcdef0123456789abcdef").await,
            Err(SupplierError::OutOfStock)
        ));

        let mismatch = Router::new().route(
            "/api/my/purchase",
            post(|| async {
                axum::Json(serde_json::json!({
                    "purchased": 2, "remaining": 0, "keys": ["kiro-only-one"]
                }))
            }),
        );
        let client =
            SupplierClient::with_kind(server(mismatch).await, "ceo-secret", SupplierKind::KiroCeo)
                .unwrap();
        assert!(
            client
                .purchase(2, "0123456789abcdef0123456789abcdef")
                .await
                .is_err()
        );
    }

    #[test]
    fn kiro_drop_decimal_strings_never_overstate_the_balance() {
        assert_eq!(decimal_string_to_u64("884.400000"), 884);
        assert_eq!(decimal_string_to_u64("884.999999"), 884);
        assert_eq!(decimal_string_to_u64(" 30 "), 30);
        assert_eq!(decimal_string_to_u64("0.000000"), 0);
        // 负数、空串、非数字、NaN/Inf 全部按 0 处理——宁可显示没钱也不能虚报余额。
        for bogus in ["-1.5", "", "abc", "NaN", "inf", "1e400"] {
            assert_eq!(decimal_string_to_u64(bogus), 0, "{bogus}");
        }
    }

    #[test]
    fn default_constructor_keeps_the_legacy_protocol() {
        assert_eq!(
            SupplierClient::new("http://localhost", "secret")
                .unwrap()
                .kind(),
            SupplierKind::KiroRs
        );
    }

    #[test]
    fn validates_constructor_and_debug_redacts_secrets() {
        assert!(SupplierClient::new("ftp://localhost", "secret").is_err());
        assert!(SupplierClient::new("http://localhost/", " ").is_err());
        let client = SupplierClient::new("http://localhost/", "secret").unwrap();
        assert!(!format!("{client:?}").contains("secret"));
    }

    #[test]
    fn rejects_non_origin_supplier_base_urls_and_builds_endpoints() {
        for base in [
            "http://localhost/api",
            "http://localhost/api/",
            "http://user:pass@localhost/",
            "http://localhost/?query=1",
            "http://localhost/#fragment",
        ] {
            assert!(SupplierClient::new(base, "secret").is_err(), "{base}");
        }
        let client = SupplierClient::new("http://localhost", "secret").unwrap();
        assert_eq!(
            client.endpoint("/api/status").unwrap().as_str(),
            "http://localhost/api/status"
        );
    }

    #[test]
    fn debug_redacts_profile_webhook_and_status_extra_values() {
        let profile = Profile {
            name: "demo".into(),
            quota: 1,
            remaining: 1,
            used_quota: 0,
            webhook_url: "https://canary.invalid/hook?secret=canary".into(),
        };
        let mut extra = serde_json::Map::new();
        extra.insert("ksk_secret".into(), serde_json::json!("ksk_value"));
        extra.insert("usr_secret".into(), serde_json::json!("usr_value"));
        let status = SupplierStatus {
            keys_active: 1,
            keys_dead: 0,
            keys_stock: 0,
            generating: false,
            extra,
        };
        for output in [format!("{profile:?}"), format!("{status:?}")] {
            assert!(!output.contains("canary"));
            assert!(!output.contains("ksk_secret"));
            assert!(!output.contains("usr_secret"));
        }
    }

    #[test]
    fn deserializes_documented_boolean_generating_status() {
        let status: SupplierStatus = serde_json::from_value(serde_json::json!({
            "keys_active": 10,
            "keys_dead": 2,
            "keys_stock": 4,
            "generating": false,
            "auto_check": true
        }))
        .unwrap();

        assert!(!status.generating);
        assert_eq!(status.keys_active, 10);
    }

    #[test]
    fn requires_a_nonempty_supplier_key_suffix_and_redacts_empty_token() {
        assert!(sanitize("ksk_", "secret").contains("[REDACTED]"));
        assert!(sanitize("ksk_good", "secret").contains("[REDACTED]"));
    }

    #[tokio::test]
    async fn retries_server_errors_exactly_three_times_and_returns_last_error() {
        let calls = Arc::new(Mutex::new(0));
        let state = calls.clone();
        let app = Router::new().route(
            "/api/my/webhook/test",
            post(move || {
                let state = state.clone();
                async move {
                    let mut calls = state.lock().unwrap();
                    *calls += 1;
                    (
                        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                        format!("last body {calls}"),
                    )
                }
            }),
        );
        let client = SupplierClient::new(server(app).await, "usr secret").unwrap();
        let error = client.test_webhook().await.unwrap_err();
        assert_eq!(*calls.lock().unwrap(), 1);
        assert!(error.to_string().contains("last body 1"));
    }

    #[tokio::test]
    async fn retries_transport_failures_until_third_request_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _attempt in 1..=1 {
                let (stream, _) = listener.accept().await.unwrap();
                drop(stream);
            }
        });
        let client = SupplierClient::new(format!("http://{address}"), "secret").unwrap();
        assert!(client.test_webhook().await.is_err());
    }

    #[tokio::test]
    async fn retries_response_body_read_failures() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for attempt in 1..=3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 512];
                let _ = stream.read(&mut request).await;
                if attempt < 3 {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nshort")
                        .await
                        .unwrap();
                } else {
                    stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                        .await
                        .unwrap();
                }
            }
        });
        let client = SupplierClient::new(format!("http://{address}"), "secret")
            .unwrap()
            .without_backoff();
        assert_eq!(client.status().await.unwrap().keys_active, 0);
    }

    #[tokio::test]
    async fn rejects_invalid_webhook_urls_before_sending() {
        let calls = Arc::new(Mutex::new(0));
        let state = calls.clone();
        let app = Router::new().fallback(move || {
            let state = state.clone();
            async move {
                *state.lock().unwrap() += 1;
                axum::http::StatusCode::NO_CONTENT
            }
        });
        let client = SupplierClient::new(server(app).await, "secret").unwrap();
        for url in ["ftp://hook", "/relative", ""] {
            assert!(client.register_webhook(url).await.is_err());
        }
        assert_eq!(*calls.lock().unwrap(), 0);
    }

    #[test]
    fn supplier_error_display_and_debug_redact_and_bound_untrusted_body() {
        let secret = "usr secret";
        let token = "ksk_current_token";
        let body = format!("{secret} {token} {}", "界".repeat(400));
        let error = SupplierError::http(500, &body, secret);
        let display = error.to_string();
        let debug = format!("{error:?}");
        for output in [&display, &debug] {
            assert!(!output.contains(secret));
            assert!(!output.contains(token));
            assert!(output.chars().count() <= 384);
        }
    }

    #[test]
    fn purchase_and_supplier_key_debug_redact_key_values() {
        let key = SupplierKey {
            key: "ksk_private".to_owned(),
            price: Some(30.0),
        };
        let purchase = Purchase {
            client_order_id: "0123456789abcdef0123456789abcdef".to_owned(),
            purchased: 1,
            remaining: 2,
            points_cost: Some(100),
            unit_price: Some(100.0),
            supplier_order_id: Some("0d9f".to_owned()),
            replayed: false,
            actual_region: None,
            region_source: None,
            keys: vec![key.clone()],
        };
        assert!(!format!("{key:?}").contains("ksk_private"));
        assert!(!format!("{purchase:?}").contains("ksk_private"));
    }
}
use reqwest::{Method, Url};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{fmt, sync::OnceLock, time::Duration};

use crate::admin::key_supplier::capabilities::RegionSource;
use crate::model::config::{SupplierKind, SupplierRegion};

const MAX_ATTEMPTS: usize = 3;
const SUPPLIER_USER_AGENT: &str = "kiro-rs-key-supplier/1.0";

/// 重试之间的等待，第 n 次重试取第 n-1 项。**首次尝试从不等待**——抢货拼的就是延迟。
///
/// 没有退避的重试等于没有重试：供货商刚广播「新一批 Key 已上架」的那一瞬间，
/// 它自己的批次往往还没落库，`POST /api/my/purchase` 会短暂返回 5xx。三次尝试
/// 全挤在几百毫秒里，只会落在同一个坏窗口里一起失败，事件直接进 failed 终态
/// （failed 不会被 `claim_next` 捡回来，只能人工点重试）。
/// 生产实例：Kiro Drop 的 `new_keys_available` 三连 500 用掉 554ms，
/// 30 秒后同样的请求体手动下单一次就成了。
const RETRY_BACKOFF: [Duration; MAX_ATTEMPTS - 1] =
    [Duration::from_secs(1), Duration::from_secs(3)];

#[derive(Clone)]
pub struct SupplierClient {
    client: reqwest::Client,
    base_url: Url,
    api_key: Secret,
    kind: SupplierKind,
    /// 重试等待表，生产恒为 `RETRY_BACKOFF`；测试里置空以免真的睡满 4 秒。
    retry_backoff: &'static [Duration],
}

impl fmt::Debug for SupplierClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierClient")
            .field("base_url", &self.base_url)
            .field("kind", &self.kind)
            .field("api_key", &"[REDACTED]")
            .finish()
    }
}

impl SupplierClient {
    /// 历史构造器，默认 `kiro-rs` 协议。
    pub fn new(base_url: impl AsRef<str>, api_key: impl AsRef<str>) -> Result<Self, SupplierError> {
        Self::with_kind(base_url, api_key, SupplierKind::KiroRs)
    }

    pub fn with_kind(
        base_url: impl AsRef<str>,
        api_key: impl AsRef<str>,
        kind: SupplierKind,
    ) -> Result<Self, SupplierError> {
        let raw_url = base_url.as_ref().trim();
        let url = Url::parse(raw_url)
            .map_err(|_| SupplierError::invalid("base_url must be a valid http(s) URL"))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(SupplierError::invalid("base_url must use http or https"));
        }
        if (url.path() != "" && url.path() != "/")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(SupplierError::invalid(
                "base_url must contain only an http(s) origin",
            ));
        }
        let mut base_url = url;
        base_url.set_path("/");
        let key = api_key.as_ref().trim();
        if key.is_empty() {
            return Err(SupplierError::invalid("api_key must not be empty"));
        }
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .no_proxy()
            .user_agent(SUPPLIER_USER_AGENT)
            .build()
            .map_err(|error| SupplierError::network(&error.to_string(), key))?;
        Ok(Self {
            client,
            base_url,
            api_key: Secret(key.to_owned()),
            kind,
            retry_backoff: &RETRY_BACKOFF,
        })
    }

    /// 测试用：去掉重试等待。只有专门断言退避的用例才保留真实等待表。
    #[cfg(test)]
    pub(crate) fn without_backoff(mut self) -> Self {
        self.retry_backoff = &[];
        self
    }

    #[cfg(test)]
    pub fn kind(&self) -> SupplierKind {
        self.kind
    }

    pub async fn profile(&self) -> Result<Profile, SupplierError> {
        self.request(Method::GET, "/api/my/profile", None, RetryPolicy::Retryable)
            .await
    }

    pub async fn stock(&self) -> Result<Stock, SupplierError> {
        self.request(Method::GET, "/api/my/stock", None, RetryPolicy::Retryable)
            .await
    }

    pub async fn status(&self) -> Result<SupplierStatus, SupplierError> {
        self.request(Method::GET, "/api/status", None, RetryPolicy::Retryable)
            .await
    }

    /// 跨协议统一的概览。缺的字段留 `None`，由调用方决定怎么展示。
    pub async fn snapshot(&self) -> Result<SupplierSnapshot, SupplierError> {
        self.snapshot_for(None).await
    }

    pub async fn snapshot_for(
        &self,
        requested_region: Option<SupplierRegion>,
    ) -> Result<SupplierSnapshot, SupplierError> {
        match self.kind {
            SupplierKind::KiroRs => {
                let profile = self.profile().await?;
                let stock = self.stock().await?;
                let status = self.status().await?;
                Ok(SupplierSnapshot {
                    stock_available: Some(stock.max),
                    key_price: None,
                    key_price_max: None,
                    balance: Some(profile.remaining),
                    webhook_url: Some(profile.webhook_url.clone()),
                    profile: Some(profile),
                    status: Some(status),
                })
            }
            SupplierKind::KiroApp => {
                let stock: KiroAppStock = self
                    .request(Method::GET, "/openapi/stock", None, RetryPolicy::Retryable)
                    .await?;
                let balance: KiroAppBalance = self
                    .request(
                        Method::GET,
                        "/openapi/balance",
                        None,
                        RetryPolicy::Retryable,
                    )
                    .await?;
                Ok(SupplierSnapshot {
                    stock_available: Some(stock.available_keys),
                    key_price: stock.key_price,
                    key_price_max: None,
                    balance: Some(balance.balance),
                    webhook_url: None,
                    profile: None,
                    status: None,
                })
            }
            // Drop 的可购买数量在 `/api/me/stock`，不是 `/api/status` 的 `keys_stock`。
            // `/api/status` 仍然要读：`generating` / `keys_active` / `keys_dead` 是概览
            // 里唯一能看出对方号池状态的东西。金额字段是字符串，profile 走单独的 wire。
            SupplierKind::KiroDrop => {
                let profile: KiroDropProfile = self
                    .request(Method::GET, "/api/my/profile", None, RetryPolicy::Retryable)
                    .await?;
                let stock: KiroDropStock = self
                    .request(Method::GET, "/api/me/stock", None, RetryPolicy::Retryable)
                    .await?;
                let status = self.status().await?;
                let webhook_url = profile.webhook_url.clone();
                Ok(SupplierSnapshot {
                    stock_available: Some(stock.stock),
                    // 注意单价按对方文档是 USD，而余额是 CNY。两者不能相加相减，
                    // 做金额预算前必须先确认汇率口径。
                    key_price: parse_decimal_string(&stock.price),
                    key_price_max: None,
                    balance: Some(decimal_string_to_u64(&profile.remaining)),
                    webhook_url: (!webhook_url.is_empty()).then_some(webhook_url),
                    profile: Some(Profile {
                        name: profile.name,
                        quota: decimal_string_to_u64(&profile.quota),
                        remaining: decimal_string_to_u64(&profile.remaining),
                        used_quota: decimal_string_to_u64(&profile.used_quota),
                        webhook_url: profile.webhook_url,
                    }),
                    status: Some(status),
                })
            }
            // kiro.ceo 没有 `/api/status`，也没有 `/api/my/status`。它是个 SPA：
            // 未命中的路径落到前端兜底路由，返回 200 + HTML。所以按 kiro-rs 那样
            // 顺手打一发 status，会在 JSON 反序列化上炸掉，而界面只看到「请求失败」。
            SupplierKind::KiroCeo => {
                let profile = self.profile().await?;
                let stock: KiroCeoStock = self
                    .request(Method::GET, "/api/my/stock", None, RetryPolicy::Retryable)
                    .await?;
                // 展示的单价必须是**真正会成交的那个区**的价，和采购路径挑同一个区。
                // 以前固定显示美国区价：美国区空了、实际会从欧洲区成交时，界面上那个
                // 数字既不是能买到的价，也解释不了为什么采购失败。
                let selected_zone = match requested_region {
                    Some(region) => stock.zones.iter().find(|zone| {
                        zone.enabled && zone.zone.parse::<SupplierRegion>().ok() == Some(region)
                    }),
                    None => pick_kiro_ceo_zone(&stock.zones),
                };
                Ok(SupplierSnapshot {
                    stock_available: Some(
                        selected_zone.map(|zone| zone.purchasable()).unwrap_or(0),
                    ),
                    key_price: selected_zone.and_then(|zone| zone.unit_price),
                    key_price_max: None,
                    balance: Some(profile.remaining),
                    webhook_url: Some(profile.webhook_url.clone()),
                    profile: Some(profile),
                    status: None,
                })
            }
            // `/api/me/stock` 一次给齐库存、报价区间和余额，不必再打 profile。
            SupplierKind::KiroAppIo => {
                let stock: KiroAppIoStock = self
                    .request(Method::GET, "/api/me/stock", None, RetryPolicy::Retryable)
                    .await?;
                Ok(SupplierSnapshot {
                    stock_available: Some(match requested_region {
                        Some(SupplierRegion::Us) => stock.stock_us,
                        Some(SupplierRegion::Eu) => stock.stock_eu,
                        None => stock.stock,
                    }),
                    key_price: stock.price_min.or(stock.price),
                    key_price_max: stock.price_max,
                    balance: Some(stock.balance),
                    webhook_url: None,
                    profile: None,
                    status: None,
                })
            }
        }
    }

    /// 下单前的报价：可买数量，以及**该协议能报出来的**单价。
    ///
    /// `unit_price` 为 `None` 表示这家在下单前拿不到单价（kiro-rs 的 `/api/my/stock`
    /// 只有 `max`）。调用方要把它和「单价是 0」严格区分开：配了单价上限却拿不到单价时
    /// 只能不买，不能当成免费放行。
    ///
    /// 各家的币种/单位不通用（Drop 报 USD、kiroapp 系报积分），所以这个数只允许和
    /// **同一家**配置的上限比较，绝不能跨家做算术。
    pub async fn purchase_quote(&self) -> Result<PurchaseQuote, SupplierError> {
        self.purchase_quote_for(None).await
    }

    pub async fn purchase_quote_for(
        &self,
        requested_region: Option<SupplierRegion>,
    ) -> Result<PurchaseQuote, SupplierError> {
        match self.kind {
            // kiro.ceo 的 `/api/my/stock` 与 kiro-rs 同形，`max` 是文档化字段。
            // ceo 另有分区单价，kiro-rs 什么价都不报。
            SupplierKind::KiroRs => Ok(PurchaseQuote {
                zone: None,
                stock: self.stock().await?.max,
                unit_price: None,
            }),
            // kiro.ceo 按区严格隔离：不传 `zone` 只从美国区取，美国区空了**不会**用
            // 欧洲区顶上，直接返 409 库存不足。所以必须挑一个真有货的区，并把它的
            // 可购量和单价一起带出去——顶层 `max` 是跨区合计，据它下单必然踩空。
            SupplierKind::KiroCeo => {
                let stock: KiroCeoStock = self
                    .request(Method::GET, "/api/my/stock", None, RetryPolicy::Retryable)
                    .await?;
                let selected = match requested_region {
                    Some(region) => stock.zones.iter().find(|zone| {
                        zone.enabled
                            && zone.purchasable() > 0
                            && zone.zone.parse::<SupplierRegion>().ok() == Some(region)
                    }),
                    None => pick_kiro_ceo_zone(&stock.zones),
                };
                match selected {
                    Some(zone) => Ok(PurchaseQuote {
                        stock: zone.purchasable(),
                        unit_price: zone.unit_price,
                        zone: Some(zone.zone.clone()),
                    }),
                    // 所有区都空了。报 0 让上层按「库存不足」跳过，而不是拿合计数去
                    // 撞一个注定 409 的下单。
                    None => Ok(PurchaseQuote {
                        stock: 0,
                        unit_price: None,
                        zone: None,
                    }),
                }
            }
            // Drop 没有 `/api/my/stock`，可提取数量在 `/api/me/stock` 的 `stock`。
            //
            // 曾经读 `/api/status` 的 `keys_stock`，四次自动采购四次拿到 500：那个数
            // 跟着 `keys_active` 走，不代表能买到货。按对方文档没货该返 404，返 500
            // 是他们的 bug，但我们据一个错的字段去下单等于主动往里踩。
            SupplierKind::KiroDrop => {
                let stock: KiroDropStock = self
                    .request(Method::GET, "/api/me/stock", None, RetryPolicy::Retryable)
                    .await?;
                Ok(PurchaseQuote {
                    zone: None,
                    stock: stock.stock,
                    // 按对方文档这个 price 是 USD，而余额是 CNY。只和本家配置的上限比，
                    // 不参与任何跨家或跨币种的算术。
                    unit_price: parse_decimal_string(&stock.price),
                })
            }
            SupplierKind::KiroApp => {
                let stock: KiroAppStock = self
                    .request(Method::GET, "/openapi/stock", None, RetryPolicy::Retryable)
                    .await?;
                Ok(PurchaseQuote {
                    zone: None,
                    stock: stock.available_keys,
                    unit_price: stock.key_price,
                })
            }
            SupplierKind::KiroAppIo => {
                let stock: KiroAppIoStock = self
                    .request(Method::GET, "/api/me/stock", None, RetryPolicy::Retryable)
                    .await?;
                Ok(PurchaseQuote {
                    zone: requested_region.map(|region| region.as_wire().to_owned()),
                    stock: match requested_region {
                        Some(SupplierRegion::Us) => stock.stock_us,
                        Some(SupplierRegion::Eu) => stock.stock_eu,
                        None => stock.stock,
                    },
                    // 阶梯定价：`price_min` 是最低档。判上限用最低价是刻意的——
                    // 「便宜的先出货」，一单里贵的那些由 `total_debit` 事后记账。
                    unit_price: stock.price_min.or(stock.price),
                })
            }
        }
    }

    /// 下单取 Key。
    ///
    /// `kiro-rs` 带 `client_order_id`，服务端幂等，网络抖动可安全重试。
    /// `kiro-app` 的 `/openapi/claim` **没有幂等键**，重试会重复扣积分，
    /// 因此走 `RetryPolicy::Never`：宁可报错让人工重放，也不冒重复购买的风险。
    /// 不定向批次的采购。生产路径走 `purchase_batch`（webhook 可能带批次号）。
    #[cfg(test)]
    pub async fn purchase(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        self.purchase_with_context(count, client_order_id, PurchaseContext::default())
            .await
    }

    /// 下单取 Key，可选定向到供货商的某个开号批次。
    ///
    /// `supplier_batch_id` 仅 `kiroapp-io` 有意义：webhook 推送里带 `order_id`，
    /// 原样传回去就只拉这一车产出的 key，不必从公共池子里跟别人抢。
    /// `zone` 仅 `kiro-ceo` 有意义，且**必须**带上：它按区严格隔离，不传就只从美国区
    /// 取号，美国区空了直接返 409 库存不足，绝不会用别的区顶上。值取自同一次
    /// `purchase_quote()` 选中的区，保证「按哪个区的库存和价格决定的，就买哪个区」。
    pub async fn purchase_batch(
        &self,
        count: u32,
        client_order_id: &str,
        supplier_batch_id: Option<&str>,
        zone: Option<&str>,
    ) -> Result<Purchase, SupplierError> {
        let requested_region = zone
            .map(str::parse::<SupplierRegion>)
            .transpose()
            .map_err(|_| SupplierError::invalid("zone must be us or eu"))?;
        self.purchase_with_context(
            count,
            client_order_id,
            PurchaseContext {
                supplier_batch_id,
                requested_region,
                region_source: requested_region.map(|_| RegionSource::Request),
            },
        )
        .await
    }

    pub async fn purchase_with_context(
        &self,
        count: u32,
        client_order_id: &str,
        context: PurchaseContext<'_>,
    ) -> Result<Purchase, SupplierError> {
        if count == 0 {
            return Err(SupplierError::invalid("purchase count must be positive"));
        }
        if client_order_id.len() != 32
            || !client_order_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SupplierError::invalid(
                "client_order_id must be 32 hexadecimal characters",
            ));
        }
        match self.kind {
            SupplierKind::KiroRs => self.purchase_kiro_rs(count, client_order_id).await,
            SupplierKind::KiroDrop => {
                self.purchase_kiro_drop(count, client_order_id, context)
                    .await
            }
            SupplierKind::KiroCeo => {
                self.purchase_kiro_ceo(count, client_order_id, context)
                    .await
            }
            SupplierKind::KiroApp => self.claim_kiro_app(count, client_order_id).await,
            SupplierKind::KiroAppIo => {
                self.purchase_kiro_app_io(count, client_order_id, context)
                    .await
            }
        }
    }

    /// `POST /api/me/purchase`。带 `client_order_id`，服务端幂等：
    /// 同 id 重放返回字节一致的原响应，绝不重复扣款，所以网络抖动可安全重试。
    ///
    /// 部分成交是**正常路径**——余额不够时对方按买得起的数量成交，`purchased < count`
    /// 不是错误。一个都买不起才返 403（映射成 `InsufficientBalance`）。
    async fn purchase_kiro_app_io(
        &self,
        count: u32,
        client_order_id: &str,
        context: PurchaseContext<'_>,
    ) -> Result<Purchase, SupplierError> {
        let mut body = serde_json::json!({
            "count": count,
            "client_order_id": client_order_id,
        });
        // 只在拿到批次号时带上：缺省行为是从公共池子取，带上则只取该批次产出。
        if let Some(batch_id) = context
            .supplier_batch_id
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            body["order_id"] = serde_json::Value::String(batch_id.to_owned());
        } else if let Some(region) = context.requested_region {
            body["region"] = serde_json::Value::String(region.as_wire().to_owned());
        }
        let response: KiroAppIoPurchase = self
            .request(
                Method::POST,
                "/api/me/purchase",
                Some(body),
                RetryPolicy::Retryable,
            )
            .await?;
        if response.purchased > count {
            return Err(SupplierError::invalid(
                "purchase response purchased exceeds count",
            ));
        }
        if response.keys.len() != response.purchased as usize {
            return Err(SupplierError::invalid(
                "purchase response key count mismatch",
            ));
        }
        // 有货但一个都没成交：对方文档说这种情况返 403，走不到这里。真收到 0
        // 就当竞争失败跳过，不记 failed——重试也只是再抢一次空气。
        if response.purchased == 0 {
            return Err(SupplierError::OutOfStock);
        }
        if response.replayed {
            // 幂等重放：说明上一次其实成功了，只是响应没回到我们手上。
            tracing::info!(
                purchased = response.purchased,
                "kiroapp-io 采购命中幂等重放，未重复扣费"
            );
        }
        Ok(Purchase {
            client_order_id: client_order_id.to_owned(),
            purchased: response.purchased,
            // kiroapp-io 的 remaining 是本次成交后对方剩余库存。
            remaining: response.remaining,
            // total_debit 是权威扣费数字；阶梯定价下不能用 unit_price 反推。
            points_cost: response.total_debit,
            unit_price: response.unit_price,
            supplier_order_id: response.order_id,
            replayed: response.replayed,
            actual_region: context.requested_region,
            region_source: context.requested_region.and(context.region_source),
            // 前缀走宽松校验：钱已经扣了，不能因为前缀不合预期就把 key 扔掉。
            // 每个 key 的单价跟着 key 一起带走：阶梯定价下同一单里各 key 不同价。
            keys: accept_paid_keys(response.keys.into_iter().map(|key| (key.key, key.price)))?,
        })
    }

    /// `POST /api/my/purchase`（Kiro Drop）。请求体与 kiro-rs 相同，响应的 `remaining`
    /// 是字符串。带 `client_order_id` 幂等：同 id 同 count 可安全重试。
    ///
    /// 不发 `max_total_cny`：那是可选的总价保护，但我们目前没有金额预算能力，
    /// 凭空填一个数会在对方涨价时把正常采购挡掉。等做了金额预算再接上。
    /// `POST /api/my/purchase`（Kiro Drop）。
    ///
    /// **区域回退**：不传 `region` 时对方默认只从美区出货，美区空了就 409 缺货，
    /// 而欧区往往还有货——线上表现是「webhook 到了却一直买不到」。所以美区判定缺货后
    /// 自动改打欧区一次。
    ///
    /// 换区必须换幂等号（见 [`derive_region_order_id`]），否则可能被当成重放美区那单。
    ///
    /// 只回退一次、且只在**明确读出缺货**时回退：409 是多义的，余额不足换区照样买不起。
    async fn purchase_kiro_drop(
        &self,
        count: u32,
        client_order_id: &str,
        context: PurchaseContext<'_>,
    ) -> Result<Purchase, SupplierError> {
        // 上层显式指定了区就尊重它，不再自作主张回退——那是运维配死的意图。
        if let Some(region) = context.requested_region {
            return self
                .purchase_kiro_drop_in(count, client_order_id, Some(region), context)
                .await;
        }

        // 未指定：先打默认区（对方默认美区），缺货再试欧区。
        match self
            .purchase_kiro_drop_in(count, client_order_id, None, context)
            .await
        {
            Err(SupplierError::OutOfStock) => {}
            other => return other,
        }

        let fallback = SupplierRegion::Eu;
        let fallback_order_id = derive_region_order_id(client_order_id, fallback);
        tracing::info!(
            supplier = %self.kind,
            fallback_region = fallback.as_api_region(),
            "默认区缺货，改打欧区一次（换用派生幂等号）"
        );
        self.purchase_kiro_drop_in(count, &fallback_order_id, Some(fallback), context)
            .await
    }

    /// 单次下单，`region` 为 `None` 时不带该字段（走对方默认区）。
    async fn purchase_kiro_drop_in(
        &self,
        count: u32,
        client_order_id: &str,
        region: Option<SupplierRegion>,
        _context: PurchaseContext<'_>,
    ) -> Result<Purchase, SupplierError> {
        let mut body = serde_json::json!({
            "count": count,
            "client_order_id": client_order_id,
        });
        // 传 us / eu 以外的值对方直接 400，所以只在明确有值时带。
        if let Some(region) = region {
            body["region"] = serde_json::Value::String(region.as_wire().to_owned());
        }
        let response: KiroDropPurchase = self
            .request(
                Method::POST,
                "/api/my/purchase",
                Some(body),
                RetryPolicy::Retryable,
            )
            .await?;
        // 对方回显了订单号就比对；空串说明这版没回显，不当成错误。
        if !response.client_order_id.is_empty() && response.client_order_id != client_order_id {
            return Err(SupplierError::invalid(
                "purchase response client_order_id mismatch",
            ));
        }
        if response.purchased > count {
            return Err(SupplierError::invalid(
                "purchase response purchased exceeds count",
            ));
        }
        if response.keys.len() != response.purchased as usize {
            return Err(SupplierError::invalid(
                "purchase response key count mismatch",
            ));
        }
        // 库存不足对方返 404，走不到这里。真收到 0 就当竞争失败跳过。
        if response.purchased == 0 {
            return Err(SupplierError::OutOfStock);
        }
        // 实际出货区域以对方响应为准（文档：「本单实际出货区域，始终为完整区域值」）。
        // 解析不出来时不猜——留 None 让上层退到自己的兜底链，而不是硬写成美区。
        let actual_region = response
            .region
            .as_deref()
            .and_then(|value| value.trim().parse::<SupplierRegion>().ok())
            .or(region);
        let region_source = actual_region.map(|_| {
            if response.region.is_some() {
                RegionSource::PurchaseResponse
            } else {
                RegionSource::Request
            }
        });
        Ok(Purchase {
            client_order_id: client_order_id.to_owned(),
            purchased: response.purchased,
            // Drop 的 remaining 是购买后的剩余余额（人民币，字符串）。
            remaining: decimal_string_to_u64(&response.remaining),
            // Drop 不报本单扣费额，只报扣完的余额；金额要靠前后余额差反推，
            // 那不可靠（并发采购会互相干扰），所以这里留空不猜。
            points_cost: None,
            unit_price: None,
            supplier_order_id: response.order_id.filter(|id| !id.trim().is_empty()),
            replayed: false,
            actual_region,
            region_source,
            // 前缀走宽松校验：钱已经扣了，不能因为前缀不合预期就把 key 扔掉。
            keys: accept_paid_keys(response.keys.into_iter().map(|key| (key.key, key.price)))?,
        })
    }

    /// `POST /api/my/purchase`（kiro.ceo）。请求体与 kiro-rs 相同，响应形状不同：
    /// `keys` 是**纯字符串数组**而不是 `[{"key": …}]`，另有 `unit_price`、
    /// `total_credits`、`order_id` 和一个带账号密码的 `details` 数组。
    ///
    /// `zone` 必须带：对方按区严格隔离，不传就只从美国区取号，美国区空了返 409
    /// 库存不足而**不会**用欧洲区顶上。值来自同一次 `purchase_quote()` 选中的区。
    ///
    /// 之前不传，于是拿跨区合计的 `max > 0` 去下一个只打美国区的单，美国区一空就
    /// 连续 409——线上表现是「一直购买失败」，而欧洲区其实一直有货。
    async fn purchase_kiro_ceo(
        &self,
        count: u32,
        client_order_id: &str,
        context: PurchaseContext<'_>,
    ) -> Result<Purchase, SupplierError> {
        let mut body = serde_json::json!({
            "count": count,
            "client_order_id": client_order_id,
        });
        // 传 us / eu 以外的值对方直接 400，所以空值一律不带，让它走默认。
        if let Some(region) = context.requested_region {
            body["zone"] = serde_json::Value::String(region.as_wire().to_owned());
        }
        let response: KiroCeoPurchase = self
            .request(
                Method::POST,
                "/api/my/purchase",
                Some(body),
                RetryPolicy::Retryable,
            )
            .await?;
        if !response.client_order_id.is_empty() && response.client_order_id != client_order_id {
            return Err(SupplierError::invalid(
                "purchase response client_order_id mismatch",
            ));
        }
        if response.purchased > count {
            return Err(SupplierError::invalid(
                "purchase response purchased exceeds count",
            ));
        }
        if response.keys.len() != response.purchased as usize {
            return Err(SupplierError::invalid(
                "purchase response key count mismatch",
            ));
        }
        // 「库存是并发争抢的，申请 5 个拿到 3 个是正常结果」——按 purchased 处理。
        // 但一个都没成交不能记成 succeeded，那会让事件历史看着像买到了。
        if response.purchased == 0 {
            return Err(SupplierError::OutOfStock);
        }
        Ok(Purchase {
            client_order_id: client_order_id.to_owned(),
            purchased: response.purchased,
            remaining: response.remaining,
            // `total_credits` 是本单权威扣费额，`unit_price` 是该区单价。
            points_cost: response.total_credits,
            unit_price: response.unit_price,
            supplier_order_id: response.order_id.filter(|id| !id.trim().is_empty()),
            replayed: response.replayed,
            actual_region: response.zone.or(context.requested_region),
            region_source: if response.zone.is_some() {
                Some(RegionSource::PurchaseResponse)
            } else {
                context.requested_region.and(context.region_source)
            },
            // 宽松前缀：kiro.ceo 的 key 不是 `ksk_` 前缀，而积分已经扣了。
            // 按 kiro-rs 的严格校验会把整单已付费的 key 全判无效——钱花了 key 扔了。
            keys: accept_paid_keys(response.keys.into_iter().map(|key| (key.into_key(), None)))?,
        })
    }

    async fn purchase_kiro_rs(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        let response: PurchaseWire = self
            .request(
                Method::POST,
                "/api/my/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": client_order_id,
                })),
                RetryPolicy::Retryable,
            )
            .await?;
        if response.client_order_id != client_order_id {
            return Err(SupplierError::invalid(
                "purchase response client_order_id mismatch",
            ));
        }
        if response.purchased > count {
            return Err(SupplierError::invalid(
                "purchase response purchased exceeds count",
            ));
        }
        if response.keys.len() != response.purchased as usize {
            return Err(SupplierError::invalid(
                "purchase response key count mismatch",
            ));
        }
        Ok(Purchase {
            client_order_id: response.client_order_id,
            purchased: response.purchased,
            remaining: response.remaining,
            points_cost: None,
            unit_price: None,
            supplier_order_id: None,
            replayed: false,
            actual_region: None,
            region_source: None,
            keys: validate_keys(response.keys.into_iter().map(|key| (key.key, key.price)))?,
        })
    }

    async fn claim_kiro_app(
        &self,
        count: u32,
        client_order_id: &str,
    ) -> Result<Purchase, SupplierError> {
        let text = self
            .send(
                Method::POST,
                "/openapi/claim",
                Some(serde_json::json!({ "count": count })),
                RetryPolicy::Never,
            )
            .await?;
        let response: KiroAppClaim = serde_json::from_str(&text)
            .map_err(|error| SupplierError::decode(&error.to_string(), &self.api_key.0))?;
        // 取 1 个时对方可能返回 {key}，批量时返回 {keys:[...]}。
        let raw_keys = match (response.keys, response.key) {
            (Some(keys), _) => keys,
            (None, Some(key)) => vec![key],
            (None, None) => {
                return Err(SupplierError::invalid("claim response contains no keys"));
            }
        };
        // 空数组和缺字段是同一件事（一个 key 都没拿到），必须同样报错。
        // 放过去会记成「成功买了 0 个」，万一积分已经扣了就查不出来了。
        if raw_keys.is_empty() {
            return Err(SupplierError::invalid("claim response contains no keys"));
        }
        if raw_keys.len() > count as usize {
            return Err(SupplierError::invalid(
                "claim response returned more keys than requested",
            ));
        }
        // 走宽松校验：积分已经扣了，不能因为前缀不合预期就把 key 扔掉。
        // claim 只给整单 pointsCost，不按 key 报价，所以每个 key 的单价是 None。
        let keys = accept_paid_keys(raw_keys.into_iter().map(|key| (key, None)))?;
        let purchased = keys.len() as u32;
        Ok(Purchase {
            client_order_id: client_order_id.to_owned(),
            purchased,
            // claim 响应给的是扣费后余额；库存快照它不返回。
            remaining: response.balance.unwrap_or_default(),
            points_cost: response.points_cost,
            // 按 key 摊出均价：claim 不给 unit_price，但整单扣费和数量都有。
            unit_price: response
                .points_cost
                .filter(|_| purchased > 0)
                .map(|cost| cost as f64 / f64::from(purchased)),
            // claim 没有订单号，也没有幂等重放的概念。
            supplier_order_id: None,
            replayed: false,
            actual_region: None,
            region_source: None,
            keys,
        })
    }

    pub async fn register_webhook(&self, webhook_url: &str) -> Result<(), SupplierError> {
        if !self.kind.supports_webhook_registration() {
            return Err(SupplierError::Unsupported(
                "该供货商不支持远程注册 webhook，请在对方面板手动填写回调地址".to_owned(),
            ));
        }
        validate_http_url(webhook_url)?;
        self.request_empty(
            Method::PUT,
            "/api/my/webhook",
            Some(serde_json::json!({ "webhook_url": webhook_url })),
            RetryPolicy::Retryable,
        )
        .await
    }

    pub async fn test_webhook(&self) -> Result<(), SupplierError> {
        if !self.kind.supports_webhook_registration() {
            return Err(SupplierError::Unsupported(
                "该供货商不支持 webhook 测试推送".to_owned(),
            ));
        }
        self.request_empty(
            Method::POST,
            "/api/my/webhook/test",
            None,
            RetryPolicy::Never,
        )
        .await
    }

    async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<T, SupplierError> {
        let text = self.send(method, path, body, policy).await?;
        serde_json::from_str(&text)
            .map_err(|error| SupplierError::decode(&error.to_string(), &self.api_key.0))
    }

    async fn request_empty(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<(), SupplierError> {
        self.send(method, path, body, policy).await.map(|_| ())
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        body: Option<serde_json::Value>,
        policy: RetryPolicy,
    ) -> Result<String, SupplierError> {
        let url = self.endpoint(path)?;
        let mut last_network = None;
        let attempts = policy.attempts();
        for attempt in 0..attempts {
            // 首发不等；只有重试才退避，见 `RETRY_BACKOFF`。
            if let Some(delay) = attempt
                .checked_sub(1)
                .and_then(|index| self.retry_backoff.get(index))
            {
                tokio::time::sleep(*delay).await;
            }
            let mut request = self.client.request(method.clone(), url.clone());
            request = match self.kind {
                // Drop 与 kiro.ceo 的认证头都与 kiro-rs 相同（Drop 的令牌前缀是 `usr-`）。
                SupplierKind::KiroRs | SupplierKind::KiroDrop | SupplierKind::KiroCeo => {
                    request.header("X-API-Key", &self.api_key.0)
                }
                SupplierKind::KiroApp | SupplierKind::KiroAppIo => {
                    request.bearer_auth(&self.api_key.0)
                }
            };
            if let Some(json) = body.clone() {
                request = request.json(&json);
            }
            let response = match request.send().await {
                Ok(response) => response,
                Err(error) => {
                    last_network =
                        Some(SupplierError::network(&error.to_string(), &self.api_key.0));
                    if attempt + 1 < attempts {
                        continue;
                    }
                    return Err(last_network.unwrap());
                }
            };
            let status = response.status();
            let text = match response.text().await {
                Ok(text) => text,
                Err(error) => {
                    last_network =
                        Some(SupplierError::network(&error.to_string(), &self.api_key.0));
                    if attempt + 1 < attempts {
                        continue;
                    }
                    return Err(last_network.unwrap());
                }
            };
            if status.is_server_error() && policy.allows_retry() && attempt + 1 < attempts {
                continue;
            }
            if !status.is_success() {
                // 429 一律不重试：kiro-app 的 claim 没有幂等键，盲目重试会重复扣积分。
                if status.as_u16() == 429 {
                    return Err(SupplierError::rate_limited(
                        &text,
                        retry_after_seconds(&text),
                        &self.api_key.0,
                    ));
                }
                // 库存被别人抢完是正常竞争结果，不是故障。按 error.type 判定
                // （对方文档明确说不要依赖中文错误文案）。
                if error_type(&text).is_some_and(|kind| kind == "out_of_stock") {
                    return Err(SupplierError::OutOfStock);
                }
                // 三家都用扁平 `{"error":"中文原因"}`，没有机器可判定的 type 字段，
                // 只能按状态码分。而同一个码在各家含义不同，接错就会把「该去充钱」
                // 记成故障、或把故障记成正常竞争：
                //
                // - kiroapp.io：403 = 有货但一个都买不起
                // - Kiro Drop：403 = 余额不足，404 = 库存不足无可用 Key
                // - kiro.ceo：402 = 积分不足，403 = 账号被停用（**不是**余额问题）
                //
                // 余额不足单独归类：重试没用，得先充值，事件历史要能一眼看出来。
                let insufficient_balance = match self.kind {
                    SupplierKind::KiroAppIo | SupplierKind::KiroDrop => status.as_u16() == 403,
                    SupplierKind::KiroCeo => status.as_u16() == 402,
                    SupplierKind::KiroRs | SupplierKind::KiroApp => false,
                };
                if insufficient_balance {
                    return Err(SupplierError::InsufficientBalance(sanitize(
                        &text,
                        &self.api_key.0,
                    )));
                }
                // Drop 早期用 404 表示「库存不足，无可用 Key」。新版文档把它并进了 409，
                // 但保留这条映射：万一还有旧行为，把缺货判成缺货比判成硬故障好。
                if self.kind == SupplierKind::KiroDrop && status.as_u16() == 404 {
                    return Err(SupplierError::OutOfStock);
                }
                // 409 + 幂等协议 = 同一订单号换了参数，也就是原单已经成交。钱已经花了，
                // 这不是「请求失败」而是「我们和对方的账对不上」，得单独归类去核对。
                // 非幂等协议（kiro-app claim）没有订单号概念，409 是别的意思，不特判。
                //
                // kiro.ceo 的 409 是二义的（文档：「库存不足、幂等键撞了别的订单」），
                // 没有可判定字段能分开。仍然归到这里：对方的中文原因会原样带进事件
                // 记录，运维能看出是哪种；而且这条路径既不扣钱也不丢 key，宁可多提醒。
                // Drop 把四种原因都塞进 409。缺货和余额不足能从原文读出来时单独归类：
                // 缺货要能触发换区重试（原先归成 StateConflict，回退根本不会发生），
                // 余额不足要能提示充值。读不出来就沿用下面的多义处理，不猜。
                if status.as_u16() == 409 && self.kind == SupplierKind::KiroDrop {
                    let detail = sanitize(&text, &self.api_key.0);
                    match classify_drop_conflict(&text) {
                        DropConflictKind::OutOfStock => return Err(SupplierError::OutOfStock),
                        DropConflictKind::InsufficientBalance => {
                            return Err(SupplierError::InsufficientBalance(detail));
                        }
                        DropConflictKind::Indeterminate => {}
                    }
                }
                if status.as_u16() == 409 && self.kind.purchase_is_idempotent() {
                    let detail = sanitize(&text, &self.api_key.0);
                    // kiro.ceo 和 Kiro Drop 的 409 都是多义的（库存不足 / 余额不足 /
                    // 持有上限 / 幂等键撞单 / 价格超上限），里面只有一种扣了钱。
                    // 按「已成交」去报等于让人去查一条不存在的订单。
                    // 只有对方原文能分开这几种，所以必须把它带出去。
                    return Err(if self.kind.conflict_means_order_settled() {
                        SupplierError::OrderConflict(detail)
                    } else {
                        SupplierError::StateConflict(detail)
                    });
                }
                return Err(SupplierError::http(status.as_u16(), &text, &self.api_key.0));
            }
            return Ok(text);
        }
        Err(last_network.unwrap_or_else(|| SupplierError::invalid("request failed")))
    }

    fn endpoint(&self, path: &str) -> Result<Url, SupplierError> {
        self.base_url
            .join(path)
            .map_err(|_| SupplierError::invalid("invalid supplier endpoint"))
    }
}

#[derive(Clone, Copy)]
enum RetryPolicy {
    Retryable,
    Never,
}

impl RetryPolicy {
    fn attempts(self) -> usize {
        match self {
            Self::Retryable => MAX_ATTEMPTS,
            Self::Never => 1,
        }
    }

    fn allows_retry(self) -> bool {
        matches!(self, Self::Retryable)
    }
}

#[derive(Clone)]
struct Secret(String);

/// 下单前问到的报价。
///
/// 不派生 `Eq`：`unit_price` 是 f64。
#[derive(Debug, Clone, PartialEq)]
pub struct PurchaseQuote {
    /// 现在能买到的数量。
    pub stock: u64,
    /// 单价，`None` = 这家在下单前报不出价。单位由各家自己定，不可跨家比较。
    pub unit_price: Option<f64>,
    /// 这份报价对应的区域，下单时必须原样带回去。仅 kiro.ceo 有：它按区严格隔离，
    /// 不传就默认美国区，而报价可能来自欧洲区。
    pub zone: Option<String>,
}

/// Kiro Drop 的 409 语义分类。
///
/// 文档把四种原因合并到 409：余额不足 / 库存不足 / 订单号冲突 / 价格超 `max_total_cny`。
/// 只有「库存不足」适合换区重试——余额不足换区照样买不起，订单号冲突换区可能重复下单。
/// 对方没有可判定字段，只能读原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DropConflictKind {
    /// 明确是缺货 → 可以换区再试。
    OutOfStock,
    /// 明确是余额不足 → 换区无用，直接归类让人去充值。
    InsufficientBalance,
    /// 读不出来 → **不换区**。宁可少买不可乱花：把「余额不足」误判成缺货去打欧区，
    /// 既浪费一次请求又会掩盖真实原因。
    Indeterminate,
}

/// 从 409 响应正文判断是哪一种冲突。
///
/// **优先读机器可判定的 `error.code`**，读不到再退回文案匹配。线上实测的缺货响应：
///
/// ```json
/// {"error":{"code":"STORE_INVENTORY_SHORTAGE","details":{"available":0},
///           "message":"Store 库存不足","request_id":"req_…"}}
/// ```
///
/// 两个教训都来自这条真实样本：
/// 1. 有 `code` 就别猜文案。`STORE_INVENTORY_SHORTAGE` 不含 "insufficient stock"
///    这类英文短语，纯词表匹配抓不到。
/// 2. 正文里的中文是 **`\uXXXX` 转义**的（`response.text()` 拿到的是原始字节），
///    所以 `contains("库存不足")` 永远不成立——必须同时匹配转义形态。
///
/// 判定不出来一律 `Indeterminate`，即维持改动前的行为，不会误切区。
pub(crate) fn classify_drop_conflict(body: &str) -> DropConflictKind {
    // 先走结构化字段：对方给了 code 就以它为准。
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        let code = value
            .pointer("/error/code")
            .or_else(|| value.pointer("/code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_ascii_uppercase);
        if let Some(code) = code.as_deref() {
            // 缺货：实测 STORE_INVENTORY_SHORTAGE。用「含 SHORTAGE / INVENTORY /
            // OUT_OF_STOCK」而不是全等，兼容对方给同族 code 换前缀。
            if code.contains("SHORTAGE")
                || code.contains("INVENTORY")
                || code.contains("OUT_OF_STOCK")
                || code.contains("NO_STOCK")
            {
                return DropConflictKind::OutOfStock;
            }
            if code.contains("BALANCE") || code.contains("INSUFFICIENT_FUND") {
                return DropConflictKind::InsufficientBalance;
            }
        }
        // 没有 code 但有 details.available == 0 也足以判缺货。
        if value
            .pointer("/error/details/available")
            .and_then(serde_json::Value::as_i64)
            == Some(0)
        {
            return DropConflictKind::OutOfStock;
        }
    }

    // 退回文案匹配。同时列 UTF-8 与 `\uXXXX` 转义两种形态——原始正文里是后者。
    let text = body.to_ascii_lowercase();
    let has = |needles: &[&str]| needles.iter().any(|needle| text.contains(needle));

    // 余额优先：余额不足时对方也可能顺带提到库存，反过来一般不会。
    if has(&[
        "余额不足",
        // `\uXXXX` 转义形态：原始正文里的中文长这样
        r"\u4f59\u989d\u4e0d\u8db3",
        "insufficient balance",
        "insufficient_balance",
        "not enough balance",
    ]) {
        return DropConflictKind::InsufficientBalance;
    }
    if has(&[
        "库存不足",
        r"\u5e93\u5b58\u4e0d\u8db3",
        "没有库存",
        r"\u6ca1\u6709\u5e93\u5b58",
        "无可用",
        r"\u65e0\u53ef\u7528",
        "insufficient stock",
        "insufficient_stock",
        "out of stock",
        "out_of_stock",
        "no stock",
        "not enough stock",
        "no available key",
    ]) {
        return DropConflictKind::OutOfStock;
    }
    DropConflictKind::Indeterminate
}

/// 为「同一次采购换区重试」派生一个稳定的幂等号。
///
/// 不能复用美区那个号：文档说「同一 client_order_id + 同一 count 会原样重放」，
/// 拿它去打欧区可能被当成重放美区那单。官方双区 webhook 自己就按区各给一个
/// `purchase_order_ids_by_region`，这里遵循同一模型。
///
/// 用哈希而不是随机：同一事件重试能得到同一个号，幂等性仍然成立。
pub(crate) fn derive_region_order_id(base_client_order_id: &str, region: SupplierRegion) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(format!("{base_client_order_id}|{}", region.as_api_region()));
    hex::encode(digest)[..32].to_owned()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PurchaseContext<'a> {
    pub supplier_batch_id: Option<&'a str>,
    pub requested_region: Option<SupplierRegion>,
    pub region_source: Option<RegionSource>,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Profile {
    pub name: String,
    pub quota: u64,
    pub remaining: u64,
    pub used_quota: u64,
    pub webhook_url: String,
}

impl fmt::Debug for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Profile")
            .field("name", &self.name)
            .field("quota", &self.quota)
            .field("remaining", &self.remaining)
            .field("used_quota", &self.used_quota)
            .field("webhook_url", &"[REDACTED]")
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct Stock {
    pub max: u64,
}

/// 跨协议统一的供货商概览。字段按协议能力取值，取不到的留 `None`。
#[derive(Clone, PartialEq)]
pub struct SupplierSnapshot {
    /// 可采购的库存数量。
    pub stock_available: Option<u64>,
    /// 单个 Key 的价格（`kiro-app` 的 `keyPrice`）。
    pub key_price: Option<f64>,
    /// 阶梯定价的最高档单价（`kiroapp-io` 的 `price_max`）。与 `key_price`（最低价）
    /// 一起构成报价区间——单价按母号累计产量分档，下单前算不出确切总价。
    pub key_price_max: Option<f64>,
    /// 剩余额度/积分。
    pub balance: Option<u64>,
    /// 供货商侧登记的回调地址（仅 `kiro-rs` 可读）。
    pub webhook_url: Option<String>,
    pub profile: Option<Profile>,
    pub status: Option<SupplierStatus>,
}

impl fmt::Debug for SupplierSnapshot {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierSnapshot")
            .field("stock_available", &self.stock_available)
            .field("key_price", &self.key_price)
            .field("key_price_max", &self.key_price_max)
            .field("balance", &self.balance)
            .field("webhook_url_configured", &self.webhook_url.is_some())
            .field("profile", &self.profile)
            .field("status", &self.status)
            .finish()
    }
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct SupplierStatus {
    #[serde(default)]
    pub keys_active: u64,
    #[serde(default)]
    pub keys_dead: u64,
    #[serde(default)]
    pub keys_stock: u64,
    #[serde(default)]
    pub generating: bool,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

impl fmt::Debug for SupplierStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SupplierStatus")
            .field("keys_active", &self.keys_active)
            .field("keys_dead", &self.keys_dead)
            .field("keys_stock", &self.keys_stock)
            .field("generating", &self.generating)
            .field("extra_count", &self.extra.len())
            .finish()
    }
}

#[derive(Clone, PartialEq)]
pub struct SupplierKey {
    key: String,
    /// 这一个 key 实际扣了多少（供货商积分）。
    ///
    /// 阶梯定价下同一单里各 key 可能不同价，所以单价必须跟着 key 走而不是按单摊——
    /// 「每存活小时成本」要拿它和该凭据的 `added_at` / `died_at` 相除。
    /// `None` = 该协议不按 key 返回单价。
    price: Option<f64>,
}

impl SupplierKey {
    pub fn into_inner(self) -> String {
        self.key
    }

    pub fn price(&self) -> Option<f64> {
        self.price
    }
}

impl fmt::Debug for SupplierKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 单价不是秘密，但和 key 一起出现容易在日志里被当成整体复制，索性都不打。
        f.write_str("[REDACTED]")
    }
}

#[derive(Clone, PartialEq)]
pub struct Purchase {
    pub client_order_id: String,
    pub purchased: u32,
    /// kiro-rs：剩余可采购额度。kiro-app / kiroapp-io：扣费后剩余积分或库存。
    pub remaining: u64,
    /// 本单实际扣费总额（供货商积分）。kiro-app 的 `pointsCost`、
    /// kiroapp-io 的 `total_debit`。阶梯定价下这是唯一权威数字。
    pub points_cost: Option<u64>,
    /// 本单均价。对方直接给，不必用 `points_cost / purchased` 反推。
    pub unit_price: Option<f64>,
    /// 供货商侧订单号，用于和对方的订单历史对账。仅 kiroapp-io 返回。
    pub supplier_order_id: Option<String>,
    /// 命中对方的幂等重放：上一次其实已经成交，只是响应没回到我们手上。
    pub replayed: bool,
    pub actual_region: Option<SupplierRegion>,
    pub region_source: Option<RegionSource>,
    pub keys: Vec<SupplierKey>,
}

impl fmt::Debug for Purchase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Purchase")
            .field("client_order_id", &self.client_order_id)
            .field("purchased", &self.purchased)
            .field("remaining", &self.remaining)
            .field("points_cost", &self.points_cost)
            .field("unit_price", &self.unit_price)
            .field("supplier_order_id", &self.supplier_order_id)
            .field("replayed", &self.replayed)
            .field("actual_region", &self.actual_region)
            .field("region_source", &self.region_source)
            .field("keys", &"[REDACTED]")
            .finish()
    }
}

#[derive(Deserialize)]
struct PurchaseWire {
    client_order_id: String,
    purchased: u32,
    remaining: u64,
    keys: Vec<KeyWire>,
}
#[derive(Deserialize)]
struct KeyWire {
    key: String,
    /// 这一个 key 实际扣了多少。只有 kiroapp-io 按 key 给价。
    #[serde(default)]
    price: Option<f64>,
}

/// `GET /openapi/stock` → `{availableKeys, keyPrice}`。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroAppStock {
    #[serde(default)]
    available_keys: u64,
    #[serde(default)]
    key_price: Option<f64>,
}

/// `GET /openapi/balance` → `{balance}`。
#[derive(Deserialize)]
struct KiroAppBalance {
    #[serde(default)]
    balance: u64,
}

/// Kiro Drop 把金额编码成**字符串**（`"884.400000"`），不是 JSON 数字。
///
/// 直接复用 `Profile` / `PurchaseWire`（字段是 `u64`）会在反序列化时报 Decode 错误。
/// 采购响应也中招——那意味着钱已经扣了却解析不出 key，所以必须单独一套 wire。
fn parse_decimal_string(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok().filter(|v| v.is_finite())
}

/// 把字符串金额转成对外统一的整数额度。
///
/// Drop 的单位是人民币元且带 6 位小数，而 `Profile` 对外是 `u64`。向下取整而不是
/// 四舍五入：额度用于展示与「够不够买」的判断，宁可少报也不能多报。
fn decimal_string_to_u64(value: &str) -> u64 {
    parse_decimal_string(value)
        .filter(|v| *v >= 0.0)
        .map(|v| v.floor() as u64)
        .unwrap_or(0)
}

/// `GET /api/my/profile`（Drop）→ 金额全是字符串。
#[derive(Deserialize)]
struct KiroDropProfile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    quota: String,
    #[serde(default)]
    remaining: String,
    #[serde(default)]
    used_quota: String,
    #[serde(default)]
    webhook_url: String,
}

/// `GET /api/me/stock`（Drop）→ `{stock, price(字符串), balance(字符串)}`。
///
/// `stock` 才是**可提取**的数量。`/api/status` 的 `keys_stock` 不是同一个东西：它跟着
/// `keys_active` 一起动，我们据它下单时对方却返 500，而它自己的文档说没货该返 404。
///
/// 响应里的 `balance` 不读：余额统一从 `/api/my/profile` 的 `remaining` 取，
/// 两处都读只会多一个可能不一致的来源。
#[derive(Deserialize)]
struct KiroDropStock {
    #[serde(default)]
    stock: u64,
    #[serde(default)]
    price: String,
}

/// `POST /api/my/purchase`（Drop）→ `{client_order_id, purchased, remaining(字符串), keys}`。
///
/// 与 `kiro-rs` 的 `PurchaseWire` 唯一区别是 `remaining` 的类型，但这一个字段就足以
/// 让整个响应解析失败，所以不能共用。
#[derive(Deserialize)]
struct KiroDropPurchase {
    #[serde(default)]
    client_order_id: String,
    #[serde(default)]
    purchased: u32,
    #[serde(default)]
    remaining: String,
    #[serde(default)]
    keys: Vec<KeyWire>,
    /// 本单**实际**出货区域，按文档「始终为完整区域值」（`us-east-1` / `eu-central-1`）。
    /// 原先不解析，于是买到的欧区号一路按美区落库。
    #[serde(default)]
    region: Option<String>,
    /// Drop 侧购买订单 ID。事件记录里带上，出问题能直接跟对方对单。
    #[serde(default)]
    order_id: Option<String>,
}

/// `POST /api/my/purchase`（kiro.ceo）→ `{client_order_id, purchased, remaining,
/// keys:["kiro-xxx", …], zone, unit_price, total_credits, order_id, details:[…]}`。
///
/// `GET /api/my/stock`（kiro.ceo）→ `{max, max_purchase, min, quota, reserved,
/// zones:[{zone, label, unit_price, stock, available, max, enabled}]}`。
///
/// 固定区域概览只读取对应的 `zones` 项；顶层 `max` 是跨区合计，不能表示美国区库存。
#[derive(Deserialize)]
struct KiroCeoStock {
    #[serde(default)]
    zones: Vec<KiroCeoZone>,
}

/// 一个区的报价与可购量。
///
/// 顶层的 `max` 是**跨区合计**，不能当成某一区的可购量用：美国区 0、欧洲区 4 时
/// `max` 也是正数，据它下单而又不指定 `zone`（默认美国区）就一定拿到 409 库存不足。
#[derive(Deserialize)]
struct KiroCeoZone {
    #[serde(default)]
    zone: String,
    #[serde(default)]
    unit_price: Option<f64>,
    /// 本区当前可购数量。线上字段是 `available`，`max` 是本区允许的单笔上限。
    #[serde(default)]
    available: u64,
    #[serde(default)]
    max: u64,
    /// 关闭的区不能买。缺省按可用处理：老版本响应没有这个字段。
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_true() -> bool {
    true
}

impl KiroCeoZone {
    /// 本区这次实际能买几个。`available` 是本区库存，`max` 是本区单笔上限；
    /// 上限为 0 视为「没设上限」而不是「不许买」，否则老版本响应会被判成全区无货。
    fn purchasable(&self) -> u64 {
        if self.max == 0 {
            self.available
        } else {
            self.available.min(self.max)
        }
    }
}

/// 挑一个能买的区：**只看真有货且启用的区，取单价最低的那个**。
///
/// 取最低价而不是固定美国区：区之间价格不同（线上美国区 20、欧洲区 15），而且
/// 单价上限那道闸也只有拿到真正要成交的那个区的价才有意义。
fn pick_kiro_ceo_zone(zones: &[KiroCeoZone]) -> Option<&KiroCeoZone> {
    zones
        .iter()
        .filter(|zone| zone.enabled && zone.purchasable() > 0 && !zone.zone.trim().is_empty())
        .min_by(|left, right| match (left.unit_price, right.unit_price) {
            (Some(a), Some(b)) => a.total_cmp(&b),
            // 报不出价的区排在后面：宁可买知道价的那个。
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        })
}

/// `keys` 的元素两种形状都要接。对方文档写的是**纯字符串数组**
/// （`["kiro-xxx", …]`），线上实际返回的是**对象数组**：
/// `{key, account, password, issuer_url, zone, aws_region, status, created_at}`。
///
/// 只按文档接过一次，代价是线上连丢 7 单：响应键名按字母序排列，`"keys":[` 的
/// `[` 正好落在第 62 列，于是 `Vec<String>` 报
/// `invalid type: map, expected a string at line 1 column 62`——而积分**已经扣了**，
/// key 却因为解析失败进不了库。两种形状都接就不会再因为对方改形状而丢钱。
#[derive(Deserialize)]
#[serde(untagged)]
enum KiroCeoKey {
    /// 文档描述的形状。
    Plain(String),
    /// 线上实际形状。只取 `key`；账号密码等字段由后续凭据导入自行处理。
    Detailed { key: String },
}

impl KiroCeoKey {
    fn into_key(self) -> String {
        match self {
            Self::Plain(key) => key,
            Self::Detailed { key } => key,
        }
    }
}

/// `POST /api/my/purchase`（kiro.ceo）的响应。
#[derive(Deserialize)]
struct KiroCeoPurchase {
    #[serde(default)]
    client_order_id: String,
    #[serde(default)]
    purchased: u32,
    #[serde(default)]
    remaining: u64,
    #[serde(default)]
    keys: Vec<KiroCeoKey>,
    /// 本单权威扣费总额（积分）。
    #[serde(default)]
    total_credits: Option<u64>,
    /// 该区单价（积分/个）。
    #[serde(default)]
    unit_price: Option<f64>,
    /// 对方订单号，用于和 `/api/my/purchase-orders` 对账。
    #[serde(default)]
    order_id: Option<String>,
    /// 命中对方的幂等重放：上一单其实已经成交，钱早扣了。重放不会二次扣费，
    /// 所以失败订单可以靠同一个 `client_order_id` 把已付费的 key 捞回来。
    #[serde(default)]
    replayed: bool,
    #[serde(default)]
    zone: Option<SupplierRegion>,
}

/// `GET /api/me/stock` → `{stock, price, price_min, price_max, balance}`。
///
/// `price` 是向后兼容的别名（等于 `price_min`），阶梯定价下只是**最低**价，
/// 不能用来预估总价。
#[derive(Deserialize)]
struct KiroAppIoStock {
    #[serde(default)]
    stock: u64,
    #[serde(default)]
    price_min: Option<f64>,
    #[serde(default)]
    price_max: Option<f64>,
    #[serde(default)]
    price: Option<f64>,
    #[serde(default)]
    balance: u64,
    #[serde(default)]
    stock_us: u64,
    #[serde(default)]
    stock_eu: u64,
}

/// `POST /api/me/purchase` → `{purchased, requested, remaining, unit_price, total_debit,
/// order_id, keys:[{key, account, password, issuer_url, price}], replayed}`。
///
/// 注意它**不回显** `client_order_id`（只有自己的 `order_id`），所以没有回显比对可做。
#[derive(Deserialize)]
struct KiroAppIoPurchase {
    #[serde(default)]
    purchased: u32,
    #[serde(default)]
    remaining: u64,
    /// 本单实际扣费总额，权威数字。阶梯定价下不能用 `unit_price × count` 反推。
    #[serde(default)]
    total_debit: Option<u64>,
    /// 本单均价 = `total_debit / purchased`。对方算好给我们。
    #[serde(default)]
    unit_price: Option<f64>,
    /// 对方自己的订单号，拿去 `/api/me/orders` 对账。
    #[serde(default)]
    order_id: Option<String>,
    #[serde(default)]
    keys: Vec<KeyWire>,
    /// 幂等重放标记。重放返回的是原响应，不重复扣款。
    #[serde(default)]
    replayed: bool,
}

/// `POST /openapi/claim` → `{key, pointsCost, balance}`（单个）
/// 或 `{keys:[...], pointsCost, balance}`（批量）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KiroAppClaim {
    #[serde(default)]
    key: Option<String>,
    #[serde(default)]
    keys: Option<Vec<String>>,
    /// 本次消耗的积分。车主自投凭据产出的 key 为 0。
    #[serde(default)]
    points_cost: Option<u64>,
    /// 扣费后的剩余积分。
    #[serde(default)]
    balance: Option<u64>,
}

/// `{ "error": { "type", "message" } }` 统一错误信封。
#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ErrorBody {
    /// 机器可判定的错误类型，例如 `out_of_stock` / `rate_limit_exceeded`。
    #[serde(default, rename = "type")]
    kind: Option<String>,
    /// 对方文档写的是 `retryAfter`；`alias` 兼容 snake_case 变体。
    #[serde(default, alias = "retry_after")]
    retry_after: Option<u64>,
}

/// 从限流响应里取 `retryAfter`（秒）。取不到就 `None`，不影响主流程。
fn retry_after_seconds(body: &str) -> Option<u64> {
    serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error.retry_after)
}

/// 取 `error.type`。对方文档要求按这个字段判定，不要依赖错误文案。
fn error_type(body: &str) -> Option<String> {
    serde_json::from_str::<ErrorEnvelope>(body)
        .ok()
        .and_then(|envelope| envelope.error.kind)
}

/// 严格校验：必须是 `ksk_` 前缀。用于 kiro-rs——它的响应格式已验证过。
fn validate_keys(
    keys: impl IntoIterator<Item = (String, Option<f64>)>,
) -> Result<Vec<SupplierKey>, SupplierError> {
    keys.into_iter()
        .map(|(key, price)| {
            let key = key.trim().to_owned();
            if !key.starts_with("ksk_") || key.len() <= "ksk_".len() {
                Err(SupplierError::invalid(
                    "purchase response contains an invalid key",
                ))
            } else {
                Ok(SupplierKey { key, price })
            }
        })
        .collect()
}

/// 宽松校验：只要非空就收下，前缀不像 Kiro Key 时打日志告警。
///
/// 为什么不严格：kiroapp 的 claim **已经扣了积分**才返回 key。此时因为前缀不符合
/// 我们的预期就整单报错，等于钱花了、key 扔了、还得人工去对方后台捞。对方文档里的
/// 示例是 `"key-1"` / `"实际的 Kiro Key"`，并没有承诺 `ksk_` 前缀，所以这里
/// 不能拿前缀当硬门槛——真正的有效性由后续凭据导入去判。
fn accept_paid_keys(
    keys: impl IntoIterator<Item = (String, Option<f64>)>,
) -> Result<Vec<SupplierKey>, SupplierError> {
    let mut accepted = Vec::new();
    let mut unexpected_prefix = 0_usize;
    for (key, price) in keys {
        let key = key.trim().to_owned();
        if key.is_empty() {
            continue;
        }
        if !key.starts_with("ksk_") {
            unexpected_prefix += 1;
        }
        accepted.push(SupplierKey { key, price });
    }
    if unexpected_prefix > 0 {
        // 只报个数，绝不打 key 本身。
        tracing::warn!(
            count = unexpected_prefix,
            "kiroapp claim 返回的 key 不是 ksk_ 前缀；已照收（积分已扣），请确认是否可用"
        );
    }
    if accepted.is_empty() {
        return Err(SupplierError::invalid("claim response contains no keys"));
    }
    Ok(accepted)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupplierError {
    InvalidInput(String),
    /// 该协议不支持这个操作（例如 kiro-app 无法远程注册 webhook）。
    Unsupported(String),
    /// 库存被别人抢完了。正常竞争结果，不是故障，不该重试也不该记成失败。
    OutOfStock,
    /// 有货但积分不够买。重试无用，得先充值——所以和普通 HTTP 错误分开记。
    InsufficientBalance(String),
    /// 同一 `client_order_id` 换了参数（409）。对幂等协议这意味着**上一单已经成交**：
    /// 钱已经扣、key 已经出货，只是这次的参数和原单对不上。
    ///
    /// 必须和普通 HTTP 错误分开：记成失败会让人反复点 retry，而每次都拿到同一个 409，
    /// 付过钱的 key 一直留在对方账上没人去捞。
    OrderConflict(String),
    /// 409，但含义不止「原单已成交」。kiro.ceo 用同一个码表示库存不足、已达最大持有
    /// 库存上限、以及幂等键撞单——前两种没扣钱。只有对方原文能分辨，所以原样带走。
    StateConflict(String),
    Http {
        status: u16,
        message: String,
    },
    /// 供货商限流。绝不重试，`retry_after` 是对方给的建议等待秒数。
    RateLimited {
        retry_after: Option<u64>,
        message: String,
    },
    Network(String),
    Decode(String),
}

impl SupplierError {
    fn invalid(message: &str) -> Self {
        Self::InvalidInput(message.to_owned())
    }
    fn http(status: u16, body: &str, secret: &str) -> Self {
        Self::Http {
            status,
            message: sanitize(body, secret),
        }
    }
    fn rate_limited(body: &str, retry_after: Option<u64>, secret: &str) -> Self {
        Self::RateLimited {
            retry_after,
            message: sanitize(body, secret),
        }
    }
    fn network(message: &str, secret: &str) -> Self {
        Self::Network(sanitize(message, secret))
    }
    fn decode(message: &str, secret: &str) -> Self {
        Self::Decode(sanitize(message, secret))
    }
}

impl fmt::Display for SupplierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) => write!(f, "invalid input: {message}"),
            Self::Unsupported(message) => write!(f, "unsupported operation: {message}"),
            Self::OutOfStock => f.write_str("supplier is out of stock"),
            Self::InsufficientBalance(message) => {
                write!(f, "supplier balance is insufficient: {message}")
            }
            Self::OrderConflict(message) => {
                write!(
                    f,
                    "supplier order already settled with different parameters: {message}"
                )
            }
            Self::StateConflict(message) => {
                write!(
                    f,
                    "supplier rejected the order as a state conflict: {message}"
                )
            }
            Self::Http { status, message } => write!(f, "supplier HTTP {status}: {message}"),
            Self::RateLimited {
                retry_after,
                message,
            } => match retry_after {
                Some(seconds) => {
                    write!(
                        f,
                        "supplier rate limited (retry after {seconds}s): {message}"
                    )
                }
                None => write!(f, "supplier rate limited: {message}"),
            },
            Self::Network(message) => write!(f, "supplier network error: {message}"),
            Self::Decode(message) => write!(f, "supplier response error: {message}"),
        }
    }
}
impl std::error::Error for SupplierError {}

fn validate_http_url(value: &str) -> Result<(), SupplierError> {
    let url = Url::parse(value)
        .map_err(|_| SupplierError::invalid("webhook_url must be a valid http(s) URL"))?;
    if matches!(url.scheme(), "http" | "https") {
        Ok(())
    } else {
        Err(SupplierError::invalid("webhook_url must use http or https"))
    }
}

fn sanitize(value: &str, secret: &str) -> String {
    static TOKEN: OnceLock<regex::Regex> = OnceLock::new();
    let token = TOKEN.get_or_init(|| regex::Regex::new(r#"ksk_[^\s"'<>]*"#).unwrap());
    let replaced_secret = value.replace(secret, "[REDACTED]");
    let redacted = token.replace_all(&replaced_secret, "[REDACTED]");
    redacted.chars().take(300).collect()
}
