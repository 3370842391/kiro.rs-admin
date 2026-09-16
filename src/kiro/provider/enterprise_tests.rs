use super::*;
use crate::model::config::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct TestEndpoint {
    name: &'static str,
    url: String,
    slow_preparation_started: Option<Arc<std::sync::atomic::AtomicBool>>,
}

impl KiroEndpoint for TestEndpoint {
    fn name(&self) -> &'static str {
        self.name
    }
    fn protocol(&self) -> &'static str {
        "ide"
    }
    fn api_url(&self, _: &RequestContext<'_>) -> String {
        format!("{}/{}", self.url, self.name)
    }
    fn mcp_url(&self, ctx: &RequestContext<'_>) -> String {
        self.api_url(ctx)
    }
    fn decorate_api(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        req.bearer_auth(ctx.token)
    }
    fn decorate_mcp(
        &self,
        req: reqwest::RequestBuilder,
        ctx: &RequestContext<'_>,
    ) -> reqwest::RequestBuilder {
        self.decorate_api(req, ctx)
    }
    fn transform_api_body(&self, body: &str, _: &RequestContext<'_>) -> String {
        if body == "slow preparation" {
            if let Some(started) = &self.slow_preparation_started {
                started.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(300));
            }
        }
        body.to_string()
    }
    fn transform_mcp_body(&self, body: &str, ctx: &RequestContext<'_>) -> String {
        self.transform_api_body(body, ctx)
    }
}

fn frame(event_type: &str, payload: &str) -> Vec<u8> {
    typed_frame("event", ":event-type", event_type, payload)
}

fn exception_frame(exception_type: &str, payload: &str) -> Vec<u8> {
    typed_frame("exception", ":exception-type", exception_type, payload)
}

fn typed_frame(message_type: &str, type_header: &str, event_type: &str, payload: &str) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [(":message-type", message_type), (type_header, event_type)] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&((16 + headers.len() + payload.len()) as u32).to_be_bytes());
    bytes.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&crate::kiro::parser::crc::crc32(&bytes).to_be_bytes());
    bytes.extend(headers);
    bytes.extend_from_slice(payload.as_bytes());
    bytes.extend_from_slice(&crate::kiro::parser::crc::crc32(&bytes).to_be_bytes());
    bytes
}

type Calls = Arc<Mutex<Vec<(bool, String)>>>;

pub(crate) async fn single_enterprise_error_for_test(status: u16, body: &[u8], mcp: bool) -> anyhow::Error {
    let (url, _) = server(status, body.to_vec(), 200).await;
    let (provider, manager) = provider(&url, false, true);
    manager.set_enterprise_max_retries(1).unwrap();
    let response = if mcp { provider.call_mcp("{}").await }
        else { provider.call_api("{}", None, None).await };
    response.err().expect("fake upstream must reject the request")
}

pub(crate) async fn shared_enterprise_wait_error_for_test(status: u16) -> (anyhow::Error, usize) {
    let (url, calls) = server_with_responses(move |_, _| TestResponse {
        retry_after: Some("10"),
        ..TestResponse::new(status, b"USER_REQUEST_RATE_EXCEEDED 429".to_vec())
    }).await;
    let (provider, manager) = provider_with_settings(&url, false, true,
        crate::model::config::EnterpriseRetrySettings {
            first_event_timeout_ms: 500, total_timeout_ms: 1_500, ..Default::default()
        });
    manager.set_enterprise_max_retries(1).unwrap();
    let first = provider.call_api("{}", None, None).await.err().unwrap();
    assert!(first.to_string().contains(&format!("enterprise upstream {status}")), "fixture must receive the actual status before testing shared wait: {first}");
    let error = provider.call_mcp("{}").await.err().expect("shared wait exceeds remaining budget");
    let sent = calls.lock().len();
    (error, sent)
}

pub(crate) async fn enterprise_later_failure_for_test(last_status: u16) -> anyhow::Error {
    let sequence = AtomicU32::new(0);
    let (url, _) = server_with_responses(move |_, _| {
        let status = if sequence.fetch_add(1, Ordering::Relaxed) == 0 { 429 } else { last_status };
        TestResponse::new(status, b"temporary upstream failure 429".to_vec())
    }).await;
    let (provider, _) = provider(&url, false, true);
    provider.call_api("{}", None, None).await.err().unwrap()
}

