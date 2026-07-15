//! Kiro API Provider
//!
//! 核心组件，负责与 Kiro API 通信
//! 支持流式和非流式请求
//! 支持多凭据故障转移和重试
//! 支持按凭据级 endpoint 切换不同 Kiro API 端点

use reqwest::{Client, header};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::sleep;

use crate::admin::proxy_pool::{ProxyInFlightGuard, ProxyPoolManager};
use crate::admin::trace_db::{
    TraceAttempt, TraceDiagnosticEvent, TraceSink, outcome, truncate_snippet,
};
use crate::anthropic::converter::normalize_model_id;
use crate::http_client::{ProxyConfig, build_client_with_read_timeout};
use crate::kiro::endpoint::{KiroEndpoint, RequestContext};
use crate::kiro::image_budget::ImageBudgetPolicy;
use crate::kiro::machine_id;
use crate::kiro::model::credentials::KiroCredentials;
use crate::kiro::model::events::Event;
use crate::kiro::model::requests::conversation::{
    ConversationState, CurrentMessage, UserInputMessage,
};
use crate::kiro::model::requests::kiro::KiroRequest;
use crate::kiro::model_capabilities::ModelAvailability;
use crate::kiro::model_catalog::{DynamicModelCatalog, ModelCatalogError};
use crate::kiro::parser::decoder::EventStreamDecoder;
use crate::kiro::token_manager::MultiTokenManager;
use crate::model::config::{RetryMode, RetryPolicy, TlsBackend};
use parking_lot::{Mutex, RwLock};

/// 每个凭据的最大重试次数
const MAX_RETRIES_PER_CREDENTIAL: usize = 3;

/// 总重试次数硬上限（避免无限重试）
///
/// 注：上游 429 多为账号级速率配额（SERVICE_REQUEST_RATE_EXCEEDED），高峰期
/// 多账号同时触顶时，过多重试会在账号间连环撞墙、放大限流。故上限取较小值，
/// 配合 429 专用长退避（见 retry_delay_throttle），被限时尽早返回而非耗尽配额。
const MAX_TOTAL_RETRIES: usize = 4;

/// 可配置 429 策略的总重试次数硬上限。仅非默认策略使用，避免 GreyGunG 的高重试
/// 预设在多账号池里无限放大。
const MAX_POLICY_TOTAL_RETRIES: usize = 30;

/// HTTP Client 缓存容量上限（不含常驻的全局代理 client）。
/// 代理池条目较多时，避免每个不同代理都常驻一个 reqwest::Client 导致内存无界增长。
const CLIENT_CACHE_CAP: usize = 64;

fn is_content_length_threshold_error(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
}

async fn call_with_content_length_retry<T, F, Fut>(
    primary_body: &str,
    threshold_retry_body: Option<&str>,
    mut call: F,
) -> anyhow::Result<T>
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<T>>,
{
    match call(primary_body.to_owned()).await {
        Ok(result) => Ok(result),
        Err(error)
            if is_content_length_threshold_error(&error)
                && threshold_retry_body.is_some_and(|body| body != primary_body) =>
        {
            let retry_body = threshold_retry_body.expect("guarded by is_some_and");
            tracing::warn!(
                primary_bytes = primary_body.len(),
                retry_bytes = retry_body.len(),
                "上游拒绝请求体长度，使用更激进的历史图片压缩结果重试一次"
            );
            call(retry_body.to_owned()).await
        }
        Err(error) => Err(error),
    }
}

/// 带容量上限的 HTTP Client 缓存。
///
/// - key 为 effective proxy 配置（None = 直连/全局回退）
/// - 受保护 key（全局代理对应的 effective 配置）永不被淘汰
/// - 超出容量时按插入顺序淘汰最旧的「非受保护」条目
struct ClientCache {
    map: HashMap<Option<ProxyConfig>, Client>,
    /// 插入顺序（仅记录可淘汰的非受保护 key）
    order: std::collections::VecDeque<Option<ProxyConfig>>,
    /// 受保护、不参与淘汰的 key（全局代理）
    protected: Option<ProxyConfig>,
    cap: usize,
}

impl ClientCache {
    fn new(protected: Option<ProxyConfig>, initial: Client, cap: usize) -> Self {
        let mut map = HashMap::new();
        map.insert(protected.clone(), initial);
        Self {
            map,
            order: std::collections::VecDeque::new(),
            protected,
            cap,
        }
    }

    fn get(&self, key: &Option<ProxyConfig>) -> Option<Client> {
        self.map.get(key).cloned()
    }

    /// 插入新条目，必要时淘汰最旧的非受保护条目
    fn insert(&mut self, key: Option<ProxyConfig>, client: Client) {
        if key == self.protected || self.map.contains_key(&key) {
            self.map.insert(key, client);
            return;
        }
        while self.order.len() >= self.cap {
            if let Some(evict) = self.order.pop_front() {
                self.map.remove(&evict);
            } else {
                break;
            }
        }
        self.order.push_back(key.clone());
        self.map.insert(key, client);
    }
}

/// API 调用结果，附带本次实际命中的上游凭据 ID（用于用量统计）
pub struct KiroCallResult {
    pub response: reqwest::Response,
    pub credential_id: u64,
}

/// Admin 手动响应测试结果。
pub struct CredentialTestResult {
    pub credential_id: u64,
    pub model: String,
    pub success: bool,
    pub latency_ms: u64,
    pub http_status: Option<u16>,
    pub response_snippet: Option<String>,
    pub error: Option<String>,
}

struct ProxyAttemptResult {
    response: reqwest::Response,
    proxy: Option<ProxyConfig>,
}

fn readable_response_snippet_from_bytes(body: &[u8]) -> Option<String> {
    let mut decoder = EventStreamDecoder::new();
    if decoder.feed(body).is_ok() {
        let mut text = String::new();
        let mut errors = Vec::new();

        for result in decoder.decode_iter() {
            let Ok(frame) = result else {
                continue;
            };
            match Event::from_frame(frame) {
                Ok(Event::AssistantResponse(resp)) => text.push_str(&resp.content),
                Ok(Event::Error {
                    error_code,
                    error_message,
                }) => errors.push(format!("{}: {}", error_code, error_message)),
                Ok(Event::Exception {
                    exception_type,
                    message,
                }) => errors.push(format!("{}: {}", exception_type, message)),
                _ => {}
            }
        }

        if !text.trim().is_empty() {
            return truncate_snippet(&text);
        }
        if !errors.is_empty() {
            return truncate_snippet(&errors.join("\n"));
        }
    }

    let fallback = String::from_utf8_lossy(body);
    truncate_snippet(&fallback)
}

fn should_try_next_proxy(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 407 | 502 | 503 | 504)
}

/// Kiro API Provider
///
/// 核心组件，负责与 Kiro API 通信
/// 支持多凭据故障转移和重试机制
/// 按凭据 `endpoint` 字段选择 [`KiroEndpoint`] 实现
pub struct KiroProvider {
    token_manager: Arc<MultiTokenManager>,
    /// Client 缓存：key = effective proxy config, value = reqwest::Client
    /// 不同代理配置的凭据使用不同的 Client，共享相同代理的凭据复用 Client。
    /// 带容量上限淘汰（全局代理 client 常驻），避免代理数量增长导致内存无界增长。
    client_cache: Mutex<ClientCache>,
    /// TLS 后端配置
    tls_backend: TlsBackend,
    /// 流式/请求 Client 的读空闲超时（秒，None = 不设置，保持旧行为）。
    /// 来自 `config.stream_idle_timeout_secs`，让底层在上游首字节前/中途挂死时尽早报错，
    /// 配合流层 idle watchdog 收尾，避免空烧到 720s 绝对超时。
    read_timeout_secs: Option<u64>,
    /// 端点实现注册表（key: endpoint 名称）
    endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
    /// 默认端点名称（凭据未指定 endpoint 时使用）
    default_endpoint: String,
    /// 代理池运行时状态；用于代理健康过滤、均衡、粘性与失败自动禁用。
    proxy_pool: Option<Arc<ProxyPoolManager>>,
    /// 已尝试过 profileArn 解析的凭据 ID（进程内）。
    ///
    /// 避免对「无 Enterprise profile」的账号（如纯 BuilderID）在每次请求都重复调用
    /// `ListAvailableProfiles`。命中真实 ARN 的账号会把 ARN 持久化进凭据，之后
    /// 通过 `streaming_profile_arn()` 直接命中，不再进入解析路径。
    profile_resolution_attempted: Mutex<HashSet<u64>>,
    model_catalog: DynamicModelCatalog,
    image_budget_policy: RwLock<ImageBudgetPolicy>,
}