pub(crate) async fn enterprise_incomplete_429_for_test() -> (anyhow::Error, anyhow::Error, usize) {
    let (url, calls) = server_with_responses(|_, _| TestResponse {
        retry_after: Some("10"),
        tail_delay: Duration::from_secs(5),
        ..TestResponse::new(429, br#"{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#.to_vec())
    }).await;
    let (provider, manager) = provider_with_settings(&url, false, true,
        crate::model::config::EnterpriseRetrySettings {
            first_event_timeout_ms: 200, total_timeout_ms: 750, ..Default::default()
        });
    manager.set_enterprise_max_retries(1).unwrap();
    let first = provider.call_api("{}", None, None).await.err().unwrap();
    assert!(first.to_string().contains("enterprise_error_body_timeout"), "fixture must time out the partial body rather than response headers: {first}");
    let second = provider.call_mcp("{}").await.err().unwrap();
    let sent = calls.lock().len();
    (first, second, sent)
}

pub(crate) async fn enterprise_wait_after_actual_error_for_test(last_status: u16, rpm_limit: u32, total_timeout_ms: u64) -> (anyhow::Error, usize) {
    let sequence = AtomicU32::new(0);
    let response_deadline = Arc::new(Mutex::new(None::<tokio::time::Instant>));
    let server_deadline = response_deadline.clone();
    let (url, calls) = server_with_responses(move |_, _| {
        let attempt = sequence.fetch_add(1, Ordering::Relaxed);
        let status = if attempt == 0 { 429 } else { last_status };
        // 首个429在deadline前180ms返回；短退避后收到即时503，余下时间不足以再等一次。
        let header_delay = if attempt == 0 && last_status == 503 {
            server_deadline.lock().unwrap()
                .saturating_duration_since(tokio::time::Instant::now())
                .saturating_sub(Duration::from_millis(180))
        } else { Duration::ZERO };
        TestResponse { header_delay, ..TestResponse::new(status, b"temporary upstream failure".to_vec()) }
    }).await;
    let (provider, manager) = provider_with_settings(&url, false, true,
        crate::model::config::EnterpriseRetrySettings {
            total_timeout_ms, first_event_timeout_ms: total_timeout_ms.min(1_000), ..short_settings()
        });
    manager.set_enterprise_max_retries(3).unwrap();
    manager.set_max_bucket_attempts_per_request(0).unwrap();
    manager.update_credential(1, None, None, None, None, None, None, None, Some(rpm_limit), None, None).unwrap();
    let sink = RequestSink::default();
    *response_deadline.lock() = Some(sink.control.policy(&provider, None, None).unwrap().unwrap().enterprise_deadline);
    let error = provider.call_api("{}", Some(&sink), None).await.err().unwrap();
    let sent = calls.lock().len();
    (error, sent)
}

struct TestResponse {
    status: u16,
    retry_after: Option<&'static str>,
    body: Vec<u8>,
    header_delay: Duration,
    tail_delay: Duration,
    close_delay: Duration,
    split_at: usize,
}

impl TestResponse {
    fn new(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            retry_after: None,
            body,
            header_delay: Duration::ZERO,
            tail_delay: Duration::ZERO,
            close_delay: Duration::ZERO,
            split_at: 4,
        }
    }
}

async fn server(status: u16, body: Vec<u8>, personal_status: u16) -> (String, Calls) {
    server_delayed(
        status,
        body,
        personal_status,
        Duration::ZERO,
        Duration::ZERO,
    )
    .await
}

async fn server_delayed(
    status: u16,
    body: Vec<u8>,
    personal_status: u16,
    header_delay: Duration,
    tail_delay: Duration,
) -> (String, Calls) {
    server_with_responses(move |enterprise, _| {
        if enterprise {
            TestResponse {
                header_delay,
                tail_delay,
                ..TestResponse::new(status, body.clone())
            }
        } else {
            TestResponse::new(personal_status, b"personal answer".to_vec())
        }
    })
    .await
}

async fn server_with_responses(
    response_for: impl Fn(bool, &str) -> TestResponse + Send + Sync + 'static,
) -> (String, Calls) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let calls: Calls = Default::default();
    let log = calls.clone();
    let response_for = Arc::new(response_for);
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            let log = log.clone();
            let response_for = response_for.clone();
            tokio::spawn(async move {
                let mut received = Vec::new();
                let mut buf = [0u8; 4096];
                while !received.windows(4).any(|w| w == b"\r\n\r\n") {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                let header_end = received.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let content_length = String::from_utf8_lossy(&received[..header_end])
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                    .unwrap_or(0);
                // 完整读掉请求体再关连接，避免 Windows 在有未读数据时发 RST 截断响应。
                while received.len() < header_end + content_length {
                    let Ok(n) = socket.read(&mut buf).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    received.extend_from_slice(&buf[..n]);
                }
                let text = String::from_utf8_lossy(&received);
                let enterprise = text.contains("ksk_enterprise");
                let endpoint = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .trim_start_matches('/')
                    .to_string();
                log.lock().push((enterprise, endpoint.clone()));
                let TestResponse {
                    status,
                    retry_after,
                    body,
                    header_delay,
                    tail_delay,
                    close_delay,
                    split_at,
                } = response_for(enterprise, &endpoint);
                if !header_delay.is_zero() {
                    sleep(header_delay).await;
                }
                // 不发送长度时，HTTP 响应以 EOF 结束；可模拟正文完整但上游一直不关闭。
                let length = if close_delay.is_zero() {
                    format!("content-length: {}\r\n", body.len())
                } else {
                    String::new()
                };
                let retry_after = retry_after
                    .map(|value| format!("retry-after: {value}\r\n"))
                    .unwrap_or_default();
                let headers = format!(
                    "HTTP/1.1 {status} Test\r\n{length}{retry_after}connection: close\r\n\r\n"
                );
                let _ = socket.write_all(headers.as_bytes()).await;
                if !tail_delay.is_zero() {
                    let n = body.len().min(split_at);
                    let _ = socket.write_all(&body[..n]).await;
                    sleep(tail_delay).await;
                    let _ = socket.write_all(&body[n..]).await;
                } else {
                    let _ = socket.write_all(&body).await;
                }
                if !close_delay.is_zero() {
                    sleep(close_delay).await;
                }
                let _ = socket.shutdown().await;
            });
        }
    });
    (url, calls)
}

fn provider(
    url: &str,
    with_personal: bool,
    enterprise: bool,
) -> (KiroProvider, Arc<MultiTokenManager>) {
    provider_with_settings(url, with_personal, enterprise, Default::default())
}