impl KiroProvider {
    /// 创建带代理配置和端点注册表的 KiroProvider 实例
    ///
    /// # Arguments
    /// * `token_manager` - 多凭据 Token 管理器
    /// * `proxy` - 全局代理配置
    /// * `endpoints` - 端点名 → 实现的注册表（至少包含 `default_endpoint` 对应条目）
    /// * `default_endpoint` - 凭据未显式指定 endpoint 时使用的名称
    pub fn with_proxy(
        token_manager: Arc<MultiTokenManager>,
        proxy: Option<ProxyConfig>,
        endpoints: HashMap<String, Arc<dyn KiroEndpoint>>,
        default_endpoint: String,
        proxy_pool: Option<Arc<ProxyPoolManager>>,
    ) -> Self {
        assert!(
            endpoints.contains_key(&default_endpoint),
            "默认端点 {} 未在 endpoints 注册表中",
            default_endpoint
        );
        let tls_backend = token_manager.config().tls_backend;
        // 读空闲超时：来自 config.stream_idle_timeout_secs（0 = 不设 read timeout，保持旧行为）。
        // 让上游首字节前挂死 / 中途停流时底层读取尽早报错，配合流层 idle watchdog 收尾。
        let read_timeout_secs = match token_manager.config().stream_idle_timeout_secs {
            0 => None,
            secs => Some(secs),
        };
        // 预热：构建全局代理对应的 Client（作为受保护的常驻条目）
        let initial_client =
            build_client_with_read_timeout(proxy.as_ref(), 720, read_timeout_secs, tls_backend)
                .expect("创建 HTTP 客户端失败");
        let client_cache = ClientCache::new(proxy.clone(), initial_client, CLIENT_CACHE_CAP);
        let configured_image_budget = ImageBudgetPolicy {
            enabled: token_manager.config().image_budget_enabled,
            total_base64_budget_bytes: token_manager.config().image_total_base64_budget_bytes,
            history_max_dimension: token_manager.config().image_history_max_dimension,
            history_jpeg_quality: token_manager.config().image_history_jpeg_quality,
            retry_history_max_dimension: token_manager.config().image_retry_history_max_dimension,
            retry_history_jpeg_quality: token_manager.config().image_retry_history_jpeg_quality,
        };
        let image_budget_policy = configured_image_budget.validate().unwrap_or_else(|error| {
            tracing::warn!(%error, "图片预算配置无效，回退内置默认值");
            ImageBudgetPolicy::default()
        });

        Self {
            token_manager,
            client_cache: Mutex::new(client_cache),
            tls_backend,
            read_timeout_secs,
            endpoints,
            default_endpoint,
            proxy_pool,
            profile_resolution_attempted: Mutex::new(HashSet::new()),
            model_catalog: DynamicModelCatalog::default(),
            image_budget_policy: RwLock::new(image_budget_policy),
        }
    }

    pub fn image_budget_policy(&self) -> ImageBudgetPolicy {
        *self.image_budget_policy.read()
    }

    pub fn set_image_budget_policy(&self, policy: ImageBudgetPolicy) -> anyhow::Result<()> {
        *self.image_budget_policy.write() = policy.validate()?;
        Ok(())
    }

    /// 按客户端 Key 分组汇总未禁用凭据真实可用的模型目录。
    pub async fn available_models(
        &self,
        group: Option<&str>,
    ) -> Result<Vec<crate::kiro::model::available_models::UpstreamModel>, ModelCatalogError> {
        let credential_ids = self.token_manager.enabled_credential_ids_in_group(group);
        let manager = Arc::clone(&self.token_manager);
        self.model_catalog
            .models_for(group, credential_ids, move |credential_id| {
                let manager = Arc::clone(&manager);
                async move { manager.get_available_models_for(credential_id).await }
            })
            .await
    }

    #[cfg(test)]
    pub(crate) fn seed_model_catalog_for_test(
        &self,
        group: Option<&str>,
        models: Vec<crate::kiro::model::available_models::UpstreamModel>,
    ) {
        self.model_catalog
            .seed_for_test(group, models, Instant::now());
    }

    async fn model_availability_for(&self, credential_id: u64, model: &str) -> ModelAvailability {
        let now = Instant::now();
        let cached = self.model_catalog.availability(credential_id, model, now);
        if cached != ModelAvailability::Unknown {
            return cached;
        }

        match self
            .token_manager
            .get_available_models_for(credential_id)
            .await
        {
            Ok(response) => {
                let available = response.models.iter().any(|entry| entry.model_id == model);
                self.model_catalog
                    .record_credential_models(credential_id, &response.models, now);
                if available {
                    ModelAvailability::Available
                } else {
                    ModelAvailability::Missing
                }
            }
            Err(error) => {
                tracing::warn!(
                    credential_id,
                    model,
                    error = %error,
                    "模型列表查询失败，按未知能力继续当前请求"
                );
                ModelAvailability::Unknown
            }
        }
    }

    fn client_for_proxy(&self, proxy: Option<ProxyConfig>) -> anyhow::Result<Client> {
        let mut cache = self.client_cache.lock();
        if let Some(client) = cache.get(&proxy) {
            return Ok(client);
        }
        let client = build_client_with_read_timeout(
            proxy.as_ref(),
            720,
            self.read_timeout_secs,
            self.tls_backend,
        )?;
        cache.insert(proxy, client.clone());
        Ok(client)
    }

    /// 流式空闲超时（秒）。`0` = 关闭。
    ///
    /// 供 SSE 流层做显式 idle watchdog：`select!` 每 25s ping 会重建
    /// `body_stream.next()` future，可能重置底层 read_timeout，故不能只靠 HTTP
    /// client 的 read_timeout，需在流层维护一个跨迭代的空闲截止时间。
    ///
    /// 读**运行时**值（token_manager 原子态），使管理面板对 `stream_idle_timeout_secs`
    /// 的修改立即作用于流层 watchdog。注意：HTTP client 的 `.read_timeout()` 在构造时
    /// 已固定（连接池复用），运行时改值只影响流层 watchdog；两者协同兜底，够用。
    pub fn stream_idle_timeout_secs(&self) -> u64 {
        self.token_manager.get_stream_idle_timeout_secs()
    }

    /// 是否在等待 Kiro 上游响应时提前提交 SSE 连接注释。
    pub fn early_stream_handshake(&self) -> bool {
        self.token_manager.config().early_stream_handshake
    }

    /// 是否在客户端请求 thinking 但上游未返回 reasoning 时强制报协议错误。
    pub fn strict_thinking_validation(&self) -> bool {
        self.token_manager.config().strict_thinking_validation
    }

    /// 是否对助手输出做身份归一化（Kiro/AWS → Claude/Anthropic）。见 anthropic::identity。
    pub fn identity_normalization(&self) -> bool {
        self.token_manager.config().identity_normalization
    }

    /// 是否启用受限的本地 `ping -> pong` 健康检查契约。
    pub fn local_ping_response(&self) -> bool {
        self.token_manager.config().local_ping_response
    }

    /// 是否仅对精确的空 user 请求形状启用最小上游兼容文本。
    pub fn empty_user_message_compat(&self) -> bool {
        self.token_manager.get_empty_user_message_compat()
    }

    /// 缓存命中率整形区间（运行时读取 token_manager 原子态），返回 `(min_pct, max_pct)`。
    /// 供 anthropic handler 在 `compute_cache_usage` 产出后注入 [`CacheUsage`]，
    /// 使管理面板对区间的修改立即作用于后续请求的命中率呈现。`(0,0)` = 不整形。
    pub fn cache_hit_rate_bounds(&self) -> (u32, u32) {
        self.token_manager.get_cache_hit_rate_bounds()
    }

    /// 全部已注册端点的「桶名 → 协议族」映射。供 admin 校验运营配置的降级桶链
    /// （桶名必须存在 + 链内桶与主端点同协议）。
    pub fn endpoint_protocols(&self) -> std::collections::HashMap<String, String> {
        self.endpoints
            .iter()
            .map(|(name, ep)| (name.clone(), ep.protocol().to_string()))
            .collect()
    }

    /// 各主端点（凭据默认可路由的端点）的静态默认降级链。键 = 主端点名，
    /// 值 = 其 `fallback_chain()`。供 admin 展示「恢复默认」与当前生效链。
    pub fn default_endpoint_chains(&self) -> std::collections::HashMap<String, Vec<String>> {
        self.endpoints
            .iter()
            .map(|(name, ep)| {
                let chain = ep
                    .fallback_chain()
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>();
                (name.clone(), chain)
            })
            .collect()
    }