fn provider_with_settings(
    url: &str,
    with_personal: bool,
    enterprise: bool,
    settings: crate::model::config::EnterpriseRetrySettings,
) -> (KiroProvider, Arc<MultiTokenManager>) {
    let mut config = Config::default();
    config.enterprise_special_handling = enterprise;
    config.enterprise_max_retries = 2;
    config.enterprise_retry = settings;
    config.endpoint_mode = EndpointMode::Manual;
    config.load_balancing_mode = "priority".to_string();
    let first = KiroCredentials {
        id: Some(1),
        auth_method: Some("api_key".into()),
        kiro_api_key: Some("ksk_enterprise".into()),
        api_region: Some("us-east-1".into()),
        subscription_title: Some("POWERUSER".into()),
        max_concurrency: 1,
        ..Default::default()
    };
    let mut credentials = vec![first];
    if with_personal {
        credentials.push(KiroCredentials {
            id: Some(2),
            auth_method: Some("api_key".into()),
            kiro_api_key: Some("ksk_personal".into()),
            api_region: Some("us-east-1".into()),
            endpoint: Some("cli".into()),
            priority: 1,
            ..Default::default()
        });
    }
    let manager = Arc::new(MultiTokenManager::new(config, credentials, None, None, true).unwrap());
    let endpoints = ["ide", "runtime", "amazonq", "codewhisperer", "cli"]
        .into_iter()
        .map(|name| {
            (
                name.to_string(),
                Arc::new(TestEndpoint {
                    name,
                    url: url.into(),
                    slow_preparation_started: None,
                }) as Arc<dyn KiroEndpoint>,
            )
        })
        .collect();
    (
        KiroProvider::with_proxy(manager.clone(), None, endpoints, "ide".into(), None),
        manager,
    )
}

#[tokio::test]
async fn enterprise_permanent_429_has_fixed_budget_and_personal_fallback() {
    let (url, calls) = server(429, b"rate limited".to_vec(), 200).await;
    let (provider, _) = provider(&url, true, true);
    let result =
        tokio::time::timeout(Duration::from_secs(1), provider.call_api("{}", None, None)).await;
    assert!(result.is_ok(), "持续 429 必须在固定预算后结束企业阶段");
    let result = result.unwrap().unwrap();
    assert_eq!(result.credential_id, 2);
    assert_eq!(calls.lock().iter().filter(|(ent, _)| *ent).count(), 2);
    assert_eq!(calls.lock().last().unwrap().1, "cli");
}