    fn global_proxy_candidates(&self) -> Vec<Option<ProxyConfig>> {
        let Some(global) = self.token_manager.proxy() else {
            return vec![None];
        };

        let candidates = ProxyConfig::split_candidates(&global.url);
        if candidates.is_empty() {
            return vec![None];
        }

        let mut out = Vec::new();
        for candidate in candidates {
            if !ProxyConfig::is_supported_entry(&candidate) {
                tracing::warn!("忽略无效全局代理候选: {}", candidate);
                continue;
            }
            let next = ProxyConfig::from_url_with_auth(
                candidate,
                global.username.as_deref(),
                global.password.as_deref(),
            );
            if !out.iter().any(|existing| existing == &next) {
                out.push(next);
            }
        }

        if out.is_empty() { vec![None] } else { out }
    }

    fn proxy_candidates_for(
        &self,
        credential_id: u64,
        credentials: &KiroCredentials,
    ) -> Vec<Option<ProxyConfig>> {
        let global = self.global_proxy_candidates();
        let mut candidates = credentials.effective_proxy_candidates(&global);

        let has_direct = candidates.iter().any(|candidate| candidate.is_none());
        candidates.retain(|candidate| candidate.is_some());

        let proxy_candidates: Vec<ProxyConfig> = candidates.into_iter().flatten().collect();
        let ordered = if let Some(pool) = &self.proxy_pool {
            let mode = self.token_manager.get_proxy_balancing_mode();
            pool.order_candidates(credential_id, proxy_candidates, &mode)
        } else {
            proxy_candidates
        };

        let mut candidates: Vec<Option<ProxyConfig>> = ordered.into_iter().map(Some).collect();

        if self.proxy_pool.is_none() && candidates.len() > 1 {
            let offset = fastrand::usize(..candidates.len());
            candidates.rotate_left(offset);
        }

        // 代理候选随机轮询；直连只作为最后兜底，避免有代理可用时主动绕过代理。
        if has_direct || !candidates.is_empty() {
            candidates.push(None);
        }
        if candidates.is_empty() {
            candidates.push(None);
        }
        candidates
    }