#[tokio::test]
async fn enterprise_metadata_only_is_not_success() {
    let (url, calls) = server(200, frame("metadataEvent", r#"{"stopReason":""}"#), 200).await;
    let (provider, _) = provider(&url, true, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(result.credential_id, 2, "只有元数据不能冒充企业首字成功");
    assert_eq!(calls.lock().iter().filter(|(ent, _)| *ent).count(), 2);
}

#[tokio::test]
async fn enterprise_quota_fallback_restores_personal_endpoint() {
    let (url, calls) = server(
        400,
        br#"{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#.to_vec(),
        200,
    )
    .await;
    let (provider, manager) = provider(&url, true, true);
    manager
        .set_enterprise_default_endpoint("amazonq".into())
        .unwrap();
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(result.credential_id, 2);
    assert_eq!(
        calls.lock().last().unwrap().1,
        "cli",
        "个人号必须恢复显式端点"
    );
}

#[tokio::test]
async fn enterprise_result_holds_account_permit_until_body_is_dropped() {
    let (url, _) = server(
        200,
        frame("assistantResponseEvent", r#"{"content":"hello"}"#),
        200,
    )
    .await;
    let (provider, manager) = provider(&url, false, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(
        manager.snapshot().entries[0].in_flight,
        1,
        "返回响应不能提前释放活跃流的许可"
    );
    drop(result);
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[tokio::test]
async fn enterprise_valid_frame_prefix_is_replayed_exactly_once() {
    let body = frame("assistantResponseEvent", r#"{"content":"hello"}"#);
    let (url, _) = server(200, body.clone(), 200).await;
    let (provider, manager) = provider(&url, false, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        body.as_slice()
    );
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[derive(Default)]
struct RequestSink {
    control: EnterpriseRequestControl,
    attempts: Mutex<Vec<TraceAttempt>>,
}
impl TraceSink for RequestSink {
    fn on_attempt(&self, attempt: TraceAttempt) {
        self.attempts.lock().push(attempt);
    }
    fn enterprise_request_control(&self) -> Option<&EnterpriseRequestControl> {
        Some(&self.control)
    }
}

fn short_settings() -> crate::model::config::EnterpriseRetrySettings {
    crate::model::config::EnterpriseRetrySettings {
        first_event_timeout_ms: 40,
        total_timeout_ms: 500,
        ..Default::default()
    }
}

#[tokio::test]
async fn enterprise_mcp_429_exhausts_fixed_budget() {
    let (url, calls) = server_with_responses(|enterprise, _| {
        if enterprise { TestResponse::new(429, b"rate limited".to_vec()) }
        else { TestResponse::new(200, br#"{"jsonrpc":"2.0","id":"r","result":{"content":[],"isError":false}}"#.to_vec()) }
    }).await;
    let (provider, manager) = provider_with_settings(&url, true, true, short_settings());
    manager
        .set_retry_policy(
            RetryMode::Custom,
            Some(RetryPolicy {
                rate_limit_cooldown_ms: 0,
                max_request_retries: 1,
                base_backoff_ms: 50,
                max_backoff_ms: 50,
                credential_switch_on_429: true,
                respect_retry_after: true,
            }),
        )
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), provider.call_mcp("{}")).await;
    assert!(result.is_ok(), "MCP 持续429必须能耗尽企业预算并兜底");
    assert!(result.unwrap().is_ok());
    assert_eq!(calls.lock().iter().filter(|(ent, _)| *ent).count(), 2);
}

#[tokio::test]
async fn enterprise_slow_headers_and_partial_body_share_first_event_deadline() {
    for (headers, tail, status) in [(true, false, 200), (false, true, 200), (false, true, 429)] {
        let body = frame("assistantResponseEvent", r#"{"content":"hello"}"#);
        let (url, calls) = server_delayed(
            status,
            body,
            200,
            if headers {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            },
            if tail {
                Duration::from_secs(5)
            } else {
                Duration::ZERO
            },
        )
        .await;
        let (provider, manager) = provider_with_settings(&url, true, true, short_settings());
        let result =
            tokio::time::timeout(Duration::from_secs(1), provider.call_api("{}", None, None))
                .await
                .expect("慢响应头/半帧/错误体必须受首事件截止时间约束")
                .unwrap();
        assert_eq!(result.credential_id, 2);
        assert_eq!(calls.lock().iter().filter(|(ent, _)| *ent).count(), 2);
        assert_eq!(
            manager
                .snapshot()
                .entries
                .iter()
                .find(|e| e.id == 1)
                .unwrap()
                .in_flight,
            0
        );
    }
}

#[tokio::test]
async fn enterprise_exhaustion_survives_personal_429_and_handler_reentry() {
    let (url, calls) = server(429, b"rate limited".to_vec(), 429).await;
    let (provider, _) = provider_with_settings(&url, true, true, short_settings());
    let sink = RequestSink::default();
    for _ in 0..2 {
        let result = tokio::time::timeout(
            Duration::from_secs(1),
            provider.call_api("{}", Some(&sink), None),
        )
        .await;
        assert!(result.is_ok(), "包含个人号失败时也必须有限退出");
        assert!(result.unwrap().is_err());
    }
    assert_eq!(
        calls.lock().iter().filter(|(ent, _)| *ent).count(),
        2,
        "外层重试不能续企业预算"
    );
}

#[tokio::test]
async fn enterprise_no_personal_pool_returns_bounded_error() {
    let (url, calls) = server(429, b"rate limited".to_vec(), 200).await;
    let (provider, manager) = provider(&url, false, true);
    let result = tokio::time::timeout(Duration::from_secs(1), provider.call_api("{}", None, None))
        .await
        .unwrap();
    assert!(result.is_err());
    assert_eq!(calls.lock().len(), 2);
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[tokio::test]
async fn enterprise_effective_thinking_tool_and_fragmented_frames_are_preserved() {
    for body in [
        frame("reasoningContentEvent", r#"{"text":"thinking"}"#),
        frame(
            "toolUseEvent",
            r#"{"name":"read","toolUseId":"t1","input":"{}"}"#,
        ),
        [
            frame("metadataEvent", r#"{"stopReason":""}"#),
            frame("assistantResponseEvent", r#"{"content":"answer"}"#),
        ]
        .concat(),
    ] {
        let (url, _) = server_delayed(
            200,
            body.clone(),
            200,
            Duration::ZERO,
            Duration::from_millis(5),
        )
        .await;
        let (provider, manager) = provider(&url, false, true);
        let result = provider.call_api_stream("{}", None, None).await.unwrap();
        assert_eq!(manager.snapshot().entries[0].in_flight, 1);
        let stream = result.into_byte_stream();
        futures::pin_mut!(stream);
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            bytes.extend_from_slice(&chunk.unwrap());
        }
        assert_eq!(bytes, body);
        assert_eq!(manager.snapshot().entries[0].in_flight, 0);
    }
}

#[tokio::test]
async fn enterprise_account_success_requires_business_completion() {
    let (url, _) = server(
        200,
        frame("assistantResponseEvent", r#"{"content":"hello"}"#),
        200,
    )
    .await;
    let (provider, manager) = provider(&url, false, true);
    let sink = RequestSink::default();
    let result = provider.call_api("{}", Some(&sink), None).await.unwrap();
    assert_eq!(manager.snapshot().entries[0].success_count, 0);
    let _ = result.collect_bytes().await.unwrap();
    sink.control.complete(true);
    sink.control.complete(true);
    assert_eq!(
        manager.snapshot().entries[0].success_count,
        1,
        "完整成功只记一次"
    );
}

#[tokio::test]
async fn enterprise_cancel_during_headers_stops_retries_and_releases_permit() {
    let (url, calls) = server_delayed(
        200,
        b"long response".to_vec(),
        200,
        Duration::from_secs(5),
        Duration::ZERO,
    )
    .await;
    let (provider, manager) = provider(&url, false, true);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            provider.call_api("{}", None, None)
        )
        .await
        .is_err()
    );
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
    let sent = calls.lock().len();
    sleep(Duration::from_millis(30)).await;
    assert_eq!(calls.lock().len(), sent);
    assert_eq!(sent, 1);
}

#[tokio::test]
async fn enterprise_four_endpoints_reuse_account_and_accept_last_endpoint() {
    let body = frame(
        "assistantResponseEvent",
        r#"{"content":"fourth endpoint answer"}"#,
    );
    let expected = body.clone();
    let (url, calls) = server_with_responses(move |enterprise, endpoint| {
        if enterprise && endpoint == "codewhisperer" {
            TestResponse::new(200, body.clone())
        } else if enterprise {
            TestResponse::new(429, b"rate limited".to_vec())
        } else {
            TestResponse::new(200, b"personal answer".to_vec())
        }
    })
    .await;
    let (provider, manager) = provider(&url, true, true);
    manager.set_enterprise_max_retries(4).unwrap();
    manager.set_max_bucket_attempts_per_request(0).unwrap();
    let sink = RequestSink::default();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        provider.call_api("{}", Some(&sink), None),
    )
    .await
    .expect("前三个端点 429 后必须在第四次发送时取得成功")
    .unwrap();
    assert_eq!(result.credential_id, 1);
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        expected.as_slice()
    );
    assert_eq!(
        *calls.lock(),
        ["ide", "runtime", "amazonq", "codewhisperer"].map(|name| (true, name.to_string()))
    );
    let attempts = sink.attempts.lock();
    assert_eq!(attempts.len(), 4);
    assert!(
        attempts.iter().all(|attempt| attempt.credential_id == 1),
        "四端点必须复用同一个企业号"
    );
}

#[tokio::test]
async fn enterprise_text_and_content_length_exception_in_one_chunk_replay_without_retry() {
    let body = [
        frame("assistantResponseEvent", r#"{"content":"partial answer"}"#),
        exception_frame(
            "ContentLengthExceededException",
            r#"{"message":"context limit reached"}"#,
        ),
    ]
    .concat();
    let (url, calls) = server(200, body.clone(), 200).await;
    let (provider, manager) = provider(&url, true, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(
        result.credential_id, 1,
        "有效文本后的上下文终止不得抛弃已生成内容"
    );
    assert_eq!(
        result.body_prefix.as_deref(),
        Some(body.as_slice()),
        "预读同一个数据块必须保留正文和终止事件"
    );
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        body.as_slice()
    );
    assert_eq!(*calls.lock(), vec![(true, "ide".into())]);
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[tokio::test]
async fn enterprise_content_filtered_metadata_is_returned_for_handler_without_retry() {
    let body = frame("metadataEvent", r#"{"stopReason":"CONTENT_FILTERED"}"#);
    let upstream_body = body.clone();
    let (url, calls) = server_with_responses(move |enterprise, _| {
        if enterprise {
            TestResponse {
                close_delay: Duration::from_secs(5),
                ..TestResponse::new(200, upstream_body.clone())
            }
        } else {
            TestResponse::new(200, b"personal answer".to_vec())
        }
    })
    .await;
    let (provider, manager) = provider(&url, true, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(result.credential_id, 1, "显式过滤终止应交原 handler 解读");
    let bytes = tokio::time::timeout(Duration::from_millis(500), result.collect_bytes())
        .await
        .expect("显式过滤终止必须立即结束响应，不得等待上游连接关闭")
        .unwrap();
    assert_eq!(bytes.as_ref(), body.as_slice(), "终止元数据必须原样回放");
    let mut decoder = EventStreamDecoder::new();
    decoder.feed(&bytes).unwrap();
    let events = decoder
        .decode_iter()
        .map(|frame| Event::from_frame(frame.unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert!(
        matches!(events.as_slice(), [Event::Metadata(event)] if event.stop_reason == "CONTENT_FILTERED")
    );
    assert_eq!(
        *calls.lock(),
        vec![(true, "ide".into())],
        "内容过滤不得触发企业重复或个人兜底"
    );
    assert_eq!(
        manager.snapshot().entries[0].success_count,
        0,
        "终止事件不是业务成功"
    );
}

#[tokio::test]
async fn enterprise_http_200_authentication_exception_for_api_key_falls_back_once() {
    let body = exception_frame(
        "AuthenticationException",
        r#"{"message":"invalid bearer token"}"#,
    );
    let (url, calls) = server(200, body, 200).await;
    let (provider, manager) = provider(&url, true, true);
    let result = tokio::time::timeout(Duration::from_secs(1), provider.call_api("{}", None, None))
        .await
        .expect("API Key 的认证异常不得尝试刷新或无限重试")
        .unwrap();
    assert_eq!(result.credential_id, 2);
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        b"personal answer"
    );
    assert_eq!(
        *calls.lock(),
        vec![(true, "ide".into()), (false, "cli".into())]
    );
    let snapshot = manager.snapshot();
    let enterprise = snapshot.entries.iter().find(|entry| entry.id == 1).unwrap();
    assert_eq!(
        enterprise.refresh_failure_count, 0,
        "不可刷新的 API Key 不应进入 OAuth 刷新链路"
    );
    assert_eq!(enterprise.success_count, 0);
    assert_eq!(enterprise.in_flight, 0);
}

#[tokio::test]
async fn enterprise_complete_quota_json_falls_back_before_connection_closes() {
    let (url, calls) = server_with_responses(|enterprise, _| {
        if enterprise {
            TestResponse {
                close_delay: Duration::from_secs(5),
                ..TestResponse::new(
                    400,
                    br#"{"reason":"OVERAGE_REQUEST_LIMIT_EXCEEDED"}"#.to_vec(),
                )
            }
        } else {
            TestResponse::new(200, b"personal answer".to_vec())
        }
    })
    .await;
    let settings = crate::model::config::EnterpriseRetrySettings {
        first_event_timeout_ms: 2_000,
        total_timeout_ms: 4_000,
        ..Default::default()
    };
    let (provider, manager) = provider_with_settings(&url, true, true, settings);
    let result = tokio::time::timeout(
        Duration::from_millis(500),
        provider.call_api("{}", None, None),
    )
    .await
    .expect("完整 quota JSON 必须立即分类，不能等待首事件超时或连接 EOF")
    .unwrap();
    assert_eq!(result.credential_id, 2);
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        b"personal answer"
    );
    assert_eq!(
        *calls.lock(),
        vec![(true, "ide".into()), (false, "cli".into())]
    );
    let snapshot = manager.snapshot();
    let enterprise = snapshot.entries.iter().find(|entry| entry.id == 1).unwrap();
    assert_eq!(enterprise.disabled_reason.as_deref(), Some("QuotaExceeded"));
    assert_eq!(enterprise.in_flight, 0);
}

#[tokio::test]
async fn personal_only_group_does_not_inherit_other_groups_enterprise_deadline() {
    let personal_delay = Duration::from_millis(150);
    let (url, calls) = server_with_responses(move |enterprise, _| {
        if enterprise {
            TestResponse::new(429, b"rate limited".to_vec())
        } else {
            TestResponse {
                header_delay: personal_delay,
                ..TestResponse::new(200, b"personal answer".to_vec())
            }
        }
    })
    .await;
    let settings = crate::model::config::EnterpriseRetrySettings {
        first_event_timeout_ms: 10,
        total_timeout_ms: 30,
        ..Default::default()
    };
    let (provider, manager) = provider_with_settings(&url, true, true, settings);
    manager
        .update_credential(
            2,
            None,
            None,
            None,
            None,
            None,
            Some(vec!["personal-only".into()]),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert!(
        manager.has_available_enterprise(None, None, &HashSet::new()),
        "其他分组仍有可用企业号"
    );
    assert!(!manager.has_available_enterprise(None, Some("personal-only"), &HashSet::new()));
    let start = tokio::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        provider.call_api("{}", None, Some("personal-only")),
    )
    .await
    .expect("个人分组请求应该正常返回")
    .expect("隔离分组中的个人响应不受企业 30ms 总超时限制");
    assert!(start.elapsed() >= personal_delay);
    assert_eq!(result.credential_id, 2);
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        b"personal answer"
    );
    assert_eq!(*calls.lock(), vec![(false, "cli".into())]);
}

#[tokio::test]
async fn enterprise_mcp_missing_or_error_result_never_counts_success_and_exits() {
    for body in [
        br#"{"jsonrpc":"2.0","id":"request-1"}"#.to_vec(),
        br#"{"jsonrpc":"2.0","id":"request-1","result":{"content":[{"type":"text","text":"tool failed"}],"isError":true}}"#.to_vec(),
    ] {
        let (url, calls) = server(200, body.clone(), 200).await;
        let (provider, manager) = provider_with_settings(&url, false, true, short_settings());
        let result = tokio::time::timeout(Duration::from_secs(1), provider.call_mcp("{}")).await
            .expect("缺少 result 或 result.isError 的 MCP 响应必须有限退出");
        assert!(result.is_err(), "HTTP 200 不足以确认 MCP 成功：{}", String::from_utf8_lossy(&body));
        assert_eq!(*calls.lock(), vec![(true, "ide".into()), (true, "ide".into())]);
        assert_eq!(manager.snapshot().entries[0].success_count, 0);
        assert_eq!(manager.snapshot().entries[0].in_flight, 0);
    }
}

#[tokio::test]
async fn enterprise_terminal_metadata_before_refusal_text_is_not_chunk_sensitive() {
    let metadata = frame("metadataEvent", r#"{"stopReason":"CONTENT_FILTERED"}"#);
    let split_at = metadata.len();
    let body = [
        metadata,
        frame(
            "assistantResponseEvent",
            r#"{"content":"I cannot help with that request."}"#,
        ),
    ]
    .concat();
    let expected = body.clone();
    let (url, calls) = server_with_responses(move |_, _| TestResponse {
        split_at,
        tail_delay: Duration::from_millis(15),
        ..TestResponse::new(200, body.clone())
    })
    .await;
    let (provider, _) = provider(&url, false, true);
    let result = provider.call_api("{}", None, None).await.unwrap();
    assert_eq!(
        result.collect_bytes().await.unwrap().as_ref(),
        expected.as_slice(),
        "过滤元数据后的合法拒绝正文不能被提前EOF丢弃"
    );
    assert_eq!(calls.lock().len(), 1);
}

#[tokio::test]
async fn enterprise_capacity_recovery_cannot_bypass_cached_request_policy() {
    let (url, calls) = server(200, Vec::new(), 200).await;
    let (provider, manager) = provider(&url, true, true);
    let held = manager.acquire_context_for_id(1).await.unwrap();
    let permit = manager.in_flight_guard(held.id);
    let sink = RequestSink::default();
    assert!(sink.control.policy(&provider, None, None).unwrap().is_none());
    drop(permit);
    let result = provider.call_api("{}", Some(&sink), None).await.unwrap();
    assert_eq!(result.credential_id, 2, "原请求未进入企业阶段，不能在容量恢复后走普通路径接受企业空200");
    assert_eq!(*calls.lock(), vec![(false, "cli".into())]);
    assert_eq!(manager.snapshot().entries[0].success_count, 0);
}

#[tokio::test]
async fn enterprise_api_and_mcp_share_inbound_budget_and_send_count() {
    let sequence = AtomicU32::new(0);
    let (url, calls) = server_with_responses(move |enterprise, _| {
        let n = sequence.fetch_add(1, Ordering::Relaxed);
        if enterprise && n == 0 { TestResponse::new(200, frame("assistantResponseEvent", r#"{"content":"hello"}"#)) }
        else { TestResponse::new(200, br#"{"jsonrpc":"2.0","id":"r","result":{"content":[],"isError":false}}"#.to_vec()) }
    }).await;
    let (provider, _) = provider(&url, true, true);
    let sink = RequestSink::default();
    let first = provider.call_api("{}", Some(&sink), None).await.unwrap();
    first.collect_bytes().await.unwrap();
    let second = provider.call_mcp_traced("{}", Some(&sink), None).await.unwrap();
    assert_eq!(second.credential_id, 1);
    second.collect_bytes().await.unwrap();
    let third = provider.call_mcp_traced("{}", Some(&sink), None).await.unwrap();
    assert_eq!(third.credential_id, 2, "MCP 不得重新分配企业预算");
    third.collect_bytes().await.unwrap();
    assert_eq!(calls.lock().iter().filter(|(enterprise, _)| *enterprise).count(), 2);
    assert_eq!(sink.control.upstream_call_count(), 3);
}

#[tokio::test]
async fn enterprise_pacing_retry_after_is_shared_before_error_body_and_cancel_releases_permit() {
    for status in [429, 503] {
        for mcp in [false, true] {
            let (url, calls) = server_with_responses(move |_, _| TestResponse {
                retry_after: Some("1"),
                tail_delay: Duration::from_secs(5),
                ..TestResponse::new(status, b"{\"error\":\"busy\"}".to_vec())
            })
            .await;
            let (provider, manager) = provider(&url, false, true);
            manager
                .update_credential(
                    1,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(2),
                    None,
                )
                .unwrap();
            let provider = Arc::new(provider);
            let active = provider.clone();
            let first = tokio::spawn(async move { active.call_api("{}", None, None).await });
            tokio::time::timeout(Duration::from_secs(1), async {
                while calls.lock().is_empty() {
                    sleep(Duration::from_millis(1)).await;
                }
            })
            .await
            .unwrap();
            // 上游已发送响应头，首请求仍在等待慢错误体。
            sleep(Duration::from_millis(25)).await;
            let sink = RequestSink::default();
            let second = tokio::time::timeout(Duration::from_millis(60), async {
                if mcp {
                    provider.call_mcp_traced("{}", Some(&sink), None).await
                } else {
                    provider.call_api("{}", Some(&sink), None).await
                }
            })
            .await;
            assert!(second.is_err(), "第二个入站请求必须等待共享 Retry-After");
            assert_eq!(
                calls.lock().len(),
                1,
                "收到 {status} 响应头后，新 API/MCP 不能继续发送"
            );
            assert_eq!(
                sink.control.upstream_call_count(),
                0,
                "等待不计真实发送次数"
            );
            assert_eq!(
                manager.snapshot().entries[0].in_flight,
                1,
                "取消等待必须释放第二份许可"
            );
            first.abort();
            let _ = first.await;
            assert_eq!(manager.snapshot().entries[0].in_flight, 0);
            sleep(Duration::from_millis(20)).await;
            assert_eq!(calls.lock().len(), 1, "取消之后不能后台重试");
        }
    }
}

#[tokio::test]
async fn enterprise_pacing_api_mcp_rpm_wait_cancel_does_not_reserve_or_consume_budget() {
    let sequence = AtomicU32::new(0);
    let (url, calls) = server_with_responses(move |_, _| {
        if sequence.fetch_add(1, Ordering::Relaxed) == 0 {
            TestResponse::new(
                200,
                frame("assistantResponseEvent", r#"{"content":"hello"}"#),
            )
        } else {
            TestResponse::new(
                200,
                br#"{"jsonrpc":"2.0","id":"r","result":{"content":[],"isError":false}}"#.to_vec(),
            )
        }
    })
    .await;
    let (provider, manager) = provider(&url, false, true);
    manager
        .update_credential(
            1,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(300),
            None,
            None,
        )
        .unwrap();
    manager.set_enterprise_max_retries(1).unwrap();
    let start = tokio::time::Instant::now();
    provider
        .call_api("{}", None, None)
        .await
        .unwrap()
        .collect_bytes()
        .await
        .unwrap();
    let sink = RequestSink::default();
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            provider.call_mcp_traced("{}", Some(&sink), None)
        )
        .await
        .is_err(),
        "API 后 MCP 必须遵守同账号 200ms 最小发送间隔"
    );
    assert_eq!(calls.lock().len(), 1);
    assert_eq!(sink.control.upstream_call_count(), 0);
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
    tokio::time::sleep_until(start + Duration::from_millis(230)).await;
    let result = tokio::time::timeout(
        Duration::from_millis(120),
        provider.call_mcp_traced("{}", Some(&sink), None),
    )
    .await
    .expect("取消等待不能预约下一时隙")
    .expect("取消等待不能消耗唯一发送预算");
    result.collect_bytes().await.unwrap();
    assert_eq!(sink.control.upstream_call_count(), 1);
    assert_eq!(calls.lock().len(), 2);
}

#[tokio::test]
async fn enterprise_pacing_plain_429_backoff_survives_new_inbound_request() {
    let sequence = AtomicU32::new(0);
    let sends = Arc::new(Mutex::new(Vec::new()));
    let recorded = sends.clone();
    let (url, _) = server_with_responses(move |_, _| {
        recorded.lock().push(Instant::now());
        if sequence.fetch_add(1, Ordering::Relaxed) == 0 {
            TestResponse::new(429, b"rate limited".to_vec())
        } else {
            TestResponse::new(
                200,
                frame("assistantResponseEvent", r#"{"content":"recovered"}"#),
            )
        }
    })
    .await;
    let (provider, manager) = provider(&url, false, true);
    manager.set_enterprise_max_retries(1).unwrap();
    assert!(provider.call_api("{}", None, None).await.is_err());
    provider
        .call_api("{}", None, None)
        .await
        .unwrap()
        .collect_bytes()
        .await
        .unwrap();
    let sends = sends.lock();
    assert_eq!(sends.len(), 2);
    assert!(
        sends[1].duration_since(sends[0]) >= Duration::from_millis(95),
        "普通 429 后必须有账号共享短退避"
    );
}

#[tokio::test]
async fn enterprise_pacing_concurrent_requests_compete_for_actual_rpm_slots() {
    let sends = Arc::new(Mutex::new(Vec::new()));
    let recorded = sends.clone();
    let (url, _) = server_with_responses(move |_, _| {
        recorded.lock().push(Instant::now());
        TestResponse::new(
            200,
            frame("assistantResponseEvent", r#"{"content":"hello"}"#),
        )
    })
    .await;
    let (provider, manager) = provider(&url, false, true);
    manager
        .update_credential(
            1,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            Some(300),
            Some(3),
            None,
        )
        .unwrap();
    let request = || async {
        provider
            .call_api("{}", None, None)
            .await
            .unwrap()
            .collect_bytes()
            .await
            .unwrap();
    };
    tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(request(), request(), request());
    })
    .await
    .unwrap();
    let sends = sends.lock();
    assert_eq!(sends.len(), 3);
    for pair in sends.windows(2) {
        assert!(
            pair[1].duration_since(pair[0]) >= Duration::from_millis(180),
            "竞争者醒来后必须重新取得账号发送时隙"
        );
    }
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[tokio::test]
async fn enterprise_pacing_huge_retry_after_exits_without_extra_send_budget() {
    let (url, calls) = server_with_responses(|_, _| TestResponse {
        retry_after: Some("18446744073709551615"),
        ..TestResponse::new(429, b"rate limited".to_vec())
    })
    .await;
    let (provider, manager) = provider_with_settings(&url, false, true, short_settings());
    let first = RequestSink::default();
    let result = tokio::time::timeout(
        Duration::from_secs(1),
        provider.call_api("{}", Some(&first), None),
    )
    .await
    .expect("巨大 Retry-After 不能溢出或越过总截止时间");
    assert!(result.is_err());
    assert_eq!(first.control.upstream_call_count(), 1);
    assert_eq!(
        first.attempts.lock().len(),
        1,
        "无法取得下一时隙不能记为真实发送"
    );
    let second = RequestSink::default();
    assert!(
        provider
            .call_mcp_traced("{}", Some(&second), None)
            .await
            .is_err()
    );
    assert_eq!(second.control.upstream_call_count(), 0);
    assert!(second.attempts.lock().is_empty());
    assert_eq!(calls.lock().len(), 1);
    assert_eq!(manager.snapshot().entries[0].in_flight, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn enterprise_pacing_slow_preparation_cannot_compress_real_api_or_mcp_send_interval() {
    for mcp in [false, true] {
        let sends = Arc::new(Mutex::new(Vec::new()));
        let recorded = sends.clone();
        let (url, _) = server_with_responses(move |_, _| {
            recorded.lock().push(Instant::now());
            let body = if mcp {
                br#"{"jsonrpc":"2.0","id":"r","result":{"content":[],"isError":false}}"#.to_vec()
            } else {
                frame("assistantResponseEvent", r#"{"content":"hello"}"#)
            };
            TestResponse::new(200, body)
        })
        .await;
        let (mut provider, manager) = provider(&url, false, true);
        manager
            .update_credential(
                1,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(300),
                Some(2),
                None,
            )
            .unwrap();
        let preparation_started = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for endpoint in provider.endpoints.values_mut() {
            *endpoint = Arc::new(TestEndpoint {
                name: endpoint.name(),
                url: url.clone(),
                slow_preparation_started: Some(preparation_started.clone()),
            });
        }
        let provider = Arc::new(provider);
        let active = provider.clone();
        let slow = tokio::spawn(async move {
            let response = if mcp {
                active.call_mcp("slow preparation").await
            } else {
                active.call_api("slow preparation", None, None).await
            };
            response.unwrap().collect_bytes().await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            while !preparation_started.load(Ordering::Acquire) {
                sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let response = if mcp {
            provider.call_mcp("{}").await
        } else {
            provider.call_api("{}", None, None).await
        };
        response.unwrap().collect_bytes().await.unwrap();
        slow.await.unwrap();
        let sends = sends.lock();
        assert_eq!(sends.len(), 2);
        assert!(
            sends[1].duration_since(sends[0]) >= Duration::from_millis(180),
            "请求准备必须在发送门之前完成；否则300ms慢准备会把200ms发送间隔挤成100ms，mcp={mcp}"
        );
    }
}