    fn proxy_in_flight_guard(&self, proxy: Option<&ProxyConfig>) -> Option<ProxyInFlightGuard<'_>> {
        self.proxy_pool
            .as_ref()
            .zip(proxy)
            .map(|(pool, proxy)| pool.in_flight_guard(proxy))
    }

    fn report_proxy_success(&self, credential_id: u64, proxy: Option<&ProxyConfig>) {
        if let (Some(pool), Some(proxy)) = (&self.proxy_pool, proxy) {
            pool.report_proxy_success(credential_id, proxy);
        }
    }

    fn report_proxy_failure(&self, credential_id: u64, proxy: Option<&ProxyConfig>) {
        if let (Some(pool), Some(proxy)) = (&self.proxy_pool, proxy) {
            pool.report_proxy_failure(credential_id, proxy);
        }
    }

    /// 用指定 endpoint 构造并发送一次 API 请求，返回原始响应（不读取 body）。
    ///
    /// 从 `call_api_with_retry` 抽出，供主端点与 429 降级后的备用端点共用：
    /// 两者除 endpoint 实现不同外，凭据 / token / machineId / 请求体来源完全一致。
    /// 仅负责「构造 URL/body/header → execute」，成功/失败语义由调用方处理。
    async fn execute_api_request(
        &self,
        endpoint: &Arc<dyn KiroEndpoint>,
        ctx: &crate::kiro::token_manager::CallContext,
        machine_id: &str,
        config: &crate::model::config::Config,
        request_body: &str,
        proxy: Option<ProxyConfig>,
        sink: Option<&dyn TraceSink>,
        attempt: u32,
    ) -> anyhow::Result<reqwest::Response> {
        let rctx = RequestContext {
            credentials: &ctx.credentials,
            token: &ctx.token,
            machine_id,
            config,
        };

        let url = endpoint.api_url(&rctx);
        let body = endpoint.transform_api_body(request_body, &rctx);

        if let Some(sink) = sink {
            sink.on_diagnostic(TraceDiagnosticEvent::UpstreamRequest {
                attempt,
                credential_id: ctx.id,
                endpoint: endpoint.name(),
                body: &body,
            });
        }

        tracing::debug!("使用端点 [{}] POST {}", endpoint.name(), url);
        tracing::debug!(
            credential_id = ctx.id,
            attempt,
            endpoint = endpoint.name(),
            body_bytes = body.len(),
            body_sha256 = %hex::encode(Sha256::digest(body.as_bytes())),
            "实际发送请求体元数据"
        );

        // 复用连接池的热 TLS 连接：从中转到 us-east-1 的 TLS 握手 1-3s，每请求
        // Connection:close 等于废掉 client_for_proxy 的连接池，把整个握手 RTT 计入
        // 首字节。改为 keep-alive 后同代理连接复用，是最大的 TFB 收益。空闲连接
        // 竞态（发请求瞬间上游关闭）由 call_api_with_retry 的网络错误重试兜底。
        let client = self.client_for_proxy(proxy.clone()).map_err(|error| {
            if let Some(sink) = sink {
                sink.on_diagnostic(TraceDiagnosticEvent::NetworkError {
                    attempt,
                    credential_id: ctx.id,
                    endpoint: endpoint.name(),
                    message: &error.to_string(),
                });
            }
            error
        })?;
        let base = client
            .post(&url)
            .body(body)
            .header("content-type", endpoint.content_type())
            .header("Connection", "keep-alive");
        let request = endpoint.decorate_api(base, &rctx);

        // 打印实际发送的请求头（RUST_LOG=debug 时输出，便于排查问题）
        let request = request.build().map_err(|e| {
            let error = anyhow::anyhow!("构建请求失败: {}", e);
            if let Some(sink) = sink {
                sink.on_diagnostic(TraceDiagnosticEvent::NetworkError {
                    attempt,
                    credential_id: ctx.id,
                    endpoint: endpoint.name(),
                    message: &error.to_string(),
                });
            }
            error
        })?;
        if tracing::enabled!(tracing::Level::DEBUG) {
            for (k, v) in request.headers() {
                tracing::debug!("  header {}: {}", k, v.to_str().unwrap_or("<binary>"));
            }
        }
        match client.execute(request).await {
            Ok(response) => Ok(response),
            Err(error) => {
                if let Some(sink) = sink {
                    sink.on_diagnostic(TraceDiagnosticEvent::NetworkError {
                        attempt,
                        credential_id: ctx.id,
                        endpoint: endpoint.name(),
                        message: &error.to_string(),
                    });
                }
                Err(error.into())
            }
        }
    }

    async fn execute_api_request_with_proxy_failover(
        &self,
        endpoint: &Arc<dyn KiroEndpoint>,
        ctx: &crate::kiro::token_manager::CallContext,
        machine_id: &str,
        config: &crate::model::config::Config,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        attempt: u32,
    ) -> anyhow::Result<ProxyAttemptResult> {
        let candidates = self.proxy_candidates_for(ctx.id, &ctx.credentials);
        let candidate_count = candidates.len();
        let mut last_error: Option<anyhow::Error> = None;

        for (idx, proxy) in candidates.into_iter().enumerate() {
            let proxy_for_guard = proxy.clone();
            let _proxy_in_flight = self.proxy_in_flight_guard(proxy_for_guard.as_ref());
            if idx > 0 {
                tracing::info!(
                    "凭据 #{} 使用下一个代理候选重试: {}",
                    ctx.id,
                    proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct")
                );
            }

            match self
                .execute_api_request(
                    endpoint,
                    ctx,
                    machine_id,
                    config,
                    request_body,
                    proxy.clone(),
                    sink,
                    attempt,
                )
                .await
            {
                Ok(response) => {
                    let status = response.status();
                    if should_try_next_proxy(status) {
                        self.report_proxy_failure(ctx.id, proxy.as_ref());
                    }
                    if idx + 1 < candidate_count && should_try_next_proxy(status) {
                        if let Some(sink) = sink {
                            sink.on_diagnostic(TraceDiagnosticEvent::UpstreamResponse {
                                attempt,
                                credential_id: ctx.id,
                                endpoint: endpoint.name(),
                                status: status.as_u16(),
                                body: "",
                            });
                        }
                        tracing::warn!(
                            "凭据 #{} 代理候选 {} 返回 HTTP {}，切换下一个候选",
                            ctx.id,
                            proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct"),
                            status.as_u16()
                        );
                        last_error = Some(anyhow::anyhow!(
                            "proxy candidate returned HTTP {}",
                            status.as_u16()
                        ));
                        continue;
                    }
                    if !should_try_next_proxy(status) {
                        self.report_proxy_success(ctx.id, proxy.as_ref());
                    }
                    return Ok(ProxyAttemptResult { response, proxy });
                }
                Err(err) => {
                    self.report_proxy_failure(ctx.id, proxy.as_ref());
                    tracing::warn!(
                        "凭据 #{} 代理候选 {} 请求发送失败: {}",
                        ctx.id,
                        proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct"),
                        err
                    );
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("没有可用代理候选")))
    }

    async fn execute_mcp_request_with_proxy_failover(
        &self,
        endpoint: &Arc<dyn KiroEndpoint>,
        ctx: &crate::kiro::token_manager::CallContext,
        machine_id: &str,
        config: &crate::model::config::Config,
        request_body: &str,
    ) -> anyhow::Result<reqwest::Response> {
        let rctx = RequestContext {
            credentials: &ctx.credentials,
            token: &ctx.token,
            machine_id,
            config,
        };
        let url = endpoint.mcp_url(&rctx);
        let body = endpoint.transform_mcp_body(request_body, &rctx);
        let candidates = self.proxy_candidates_for(ctx.id, &ctx.credentials);
        let candidate_count = candidates.len();
        let mut last_error: Option<anyhow::Error> = None;

        for (idx, proxy) in candidates.into_iter().enumerate() {
            let proxy_for_guard = proxy.clone();
            let _proxy_in_flight = self.proxy_in_flight_guard(proxy_for_guard.as_ref());
            if idx > 0 {
                tracing::info!(
                    "MCP 凭据 #{} 使用下一个代理候选重试: {}",
                    ctx.id,
                    proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct")
                );
            }
            let base = self
                .client_for_proxy(proxy.clone())?
                .post(&url)
                .body(body.clone())
                .header("content-type", endpoint.content_type())
                .header("Connection", "keep-alive");
            let request = endpoint.decorate_mcp(base, &rctx);
            match request.send().await {
                Ok(response) => {
                    let status = response.status();
                    if should_try_next_proxy(status) {
                        self.report_proxy_failure(ctx.id, proxy.as_ref());
                    }
                    if idx + 1 < candidate_count && should_try_next_proxy(status) {
                        tracing::warn!(
                            "MCP 凭据 #{} 代理候选 {} 返回 HTTP {}，切换下一个候选",
                            ctx.id,
                            proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct"),
                            status.as_u16()
                        );
                        last_error = Some(anyhow::anyhow!(
                            "proxy candidate returned HTTP {}",
                            status.as_u16()
                        ));
                        continue;
                    }
                    if !should_try_next_proxy(status) {
                        self.report_proxy_success(ctx.id, proxy.as_ref());
                    }
                    return Ok(response);
                }
                Err(err) => {
                    self.report_proxy_failure(ctx.id, proxy.as_ref());
                    tracing::warn!(
                        "MCP 凭据 #{} 代理候选 {} 请求发送失败: {}",
                        ctx.id,
                        proxy.as_ref().map(|p| p.url.as_str()).unwrap_or("direct"),
                        err
                    );
                    last_error = Some(err.into());
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("没有可用代理候选")))
    }

    /// 根据凭据选择 endpoint 实现

    fn endpoint_for(&self, credentials: &KiroCredentials) -> anyhow::Result<Arc<dyn KiroEndpoint>> {
        if credentials.is_api_key_credential() {
            let api_region = credentials
                .api_region
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("API Key 凭据缺少必填字段 apiRegion"))?;
            crate::kiro::region::validate_api_region(api_region)?;
        }
        let name = credentials
            .endpoint
            .as_deref()
            .unwrap_or(&self.default_endpoint);
        self.endpoints
            .get(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("未知端点: {}", name))
    }

    /// 在发起请求前，确保 Enterprise / IdC 账号的真实 profileArn 已解析并写入 `ctx`。
    ///
    /// 流式端点强制要求 profileArn；Enterprise / IdC 账号必须先把 BuilderID
    /// 占位符解析为真实 ARN，纯 BuilderID 账号则回退占位符。
    /// 仅对「OAuth 凭据 + profileArn 缺失或为占位符」的账号触发一次上游
    /// `ListAvailableProfiles` 查询（进程内去重）：
    /// - 命中真实 ARN → 写回 `ctx.credentials.profile_arn` 并由 token_manager 持久化；
    ///   之后该凭据的 `streaming_profile_arn()` 直接命中，不再进入此路径。
    /// - 无 Enterprise profile（纯 BuilderID 等）→ 保持占位符回退逻辑，并标记已尝试，
    ///   避免每次请求重复查询。
    async fn ensure_profile_arn(&self, ctx: &mut crate::kiro::token_manager::CallContext) {
        use crate::kiro::model::credentials::is_placeholder_profile_arn;

        if ctx.credentials.is_api_key_credential() {
            return;
        }
        let needs = match ctx.credentials.profile_arn.as_deref() {
            None => true,
            Some(arn) => is_placeholder_profile_arn(arn),
        };
        if !needs {
            // 已有真实 ARN：不再解析，但要确保 api_region 与 ARN 区域一致。
            // 早期版本回填 ARN 时未回填区域，会导致请求发到 config 默认区域
            // （us-east-1）却带着其它区域的 profileArn，上游返回 400。
            if let Some(arn) = ctx.credentials.profile_arn.clone() {
                if let Some(region) = self.token_manager.align_api_region_with_arn(ctx.id, &arn) {
                    // 同步到本次请求的 ctx，使当前请求立即使用正确区域
                    ctx.credentials.api_region = Some(region);
                }
            }
            return;
        }
        // 进程内去重：仅在「拿到上游确定结果」后才标记已尝试，避免一次网络抖动
        // 把账号永久卡在占位符上（重启前不再重试）。
        if self.profile_resolution_attempted.lock().contains(&ctx.id) {
            return;
        }
        match self
            .token_manager
            .resolve_profile_arn_for(ctx.id, &ctx.token)
            .await
        {
            Ok(Some(arn)) => {
                ctx.credentials.profile_arn = Some(arn);
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Ok(None) => {
                // 上游确认该账号无 Enterprise profile（纯 BuilderID 等）：标记已尝试，
                // 后续请求回退到占位符逻辑，不再重复查询。
                self.profile_resolution_attempted.lock().insert(ctx.id);
            }
            Err(e) => {
                // 网络/瞬态错误：不标记，下次请求再试；本次按原 profileArn 继续
                tracing::warn!(
                    "凭据 #{} 解析真实 profileArn 失败（按原 profileArn 继续）: {}",
                    ctx.id,
                    e
                );
            }
        }
    }

    /// 发送非流式 API 请求
    ///
    /// 支持多凭据故障转移（见 [`Self::call_api_with_retry`]）。
    /// `sink` 可选，用于逐跳上报链路追踪。
    pub async fn call_api(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, false, sink, group)
            .await
    }

    /// 发送流式 API 请求
    pub async fn call_api_stream(
        &self,
        request_body: &str,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        self.call_api_with_retry(request_body, true, sink, group)
            .await
    }

    /// 非流式请求在上游明确返回请求体长度阈值错误时，使用预先生成的更小请求体重试一次。
    pub async fn call_api_with_content_length_retry(
        &self,
        primary_body: &str,
        threshold_retry_body: Option<&str>,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        call_with_content_length_retry(primary_body, threshold_retry_body, |body| async move {
            self.call_api(&body, sink, group).await
        })
        .await
    }

    /// 流式请求的请求体长度阈值降级重试；只发生在尚未取得成功响应、因此尚未向客户端
    /// 交付任何模型事件时。
    pub async fn call_api_stream_with_content_length_retry(
        &self,
        primary_body: &str,
        threshold_retry_body: Option<&str>,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        call_with_content_length_retry(primary_body, threshold_retry_body, |body| async move {
            self.call_api_stream(&body, sink, group).await
        })
        .await
    }

    /// 发送 MCP API 请求（WebSearch 等工具调用）
    pub async fn call_mcp(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        self.call_mcp_with_retry(request_body).await
    }

    /// 使用指定凭据发送一次 `hello` 响应测试，不参与凭据故障转移。
    pub async fn test_credential_response(
        &self,
        credential_id: u64,
        model: &str,
    ) -> anyhow::Result<CredentialTestResult> {
        let mapped_model = normalize_model_id(model);
        let conversation_id = uuid::Uuid::new_v4().to_string();
        let state = ConversationState::new(conversation_id.clone())
            .with_agent_continuation_id(conversation_id)
            .with_agent_task_type("vibe")
            .with_chat_trigger_type("MANUAL")
            .with_current_message(CurrentMessage::new(UserInputMessage::new(
                "hello",
                mapped_model.clone(),
            )));
        let request = KiroRequest {
            conversation_state: state,
            profile_arn: None,
            additional_model_request_fields: None,
        };
        let request_body = serde_json::to_string(&request)?;

        let mut ctx = self
            .token_manager
            .acquire_context_for_id(credential_id)
            .await?;
        let _in_flight = self.token_manager.in_flight_guard(ctx.id);
        self.token_manager.record_request(ctx.id);
        self.ensure_profile_arn(&mut ctx).await;

        let config = self.token_manager.config();
        let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);
        let endpoint = self.endpoint_for(&ctx.credentials)?;
        let started = Instant::now();

        let response = match self
            .execute_api_request_with_proxy_failover(
                &endpoint,
                &ctx,
                &machine_id,
                config,
                &request_body,
                None,
                0,
            )
            .await
        {
            Ok(result) => result.response,
            Err(e) => {
                return Ok(CredentialTestResult {
                    credential_id: ctx.id,
                    model: mapped_model,
                    success: false,
                    latency_ms: started.elapsed().as_millis() as u64,
                    http_status: None,
                    response_snippet: None,
                    error: Some(e.to_string()),
                });
            }
        };

        let status = response.status();
        let body = response.bytes().await.unwrap_or_default();
        let success = status.is_success();
        if success {
            self.token_manager.report_success(ctx.id);
        }

        Ok(CredentialTestResult {
            credential_id: ctx.id,
            model: mapped_model,
            success,
            latency_ms: started.elapsed().as_millis() as u64,
            http_status: Some(status.as_u16()),
            response_snippet: readable_response_snippet_from_bytes(&body),
            error: if success {
                None
            } else {
                Some(format!("HTTP {}", status.as_u16()))
            },
        })
    }

    /// 内部方法：带重试逻辑的 MCP API 调用
    async fn call_mcp_with_retry(&self, request_body: &str) -> anyhow::Result<reqwest::Response> {
        let total_credentials = self.token_manager.total_count();
        let (retry_mode, retry_policy) = self.effective_retry_policy()?;
        let max_retries = Self::max_retries(total_credentials, retry_mode, &retry_policy);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut request_throttled_ids: HashSet<u64> = HashSet::new();
        // 会话级 RPM 记账去重（同 call_api_with_retry）
        let mut rpm_recorded: HashSet<u64> = HashSet::new();

        for attempt in 0..max_retries {
            // MCP 调用（WebSearch 等工具）不涉及模型选择，也不参与分组隔离
            let ctx_result = if request_throttled_ids.is_empty() {
                self.token_manager.acquire_context(None, None).await
            } else {
                self.token_manager
                    .acquire_context_excluding(None, None, &request_throttled_ids)
                    .await
            };
            let ctx = match ctx_result {
                Ok(c) => c,
                Err(e) => {
                    last_error = Some(e);
                    continue;
                }
            };
            // least_conn 在途计数守卫：随本次迭代作用域结束自动 -1（具名绑定，勿用裸 `_`）。
            let _in_flight = self.token_manager.in_flight_guard(ctx.id);

            // RPM 记账：本会话首次用到该凭据才记 1 次。
            if rpm_recorded.insert(ctx.id) {
                self.token_manager.record_request(ctx.id);
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    last_error = Some(e);
                    // endpoint 解析失败：记为失败，换下一张凭据
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };

            let response = match self
                .execute_mcp_request_with_proxy_failover(
                    &endpoint,
                    &ctx,
                    &machine_id,
                    config,
                    request_body,
                )
                .await
            {
                Ok(resp) => resp,
                Err(e) => {
                    tracing::warn!(
                        "MCP 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    last_error = Some(e);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };

            let status = response.status();
            let retry_after = Self::retry_after_delay(response.headers(), &retry_policy);

            // 成功响应
            if status.is_success() {
                self.token_manager.report_success(ctx.id);
                return Ok(response);
            }

            // 失败响应
            let body = response.text().await.unwrap_or_default();

            // 402 额度用尽
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 400 Bad Request
            if status.as_u16() == 400 {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 401/403 凭据问题
            if matches!(status.as_u16(), 401 | 403) {
                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!("MCP 请求失败（所有凭据已用尽）: {} {}", status, body);
                }
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                continue;
            }

            // 瞬态错误
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                if status.as_u16() == 429 {
                    let switch_on_ordinary_429 =
                        retry_mode == RetryMode::Failover || retry_policy.credential_switch_on_429;
                    if switch_on_ordinary_429 {
                        request_throttled_ids.insert(ctx.id);
                        if self.token_manager.has_available_excluding(
                            None,
                            None,
                            &request_throttled_ids,
                        ) {
                            if retry_mode != RetryMode::Failover {
                                let cooldown = retry_after.unwrap_or_else(|| {
                                    Duration::from_millis(retry_policy.rate_limit_cooldown_ms)
                                });
                                self.token_manager.report_rate_limited(ctx.id, cooldown);
                            }
                            tracing::info!(
                                "MCP 凭据 #{} 返回普通 429，按 {} 策略优先切换其它凭据",
                                ctx.id,
                                retry_mode
                            );
                            last_error = Some(anyhow::anyhow!(
                                "MCP 请求失败（凭据 #{} 普通 429，已切换其它凭据重试）: {} {}",
                                ctx.id,
                                status,
                                body
                            ));
                            continue;
                        }
                        if retry_mode == RetryMode::Failover && !request_throttled_ids.is_empty() {
                            let keep_excluded = Some(ctx.id);
                            request_throttled_ids.clear();
                            if let Some(id) = keep_excluded {
                                request_throttled_ids.insert(id);
                            }
                        } else if retry_mode != RetryMode::Failover {
                            request_throttled_ids.clear();
                        }
                    }
                }

                tracing::warn!(
                    "MCP 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
                if attempt + 1 < max_retries {
                    let delay = Self::retry_delay_for_status(
                        status,
                        attempt,
                        retry_mode,
                        &retry_policy,
                        retry_after,
                    );
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx
            if status.is_client_error() {
                anyhow::bail!("MCP 请求失败: {} {}", status, body);
            }

            // 兜底
            last_error = Some(anyhow::anyhow!("MCP 请求失败: {} {}", status, body));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!("MCP 请求失败：已达到最大重试次数（{}次）", max_retries)
        }))
    }

    /// 内部方法：带重试逻辑的 API 调用
    ///
    /// 重试策略：
    /// - 每个凭据最多重试 MAX_RETRIES_PER_CREDENTIAL 次
    /// - 总重试次数 = min(凭据数量 × 每凭据重试次数, MAX_TOTAL_RETRIES)
    /// - 硬上限 9 次，避免无限重试
    async fn call_api_with_retry(
        &self,
        request_body: &str,
        is_stream: bool,
        sink: Option<&dyn TraceSink>,
        group: Option<&str>,
    ) -> anyhow::Result<KiroCallResult> {
        // 重试预算按当前请求所属分组的账号数计算，避免小分组按全局账号数获得过多无效重试
        let total_credentials = self.token_manager.total_count_in_group(group).max(1);
        let (retry_mode, retry_policy) = self.effective_retry_policy()?;
        let max_retries = Self::max_retries(total_credentials, retry_mode, &retry_policy);
        let mut last_error: Option<anyhow::Error> = None;
        let mut force_refreshed: HashSet<u64> = HashSet::new();
        let mut request_throttled_ids: HashSet<u64> = HashSet::new();
        let mut model_incompatible_ids: HashSet<u64> = HashSet::new();
        // 会话级 RPM 记账去重：同一凭据在本会话（含 429 重试）只记 1 次 tick；
        // 故障转移到不同凭据时各记 1 次。
        let mut rpm_recorded: HashSet<u64> = HashSet::new();
        let api_type = if is_stream { "流式" } else { "非流式" };

        // 单请求内「备用桶尝试」总次数（跨 attempt 累计），受 max_bucket_attempts_per_request 限制，
        // 防止「链长 × attempt 数」把单请求放大成上百次上游调用。
        let max_bucket_attempts = self.token_manager.max_bucket_attempts_per_request();
        let mut bucket_attempts: usize = 0;

        // 尝试从请求体中提取模型信息
        let model = Self::extract_model_from_request(request_body);

        for attempt in 0..max_retries {
            let attempt_start = Instant::now();
            let mut excluded_ids = request_throttled_ids.clone();
            excluded_ids.extend(model_incompatible_ids.iter().copied());
            // 获取调用上下文（绑定 index、credentials、token）
            let mut ctx = match self
                .token_manager
                .acquire_context_excluding(model.as_deref(), group, &excluded_ids)
                .await
            {
                Ok(c) => c,
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        0,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    continue;
                }
            };
            // least_conn 在途计数守卫：随本次迭代作用域结束自动 -1，覆盖所有退出路径
            // （return/continue/bail!/? 早退）。必须具名绑定，裸 `_` 会立即 Drop。
            let _in_flight = self.token_manager.in_flight_guard(ctx.id);

            // RPM 记账：本会话首次用到该凭据才记 1 次（同凭据重试不再记）。
            if rpm_recorded.insert(ctx.id) {
                self.token_manager.record_request(ctx.id);
            }

            // 确保 Enterprise / IdC 账号的真实 profileArn 已解析（流式端点强制要求）
            self.ensure_profile_arn(&mut ctx).await;

            if let Some(model) = model.as_deref()
                && self.model_availability_for(ctx.id, model).await == ModelAvailability::Missing
            {
                tracing::warn!(
                    credential_id = ctx.id,
                    model,
                    "当前凭据不提供目标模型，切换凭据"
                );
                model_incompatible_ids.insert(ctx.id);
                last_error = Some(anyhow::anyhow!(
                    "MODEL_NOT_AVAILABLE: credential #{} does not provide {}",
                    ctx.id,
                    model
                ));
                if model_incompatible_ids.len() >= total_credentials {
                    anyhow::bail!(
                        "MODEL_NOT_AVAILABLE: requested model is unavailable for configured credentials"
                    );
                }
                continue;
            }

            let config = self.token_manager.config();
            let machine_id = machine_id::generate_from_credentials(&ctx.credentials, config);

            let endpoint = match self.endpoint_for(&ctx.credentials) {
                Ok(e) => e,
                Err(e) => {
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        "",
                        None,
                        outcome::UNKNOWN,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    last_error = Some(e);
                    self.token_manager.report_failure(ctx.id);
                    continue;
                }
            };
            let endpoint_name = endpoint.name();

            let attempt_result = match self
                .execute_api_request_with_proxy_failover(
                    &endpoint,
                    &ctx,
                    &machine_id,
                    config,
                    request_body,
                    sink,
                    attempt as u32,
                )
                .await
            {
                Ok(result) => result,
                Err(e) => {
                    tracing::warn!(
                        "API 请求发送失败（尝试 {}/{}）: {}",
                        attempt + 1,
                        max_retries,
                        e
                    );
                    Self::emit_attempt(
                        sink,
                        attempt,
                        ctx.id,
                        endpoint_name,
                        None,
                        outcome::NETWORK_ERROR,
                        Some(&e.to_string()),
                        attempt_start,
                    );
                    // 网络错误通常是上游/链路瞬态问题，不应导致"禁用凭据"或"切换凭据"
                    // （否则一段时间网络抖动会把所有凭据都误禁用，需要重启才能恢复）
                    last_error = Some(e);
                    if attempt + 1 < max_retries {
                        sleep(Self::retry_delay(attempt)).await;
                    }
                    continue;
                }
            };
            let selected_proxy = attempt_result.proxy.clone();
            let response = attempt_result.response;

            let status = response.status();
            let retry_after = Self::retry_after_delay(response.headers(), &retry_policy);

            // 成功响应
            if status.is_success() {
                tracing::info!(
                    "API 请求成功：凭据 #{} 端点 [{}]（尝试 {}/{}）",
                    ctx.id,
                    endpoint_name,
                    attempt + 1,
                    max_retries
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::SUCCESS,
                    None,
                    attempt_start,
                );
                self.token_manager.report_success(ctx.id);
                return Ok(KiroCallResult {
                    response,
                    credential_id: ctx.id,
                });
            }

            // 失败响应：读取 body 用于日志/错误信息
            let body = response.text().await.unwrap_or_default();
            if let Some(sink) = sink {
                sink.on_diagnostic(TraceDiagnosticEvent::UpstreamResponse {
                    attempt: attempt as u32,
                    credential_id: ctx.id,
                    endpoint: endpoint_name,
                    status: status.as_u16(),
                    body: &body,
                });
            }

            // 402 Payment Required 且额度用尽：禁用凭据并故障转移
            if status.as_u16() == 402 && endpoint.is_monthly_request_limit(&body) {
                tracing::warn!(
                    "API 请求失败（额度已用尽，禁用凭据并切换，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::QUOTA_EXHAUSTED,
                    Some(&body),
                    attempt_start,
                );

                let has_available = self.token_manager.report_quota_exhausted(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 400 Bad Request - 请求问题，重试/切换凭据无意义
            if status.as_u16() == 400 {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(400),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 401/403 - 更可能是凭据/权限问题：计入失败并允许故障转移
            if matches!(status.as_u16(), 401 | 403) {
                tracing::warn!(
                    "API 请求失败（可能为凭据错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::AUTH_FAILED,
                    Some(&body),
                    attempt_start,
                );

                // token 被上游失效：先尝试 force-refresh，每凭据仅一次机会
                if endpoint.is_bearer_token_invalid(&body) && !force_refreshed.contains(&ctx.id) {
                    force_refreshed.insert(ctx.id);
                    tracing::info!("凭据 #{} token 疑似被上游失效，尝试强制刷新", ctx.id);
                    if self
                        .token_manager
                        .force_refresh_token_for(ctx.id)
                        .await
                        .is_ok()
                    {
                        tracing::info!("凭据 #{} token 强制刷新成功，重试请求", ctx.id);
                        continue;
                    }
                    tracing::warn!("凭据 #{} token 强制刷新失败，计入失败", ctx.id);
                }

                let has_available = self.token_manager.report_failure(ctx.id);
                if !has_available {
                    anyhow::bail!(
                        "{} API 请求失败（所有凭据已用尽）: {} {}",
                        api_type,
                        status,
                        body
                    );
                }

                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                continue;
            }

            // 客户端请求格式错误（messages 数组违反协议）：根因在调用方，重试无意义。
            // 上游常以 5xx 返回，必须在下方「多端点降级链」「瞬态重试」之前拦截，否则会被
            // 当作上游故障在多个端点/多次重试里放大成 503 风暴。直接终止，不重试、不换端点、
            // 不切换凭据、不计入凭据失败。
            if endpoint.is_client_validation_error(&body) {
                tracing::warn!(
                    "API 请求失败（客户端请求格式错误，不重试）: {} {}",
                    status,
                    body
                );
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 524 / gateway timeout：上游边缘层超时，继续在本次请求内重试（含换端点）通常只会
            // 放大客户端等待时间和 Claude 端 Retrying 轮数；快速返回，让客户端下一次调用重新建连。
            // 同样必须在多端点降级链之前拦截。
            if status.as_u16() == 524 || endpoint.is_gateway_timeout(&body) {
                tracing::warn!("API 请求失败（上游网关超时，不重试）: {} {}", status, body);
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::TRANSIENT,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 多端点降级链（换桶不换号）：q / runtime.kiro.dev / codewhisperer 等是相互独立
            // 的限流桶，一个不可用时另一个仍可 200。对齐 demo 的多端点重试——在 429/408/5xx
            // 等「换端点可能有用」的瞬态错误上，用**同一张凭据**沿 fallback_chain() 依次重发。
            //
            // 关键：本块必须在下方「账号级风控」「瞬态重试」两个分支**之前**执行。
            // 否则含 "suspicious activity" 的账号级 429 会被风控分支先行拦截、冷却当前凭据并
            // 换号重试——始终停留在同一端点，永远轮不到备用桶，表现为「主端点连续重试几次才切」。
            // 前置后：任何可换端点的错误都先用同一张凭据沿链重发（不计 attempt、不退避、不切凭据）。
            // - 链中某桶成功 → 直接返回（trace 记为该桶 success，可见完整降级链路）。
            // - 整条链都失败 → 落回下方按原始（主端点）响应体分类：账号级风控走冷却换号，
            //   普通瞬态走退避重试；下一轮迭代再以主端点起手，形成桶间来回。
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                // 先对主端点这一跳分类并**立即**发射 trace——必须在备用桶降级之前发射，保证链路里
                // 主端点行排在备用桶行之前，顺序与真实调用一致。下方「账号级风控」「瞬态重试」两个
                // 分支因此不再重复发射本跳，仅保留控制流。
                let account_throttled = status.as_u16() == 429
                    && self.token_manager.get_account_throttle_failover()
                    && endpoint.is_account_throttled(&body);
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    if account_throttled {
                        outcome::ACCOUNT_THROTTLED
                    } else {
                        outcome::TRANSIENT
                    },
                    Some(&body),
                    attempt_start,
                );

                // 沿降级链依次尝试每个备用桶（换桶不换号），命中第一个 2xx 即返回；
                // 整条链都失败才落回下方的账号风控/瞬态重试逻辑。参考 demo 的多端点重试。
                //
                // 降级链来源：运行时覆盖（config.endpointChains，管理面板可改）优先，
                // 未配置该主端点时回退各 endpoint 的静态 fallback_chain()（零行为变化）。
                let fallback_chain: Vec<String> = self
                    .token_manager
                    .endpoint_chain_for(endpoint.name())
                    .unwrap_or_else(|| {
                        endpoint
                            .fallback_chain()
                            .iter()
                            .map(|s| s.to_string())
                            .collect()
                    });
                for fb_name in &fallback_chain {
                    // 单请求桶尝试总数硬上限（跨 attempt 累计）：防止「链长 × attempt 数」
                    // 把单请求放大成上百次上游调用。0 = 不限。
                    if max_bucket_attempts > 0 && bucket_attempts >= max_bucket_attempts {
                        tracing::warn!(
                            "凭据 #{} 已达单请求桶尝试上限 {}，停止降级链",
                            ctx.id,
                            max_bucket_attempts
                        );
                        break;
                    }
                    let Some(fb_endpoint) = self.endpoints.get(fb_name.as_str()).cloned() else {
                        continue;
                    };
                    bucket_attempts += 1;
                    tracing::info!(
                        "端点 [{}] 返回 {}（瞬态），凭据 #{} 降级到备用端点 [{}] 重试（换桶不换号）",
                        endpoint_name,
                        status.as_u16(),
                        ctx.id,
                        fb_name
                    );
                    let fb_start = Instant::now();
                    match self
                        .execute_api_request(
                            &fb_endpoint,
                            &ctx,
                            &machine_id,
                            config,
                            request_body,
                            selected_proxy.clone(),
                            sink,
                            attempt as u32,
                        )
                        .await
                    {
                        Ok(fb_resp) if fb_resp.status().is_success() => {
                            let fb_status = fb_resp.status();
                            Self::emit_attempt(
                                sink,
                                attempt,
                                ctx.id,
                                fb_name,
                                Some(fb_status.as_u16()),
                                outcome::SUCCESS,
                                None,
                                fb_start,
                            );
                            self.token_manager.report_success(ctx.id);
                            tracing::info!(
                                "凭据 #{} 在备用端点 [{}] 成功（主端点 [{}] 此前 429）",
                                ctx.id,
                                fb_name,
                                endpoint_name
                            );
                            return Ok(KiroCallResult {
                                response: fb_resp,
                                credential_id: ctx.id,
                            });
                        }
                        Ok(fb_resp) => {
                            let fb_status = fb_resp.status();
                            let fb_body = fb_resp.text().await.unwrap_or_default();
                            if let Some(sink) = sink {
                                sink.on_diagnostic(TraceDiagnosticEvent::UpstreamResponse {
                                    attempt: attempt as u32,
                                    credential_id: ctx.id,
                                    endpoint: fb_endpoint.name(),
                                    status: fb_status.as_u16(),
                                    body: &fb_body,
                                });
                            }
                            Self::emit_attempt(
                                sink,
                                attempt,
                                ctx.id,
                                fb_name,
                                Some(fb_status.as_u16()),
                                outcome::TRANSIENT,
                                Some(&fb_body),
                                fb_start,
                            );
                            tracing::warn!(
                                "备用端点 [{}] 也失败（{}），尝试链中下一个桶",
                                fb_name,
                                fb_status
                            );
                        }
                        Err(e) => {
                            Self::emit_attempt(
                                sink,
                                attempt,
                                ctx.id,
                                fb_name,
                                None,
                                outcome::NETWORK_ERROR,
                                Some(&e.to_string()),
                                fb_start,
                            );
                            tracing::warn!(
                                "备用端点 [{}] 请求发送失败（{}），尝试链中下一个桶",
                                fb_name,
                                e
                            );
                        }
                    }
                }
                // 整条降级链都失败，落回主端点 429 分类处理。
                let switch_on_ordinary_429 =
                    retry_mode == RetryMode::Failover || retry_policy.credential_switch_on_429;
                if status.as_u16() == 429 && !account_throttled && switch_on_ordinary_429 {
                    request_throttled_ids.insert(ctx.id);
                    if self.token_manager.has_available_excluding(
                        model.as_deref(),
                        group,
                        &request_throttled_ids,
                    ) {
                        if retry_mode != RetryMode::Failover {
                            let cooldown = retry_after.unwrap_or_else(|| {
                                Duration::from_millis(retry_policy.rate_limit_cooldown_ms)
                            });
                            self.token_manager.report_rate_limited(ctx.id, cooldown);
                        }
                        last_error = Some(anyhow::anyhow!(
                            "{} API 请求失败（凭据 #{} 429，备用端点也失败，已切换其它凭据重试）: {} {}",
                            api_type,
                            ctx.id,
                            status,
                            body
                        ));
                        tracing::info!(
                            "凭据 #{} 主/备用端点均返回普通 429，按 {} 策略切换其它凭据",
                            ctx.id,
                            retry_mode
                        );
                        continue;
                    }
                    if retry_mode == RetryMode::Failover && !request_throttled_ids.is_empty() {
                        let keep_excluded = Some(ctx.id);
                        request_throttled_ids.clear();
                        if let Some(id) = keep_excluded {
                            request_throttled_ids.insert(id);
                        }
                        tracing::info!(
                            "本轮可用凭据主/备用端点均返回普通 429，开启下一轮并暂避凭据 #{}。",
                            ctx.id
                        );
                    } else if retry_mode != RetryMode::Failover {
                        request_throttled_ids.clear();
                    }
                }
            }

            // 429 + suspicious activity = 账号级临时风控
            // 仅当前凭据被针对，故障转移到其它凭据可立即恢复（受配置开关控制）。
            if status.as_u16() == 429
                && self.token_manager.get_account_throttle_failover()
                && endpoint.is_account_throttled(&body)
            {
                let cooldown_secs = self
                    .token_manager
                    .get_account_throttle_cooldown_secs()
                    .max(1);
                let cooldown = std::time::Duration::from_secs(cooldown_secs);
                tracing::warn!(
                    "API 请求失败（账号级风控，凭据 #{} 冷却 {}s 并切换，尝试 {}/{}）: {}",
                    ctx.id,
                    cooldown_secs,
                    attempt + 1,
                    max_retries,
                    body
                );

                let remaining = self
                    .token_manager
                    .report_account_throttled(ctx.id, cooldown);
                // 本跳 trace 已在上方 429 降级块统一发射（含 ACCOUNT_THROTTLED 分类），此处不再重发。
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败（账号级风控，凭据 #{} 已冷却 {} 分钟）: {} {}",
                    api_type,
                    ctx.id,
                    cooldown_secs / 60,
                    status,
                    body
                ));

                if remaining == 0 {
                    anyhow::bail!(
                        "{} API 请求失败：所有凭据都处于账号风控冷却或已禁用状态。\
                         上游对凭据 #{} 的账号触发了 \"suspicious activity\" 临时限速，\
                         建议：(1) 增加更多不同 AWS 账号的凭据；\
                         (2) 在管理面板降低冷却时长或手动解除冷却以重试；\
                         (3) 提交 AWS Support 申诉解封该账号。原始响应: {} {}",
                        api_type,
                        ctx.id,
                        status,
                        body
                    );
                }
                continue;
            }

            // 429 + suspicious activity，但账号级风控转移**已关闭**：打日志说明，让开关效果可见。
            // 不冷却、不换号，按普通瞬态 429 落入下方退避重试。
            if status.as_u16() == 429
                && !self.token_manager.get_account_throttle_failover()
                && endpoint.is_account_throttled(&body)
            {
                tracing::warn!(
                    "检测到账号级风控（suspicious activity，凭据 #{}），但账号风控转移已关闭 \
                     (account_throttle_failover=false)，按普通 429 退避重试（不冷却、不换号）",
                    ctx.id
                );
            }

            // 429/408/5xx - 瞬态上游错误：重试但不禁用或切换凭据
            // （避免 429 high traffic / 502 high load 等瞬态错误把所有凭据锁死）
            // 注：这些状态的多端点降级已在上方「账号级风控」分支之前沿链统一处理过；
            // 走到这里说明主端点失败且整条降级链也失败，按瞬态错误退避重试。
            if matches!(status.as_u16(), 408 | 429) || status.is_server_error() {
                tracing::warn!(
                    "API 请求失败（上游瞬态错误，尝试 {}/{}）: {} {}",
                    attempt + 1,
                    max_retries,
                    status,
                    body
                );
                // 本跳 trace 已在上方多端点降级块（408|429|5xx 全部覆盖）统一发射，此处不再重发，
                // 避免重复行。
                last_error = Some(anyhow::anyhow!(
                    "{} API 请求失败: {} {}",
                    api_type,
                    status,
                    body
                ));
                if attempt + 1 < max_retries {
                    let delay = Self::retry_delay_for_status(
                        status,
                        attempt,
                        retry_mode,
                        &retry_policy,
                        retry_after,
                    );
                    sleep(delay).await;
                }
                continue;
            }

            // 其他 4xx - 通常为请求/配置问题：直接返回，不计入凭据失败
            if status.is_client_error() {
                Self::emit_attempt(
                    sink,
                    attempt,
                    ctx.id,
                    endpoint_name,
                    Some(status.as_u16()),
                    outcome::BAD_REQUEST,
                    Some(&body),
                    attempt_start,
                );
                anyhow::bail!("{} API 请求失败: {} {}", api_type, status, body);
            }

            // 兜底：当作可重试的瞬态错误处理（不切换凭据）
            tracing::warn!(
                "API 请求失败（未知错误，尝试 {}/{}）: {} {}",
                attempt + 1,
                max_retries,
                status,
                body
            );
            Self::emit_attempt(
                sink,
                attempt,
                ctx.id,
                endpoint_name,
                Some(status.as_u16()),
                outcome::UNKNOWN,
                Some(&body),
                attempt_start,
            );
            last_error = Some(anyhow::anyhow!(
                "{} API 请求失败: {} {}",
                api_type,
                status,
                body
            ));
            if attempt + 1 < max_retries {
                sleep(Self::retry_delay(attempt)).await;
            }
        }

        // 所有重试都失败
        Err(last_error.unwrap_or_else(|| {
            anyhow::anyhow!(
                "{} API 请求失败：已达到最大重试次数（{}次）",
                api_type,
                max_retries
            )
        }))
    }

    /// 向 trace sink 上报一跳结果（sink 为 None 时无开销）
    #[allow(clippy::too_many_arguments)]
    fn emit_attempt(
        sink: Option<&dyn TraceSink>,
        attempt: usize,
        credential_id: u64,
        endpoint: &str,
        http_status: Option<u16>,
        outcome: &str,
        error_body: Option<&str>,
        started: Instant,
    ) {
        let Some(sink) = sink else { return };
        sink.on_attempt(TraceAttempt {
            attempt: attempt as u32,
            credential_id,
            endpoint: endpoint.to_string(),
            http_status,
            outcome: outcome.to_string(),
            error_snippet: error_body.and_then(truncate_snippet),
            duration_ms: started.elapsed().as_millis() as u64,
        });
    }

    /// 从请求体中提取模型信息
    ///
    /// 尝试解析 JSON 请求体，提取 conversationState.currentMessage.userInputMessage.modelId
    fn extract_model_from_request(request_body: &str) -> Option<String> {
        use serde_json::Value;

        let json: Value = serde_json::from_str(request_body).ok()?;

        json.get("conversationState")?
            .get("currentMessage")?
            .get("userInputMessage")?
            .get("modelId")?
            .as_str()
            .map(|s| s.to_string())
    }

    fn effective_retry_policy(&self) -> anyhow::Result<(RetryMode, RetryPolicy)> {
        let (mode, _, effective) = self.token_manager.get_retry_policy()?;
        Ok((mode, effective))
    }

    fn max_retries(total_credentials: usize, mode: RetryMode, policy: &RetryPolicy) -> usize {
        if mode == RetryMode::Failover {
            (total_credentials.max(1) * MAX_RETRIES_PER_CREDENTIAL).min(MAX_TOTAL_RETRIES)
        } else {
            (total_credentials.max(1) * policy.max_request_retries).min(MAX_POLICY_TOTAL_RETRIES)
        }
    }

    fn retry_after_delay(headers: &header::HeaderMap, policy: &RetryPolicy) -> Option<Duration> {
        if !policy.respect_retry_after {
            return None;
        }

        let value = headers.get(header::RETRY_AFTER)?.to_str().ok()?.trim();
        if let Ok(seconds) = value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }

        if let Ok(date) = httpdate::parse_http_date(value) {
            if let Ok(duration) = date.duration_since(std::time::SystemTime::now()) {
                return Some(duration);
            }
        }

        None
    }

    fn retry_delay(attempt: usize) -> Duration {
        // 指数退避 + 少量抖动，避免上游抖动时放大故障
        const BASE_MS: u64 = 200;
        const MAX_MS: u64 = 2_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    fn retry_delay_policy(attempt: usize, policy: &RetryPolicy) -> Duration {
        let exp = policy
            .base_backoff_ms
            .saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(policy.max_backoff_ms);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }

    fn retry_delay_for_status(
        status: reqwest::StatusCode,
        attempt: usize,
        mode: RetryMode,
        policy: &RetryPolicy,
        retry_after: Option<Duration>,
    ) -> Duration {
        if mode == RetryMode::Failover {
            if status.as_u16() == 429 {
                Self::retry_delay_throttle(attempt)
            } else {
                Self::retry_delay(attempt)
            }
        } else if status.as_u16() == 429 {
            retry_after.unwrap_or_else(|| Self::retry_delay_policy(attempt, policy))
        } else {
            Self::retry_delay_policy(attempt, policy)
        }
    }

    /// 429 限流专用退避：比通用退避更长。
    ///
    /// 上游 429（SERVICE_REQUEST_RATE_EXCEEDED）是账号级速率配额耗尽，需要更长
    /// 时间恢复；用通用的 ≤2s 快速退避只会让请求在配额恢复前反复撞墙、持续触顶。
    /// 这里 base 1s、封顶 8s，给账号配额留出恢复窗口。
    fn retry_delay_throttle(attempt: usize) -> Duration {
        const BASE_MS: u64 = 1_000;
        const MAX_MS: u64 = 8_000;
        let exp = BASE_MS.saturating_mul(2u64.saturating_pow(attempt.min(6) as u32));
        let backoff = exp.min(MAX_MS);
        let jitter_max = (backoff / 4).max(1);
        let jitter = fastrand::u64(0..=jitter_max);
        Duration::from_millis(backoff.saturating_add(jitter))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::parser::crc::crc32;

    fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(name.len() as u8);
        out.extend_from_slice(name.as_bytes());
        out.push(7);
        out.extend_from_slice(&(value.len() as u16).to_be_bytes());
        out.extend_from_slice(value.as_bytes());
        out
    }

    fn event_stream_frame(event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.extend(string_header(":event-type", event_type));
        headers.extend(string_header(":content-type", "application/json"));
        headers.extend(string_header(":message-type", "event"));

        let total_len = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&(total_len as u32).to_be_bytes());
        frame.extend_from_slice(&(headers.len() as u32).to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend(headers);
        frame.extend_from_slice(payload.as_bytes());
        let checksum = crc32(&frame);
        frame.extend_from_slice(&checksum.to_be_bytes());
        frame
    }

    #[test]
    fn readable_response_snippet_decodes_event_stream_assistant_text() {
        let mut body = Vec::new();
        body.extend(event_stream_frame(
            "assistantResponseEvent",
            r#"{"content":"Hello ","modelId":"glm-5"}"#,
        ));
        body.extend(event_stream_frame(
            "assistantResponseEvent",
            r#"{"content":"world","modelId":"glm-5"}"#,
        ));
        body.extend(event_stream_frame(
            "meteringEvent",
            r#"{"unit":"credit","usage":0.1}"#,
        ));

        assert_eq!(
            readable_response_snippet_from_bytes(&body).as_deref(),
            Some("Hello world")
        );
    }

    #[test]
    fn readable_response_snippet_falls_back_to_plain_text() {
        assert_eq!(
            readable_response_snippet_from_bytes(b"{\"message\":\"bad request\"}").as_deref(),
            Some("{\"message\":\"bad request\"}")
        );
    }

    #[tokio::test]
    async fn content_length_error_uses_smaller_body_once() {
        let mut calls = Vec::new();
        let result = call_with_content_length_retry("primary", Some("retry"), |body| {
            calls.push(body.clone());
            async move {
                if body == "primary" {
                    Err(anyhow::anyhow!("CONTENT_LENGTH_EXCEEDS_THRESHOLD"))
                } else {
                    Ok(body)
                }
            }
        })
        .await
        .unwrap();

        assert_eq!(result, "retry");
        assert_eq!(calls, vec!["primary", "retry"]);
    }

    #[tokio::test]
    async fn non_threshold_error_is_not_retried() {
        let mut calls = 0;
        let error = call_with_content_length_retry("primary", Some("retry"), |_body| {
            calls += 1;
            async { Err::<String, _>(anyhow::anyhow!("MODEL_NOT_AVAILABLE")) }
        })
        .await
        .unwrap_err();

        assert!(error.to_string().contains("MODEL_NOT_AVAILABLE"));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn second_content_length_error_is_returned_without_third_call() {
        let mut calls = 0;
        let error = call_with_content_length_retry("primary", Some("retry"), |_body| {
            calls += 1;
            async { Err::<String, _>(anyhow::anyhow!("CONTENT_LENGTH_EXCEEDS_THRESHOLD")) }
        })
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("CONTENT_LENGTH_EXCEEDS_THRESHOLD")
        );
        assert_eq!(calls, 2);
    }
}
