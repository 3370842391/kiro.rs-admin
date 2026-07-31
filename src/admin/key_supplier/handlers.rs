use std::{sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    extract::{
        Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json, Response},
};
use serde::{Deserialize, Serialize};

use crate::admin::{middleware::AdminState, types::AdminErrorResponse};

use super::{
    config::{
        PoolConfigUpdate, SupplierConfigUpdate, SupplierEntryUpdate, SupplierEntryView,
        normalize_supplier_id,
    },
    service::{KeySupplierService, ManualPurchaseResult, SupplierOverview, SupplierServiceError},
    store::{StoredSupplierEvent, SupplierEventStatus},
};

const MAX_WEBHOOK_BODY_BYTES: usize = 64 * 1024;
const DEFAULT_EVENT_LIMIT: usize = 50;
const MAX_EVENT_LIMIT: usize = 200;
const MAX_READ_IDS: usize = 200;
const MAX_WEBHOOK_CONCURRENCY: usize = 32;
const WEBHOOK_BODY_READ_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone)]
pub(crate) struct WebhookState {
    service: Option<Arc<KeySupplierService>>,
    permits: Arc<tokio::sync::Semaphore>,
}

pub fn webhook_router(service: Option<Arc<KeySupplierService>>) -> axum::Router {
    axum::Router::new()
        .route(
            "/key-supplier/webhook/{token}",
            axum::routing::post(ingest_webhook),
        )
        .with_state(WebhookState {
            service,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_WEBHOOK_CONCURRENCY)),
        })
}

pub async fn ingest_webhook(
    State(state): State<WebhookState>,
    Path(token): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(service) = state.service else {
        return unavailable();
    };
    if !service.has_valid_webhook_token(&token) {
        return error(
            StatusCode::NOT_FOUND,
            "not_found",
            "webhook endpoint not found",
        );
    }
    if !is_json_content_type(&headers) {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "Content-Type must be application/json",
        );
    }
    let _permit = match state.permits.try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => return unavailable(),
    };
    let body = match tokio::time::timeout(
        WEBHOOK_BODY_READ_TIMEOUT,
        to_bytes(body, MAX_WEBHOOK_BODY_BYTES),
    )
    .await
    {
        Ok(Ok(body)) => body,
        Ok(Err(_)) => {
            return error(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "request body exceeds 64 KiB",
            );
        }
        Err(_) => {
            return error(
                StatusCode::REQUEST_TIMEOUT,
                "request_timeout",
                "webhook request body timed out",
            );
        }
    };
    // 必须用原始请求体字节验签：解析后再序列化会改变字段顺序/空格，签名就对不上。
    let signature = headers
        .get("x-kiro-signature")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    match tokio::task::spawn_blocking(move || {
        service.ingest_signed(&token, body, signature.as_deref())
    })
    .await
    {
        Ok(Ok(result)) => {
            let status = if result.duplicate {
                StatusCode::OK
            } else {
                StatusCode::ACCEPTED
            };
            (
                status,
                Json(IngestResponse {
                    accepted: true,
                    duplicate: result.duplicate,
                    supplier_id: result.supplier_id,
                    event_id: result.event_id,
                    event_type: result.event_type,
                }),
            )
                .into_response()
        }
        Ok(Err(SupplierServiceError::Unauthorized)) => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "webhook endpoint not found",
        ),
        // 对方文档要求验签失败返回 401/403 且不执行任何业务操作。
        Ok(Err(SupplierServiceError::InvalidSignature)) => error(
            StatusCode::UNAUTHORIZED,
            "invalid_signature",
            "webhook signature verification failed",
        ),
        Ok(Err(SupplierServiceError::InvalidJson | SupplierServiceError::InvalidPayload)) => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid webhook payload",
        ),
        Ok(Err(SupplierServiceError::Store)) | Err(_) => unavailable(),
        Ok(Err(_)) => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "webhook request could not be accepted",
        ),
    }
}

pub async fn get_config(State(state): State<AdminState>) -> Response {
    match supplier(&state) {
        Ok(service) => Json(service.config_view()).into_response(),
        Err(response) => response,
    }
}

pub async fn put_config(
    State(state): State<AdminState>,
    update: Result<Json<SupplierConfigUpdate>, JsonRejection>,
) -> Response {
    let Json(update) = match update {
        Ok(update) => update,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.update_config(update) {
        Ok(view) => Json(view).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn overview(State(state): State<AdminState>) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.overview().await {
        Ok(overview) => Json(OverviewResponse::from(overview)).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn purchase(
    State(state): State<AdminState>,
    request: Result<Json<PurchaseRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.manual_purchase(request.count).await {
        Ok(result) => Json(PurchaseResponse::from(result)).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn register_webhook(State(state): State<AdminState>) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.register_webhook().await {
        Ok(callback_url) => {
            Json(serde_json::json!({ "callbackUrl": callback_url })).into_response()
        }
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn test_webhook(State(state): State<AdminState>) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.test_webhook().await {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

// ============ 全局号池 ============

pub async fn get_pool(State(state): State<AdminState>) -> Response {
    match supplier(&state) {
        Ok(service) => Json(service.pool_view()).into_response(),
        Err(response) => response,
    }
}

pub async fn put_pool(
    State(state): State<AdminState>,
    update: Result<Json<PoolConfigUpdate>, JsonRejection>,
) -> Response {
    let Json(update) = match update {
        Ok(update) => update,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.update_pool(update) {
        Ok(view) => Json(view).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

/// 号池当前状态。纯读，不发起采购。
pub async fn pool_status(State(state): State<AdminState>) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.pool_status() {
        Ok(status) => Json(status).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

// ============ 多供货商 ============

pub async fn list_suppliers(State(state): State<AdminState>) -> Response {
    match supplier(&state) {
        Ok(service) => Json(SupplierListResponse {
            items: service.supplier_views(),
        })
        .into_response(),
        Err(response) => response,
    }
}

pub async fn create_supplier(
    State(state): State<AdminState>,
    update: Result<Json<SupplierEntryUpdate>, JsonRejection>,
) -> Response {
    let Json(update) = match update {
        Ok(update) => update,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.upsert_supplier(None, update) {
        Ok(view) => (StatusCode::CREATED, Json(view)).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn update_supplier(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    update: Result<Json<SupplierEntryUpdate>, JsonRejection>,
) -> Response {
    let Json(update) = match update {
        Ok(update) => update,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.upsert_supplier(Some(id), update) {
        Ok(view) => Json(view).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn delete_supplier(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.delete_supplier(&id) {
        Ok(()) => Json(serde_json::json!({ "deleted": true })).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn supplier_overview(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.supplier_overview(&id).await {
        Ok(overview) => Json(OverviewResponse::from(overview)).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn supplier_purchase(
    State(state): State<AdminState>,
    Path(id): Path<String>,
    request: Result<Json<PurchaseRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.manual_purchase_from(&id, request.count).await {
        Ok(result) => Json(PurchaseResponse::from(result)).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn register_supplier_webhook(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.register_supplier_webhook(&id).await {
        Ok(callback_url) => {
            Json(serde_json::json!({ "callbackUrl": callback_url })).into_response()
        }
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn test_supplier_webhook(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.test_supplier_webhook(&id).await {
        Ok(()) => Json(serde_json::json!({ "success": true })).into_response(),
        Err(error_value) => service_error_response(error_value),
    }
}

/// 回调地址查询。`kiro-app` 这种不能远程注册的供货商，把它手填到对方面板。
pub async fn supplier_callback_url(
    State(state): State<AdminState>,
    Path(id): Path<String>,
) -> Response {
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    match service.supplier_callback_url(&id) {
        Ok(callback_url) => {
            Json(serde_json::json!({ "callbackUrl": callback_url })).into_response()
        }
        Err(error_value) => service_error_response(error_value),
    }
}

pub async fn list_events(
    State(state): State<AdminState>,
    query: Result<Query<EventQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => return query_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    let limit = query.limit.unwrap_or(DEFAULT_EVENT_LIMIT);
    if !(1..=MAX_EVENT_LIMIT).contains(&limit) || query.before.is_some_and(|id| id <= 0) {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid event pagination",
        );
    }
    let supplier_id = match normalize_supplier_filter(query.supplier_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let store = service.store();
    match tokio::task::spawn_blocking(move || {
        let page = store.list(limit, query.before, supplier_id.as_deref())?;
        let unread_count = store.unread_count(supplier_id.as_deref())?;
        Ok::<_, rusqlite::Error>((page, unread_count))
    })
    .await
    {
        Ok(Ok((page, unread_count))) => Json(EventPageResponse {
            items: page.items.into_iter().map(EventView::from).collect(),
            unread_count,
        })
        .into_response(),
        Ok(Err(_)) | Err(_) => unavailable(),
    }
}

pub async fn mark_events_read(
    State(state): State<AdminState>,
    request: Result<Json<MarkReadRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => return json_rejection(rejection),
    };
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    let supplier_id = match normalize_supplier_filter(request.supplier_id.as_deref()) {
        Ok(value) => value,
        Err(response) => return response,
    };
    let operation = if request.mark_all {
        if request.ids.as_ref().is_some_and(|ids| !ids.is_empty()) {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "markAll cannot be combined with ids",
            );
        }
        MarkReadOperation::All
    } else {
        let Some(ids) = request.ids.as_deref() else {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "ids or markAll=true is required",
            );
        };
        if ids.is_empty()
            || ids.len() > MAX_READ_IDS
            || ids.iter().any(|id| *id <= 0)
            || has_duplicates(ids)
        {
            return error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "ids must be unique positive event ids",
            );
        }
        MarkReadOperation::Ids(ids.to_vec())
    };
    let store = service.store();
    match tokio::task::spawn_blocking(move || match operation {
        MarkReadOperation::All => store.mark_all_read(supplier_id.as_deref()),
        MarkReadOperation::Ids(ids) => store.mark_read(&ids),
    })
    .await
    {
        Ok(Ok(changed)) => Json(serde_json::json!({ "updated": changed })).into_response(),
        Ok(Err(_)) | Err(_) => unavailable(),
    }
}

pub async fn retry_event(State(state): State<AdminState>, Path(id): Path<String>) -> Response {
    let Ok(id) = id.parse::<i64>() else {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "event id must be positive",
        );
    };
    if id <= 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "event id must be positive",
        );
    }
    let service = match supplier(&state) {
        Ok(service) => service,
        Err(response) => return response,
    };
    let store = service.store();
    match tokio::task::spawn_blocking(move || store.retry(id)).await {
        Ok(Ok(())) => Json(serde_json::json!({ "retried": true })).into_response(),
        Ok(Err(rusqlite::Error::QueryReturnedNoRows)) => error(
            StatusCode::NOT_FOUND,
            "not_found",
            "supplier event not found or not retryable",
        ),
        Ok(Err(_)) | Err(_) => unavailable(),
    }
}

fn supplier(state: &AdminState) -> Result<Arc<KeySupplierService>, Response> {
    state.key_supplier.clone().ok_or_else(unavailable)
}

fn unavailable() -> Response {
    error(
        StatusCode::SERVICE_UNAVAILABLE,
        "service_unavailable",
        "key supplier service is unavailable",
    )
}

/// 事件过滤用的供货商 id。空串/缺省表示不过滤。
fn normalize_supplier_filter(value: Option<&str>) -> Result<Option<String>, Response> {
    match value.map(str::trim).filter(|value| !value.is_empty()) {
        None => Ok(None),
        Some(value) => normalize_supplier_id(value).map(Some).map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "invalid supplier id",
            )
        }),
    }
}

fn service_error_response(error_value: SupplierServiceError) -> Response {
    match error_value {
        SupplierServiceError::Store
        | SupplierServiceError::ConfigPathUnavailable
        | SupplierServiceError::ConfigPersistence => unavailable(),
        // 诊断必须回到界面。只回一句 "request failed" 的话，运维分不出是 401、
        // 跨协议跳转把 Authorization 丢了、还是对方根本没这个接口——只能 SSH 进
        // 容器逐个 curl 去猜。诊断在 `SupplierError` 里已经脱敏（密钥与 ksk_ 令牌
        // 被替换）并限长，管理端接口本身也是鉴权后才能调。
        SupplierServiceError::SupplierApi { diagnostic } => error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            &format!("supplier API request failed: {diagnostic}"),
        ),
        SupplierServiceError::SupplierNotFound => {
            error(StatusCode::NOT_FOUND, "not_found", "supplier not found")
        }
        SupplierServiceError::SupplierIdConflict => error(
            StatusCode::CONFLICT,
            "conflict",
            "supplier id already exists",
        ),
        SupplierServiceError::TooManySuppliers => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "too many suppliers configured",
        ),
        SupplierServiceError::WebhookRegistrationUnsupported => error(
            StatusCode::BAD_REQUEST,
            "unsupported",
            "该供货商不支持远程注册 webhook，请复制回调地址到对方面板手动填写",
        ),
        SupplierServiceError::InvalidPurchaseQuantity
        | SupplierServiceError::SupplierConfiguration
        | SupplierServiceError::InvalidEvent => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid key supplier request",
        ),
        // 校验错误要把取值范围说清楚：这是运维在管理端唯一能看到的提示。
        SupplierServiceError::PoolConfig => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "号池配置无效：启用时目标存量必须在 1..=10000，额度水位必须在 0..=100000",
        ),
        SupplierServiceError::ImporterUnavailable | SupplierServiceError::ImportFailed => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "key supplier processing is unavailable",
        ),
        SupplierServiceError::Unauthorized
        | SupplierServiceError::InvalidSignature
        | SupplierServiceError::InvalidJson
        | SupplierServiceError::InvalidPayload => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid key supplier request",
        ),
    }
}

fn error(status: StatusCode, error_type: &str, message: &str) -> Response {
    (status, Json(AdminErrorResponse::new(error_type, message))).into_response()
}

fn json_rejection(rejection: JsonRejection) -> Response {
    let status = rejection.into_response().status();
    error(status, "invalid_request", "invalid JSON request body")
}

fn query_rejection(rejection: QueryRejection) -> Response {
    let status = rejection.into_response().status();
    error(status, "invalid_request", "invalid query parameters")
}

fn is_json_content_type(headers: &HeaderMap) -> bool {
    let Some(value) = headers.get(axum::http::header::CONTENT_TYPE) else {
        return false;
    };
    let Ok(value) = value.to_str() else {
        return false;
    };
    let mut parts = value.split(';');
    if !parts
        .next()
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"))
    {
        return false;
    }
    parts.all(|parameter| {
        let Some((name, value)) = parameter.trim().split_once('=') else {
            return false;
        };
        name.trim().eq_ignore_ascii_case("charset")
            && value.trim().trim_matches('"').eq_ignore_ascii_case("utf-8")
    })
}

fn has_duplicates(ids: &[i64]) -> bool {
    let mut ids = ids.to_vec();
    ids.sort_unstable();
    ids.windows(2).any(|pair| pair[0] == pair[1])
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IngestResponse {
    accepted: bool,
    duplicate: bool,
    supplier_id: String,
    event_id: String,
    event_type: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PurchaseRequest {
    count: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PurchaseResponse {
    supplier_id: String,
    order_id: String,
    requested: u32,
    purchased: u32,
    imported: u32,
    duplicate: u32,
    failed: u32,
}

impl From<ManualPurchaseResult> for PurchaseResponse {
    fn from(result: ManualPurchaseResult) -> Self {
        Self {
            supplier_id: result.supplier_id,
            order_id: result.order_id,
            requested: result.requested,
            purchased: result.purchased,
            imported: result.imported,
            duplicate: result.duplicate,
            failed: result.failed,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SupplierListResponse {
    items: Vec<SupplierEntryView>,
}

/// 概览响应。`kiro-rs` 独有的 profile/status 缺失时留 `null`，
/// `stockMax` 保留历史字段名以免旧前端炸。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OverviewResponse {
    supplier_id: String,
    kind: &'static str,
    stock_max: u64,
    key_price: Option<f64>,
    /// 阶梯定价的最高档单价。与 `keyPrice`（最低价）一起构成报价区间。
    key_price_max: Option<f64>,
    balance: Option<u64>,
    webhook_registered: bool,
    /// 本地号池里这家名下的凭据存活情况，补货闸按 `alive` 判定。
    credential_health: crate::kiro::token_manager::SupplierCredentialHealth,
    profile: ProfileView,
    status: StatusView,
}

impl From<SupplierOverview> for OverviewResponse {
    fn from(overview: SupplierOverview) -> Self {
        let snapshot = overview.snapshot;
        Self {
            supplier_id: overview.supplier_id,
            kind: overview.kind,
            stock_max: snapshot.stock_available.unwrap_or_default(),
            key_price: snapshot.key_price,
            key_price_max: snapshot.key_price_max,
            balance: snapshot.balance,
            webhook_registered: overview.webhook_registered,
            credential_health: overview.credential_health,
            profile: snapshot
                .profile
                .as_ref()
                .map(|profile| ProfileView {
                    name: profile.name.clone(),
                    quota: profile.quota,
                    remaining: profile.remaining,
                    used_quota: profile.used_quota,
                })
                .unwrap_or_else(|| ProfileView {
                    // kiro-app 没有 profile 概念，用余额填 remaining 以便前端统一展示。
                    name: overview.kind.to_string(),
                    quota: 0,
                    remaining: snapshot.balance.unwrap_or_default(),
                    used_quota: 0,
                }),
            status: snapshot
                .status
                .as_ref()
                .map(|status| StatusView {
                    keys_active: status.keys_active,
                    keys_dead: status.keys_dead,
                    keys_stock: status.keys_stock,
                    generating: status.generating,
                })
                .unwrap_or_else(|| StatusView {
                    keys_active: 0,
                    keys_dead: 0,
                    keys_stock: snapshot.stock_available.unwrap_or_default(),
                    generating: false,
                }),
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProfileView {
    name: String,
    quota: u64,
    remaining: u64,
    /// 改造前这里漏了 `rename_all`，实际发的是 `used_quota` 而前端类型写的是
    /// `usedQuota`（没人读所以没炸）。这里对齐成 camelCase。
    used_quota: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusView {
    keys_active: u64,
    keys_dead: u64,
    keys_stock: u64,
    generating: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventQuery {
    limit: Option<usize>,
    before: Option<i64>,
    /// 只看某家供货商的事件；缺省看全部。
    supplier_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarkReadRequest {
    ids: Option<Vec<i64>>,
    #[serde(default)]
    mark_all: bool,
    /// 配合 `markAll` 限定只标记某家供货商。
    supplier_id: Option<String>,
}

enum MarkReadOperation {
    All,
    Ids(Vec<i64>),
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventPageResponse {
    items: Vec<EventView>,
    unread_count: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct EventView {
    id: i64,
    supplier_id: String,
    event_id: String,
    event_type: String,
    purchase_order_id: Option<String>,
    /// 供货商侧开号批次号，用于和对方后台的批次对账。仅 `kiroapp-io` 有值。
    supplier_batch_id: Option<String>,
    message: Option<String>,
    quantity: i64,
    received_at: String,
    status: &'static str,
    attempts: i64,
    last_error: Option<String>,
    purchased_count: i64,
    imported_count: i64,
    duplicate_count: i64,
    webhook_duplicate_count: i64,
    failed_count: i64,
    read_at: Option<String>,
    /// 本单实际扣费（供货商积分）。阶梯定价下这是唯一权威数字。
    total_debit: Option<i64>,
    /// 本单均价 = `totalDebit / purchasedCount`。
    unit_price: Option<f64>,
    /// 供货商侧订单号，用于和对方订单历史对账。
    supplier_order_id: Option<String>,
    /// 命中对方幂等重放：上一次其实已成交。
    replayed: bool,
}

impl From<StoredSupplierEvent> for EventView {
    fn from(event: StoredSupplierEvent) -> Self {
        Self {
            id: event.id,
            supplier_id: event.supplier_id,
            event_id: event.event_id,
            event_type: event.event_type,
            purchase_order_id: event.purchase_order_id,
            supplier_batch_id: event.supplier_batch_id,
            message: event.message,
            quantity: event.quantity,
            received_at: event.received_at,
            status: status_name(event.status),
            attempts: event.attempts,
            last_error: event.last_error,
            purchased_count: event.purchased_count,
            imported_count: event.imported_count,
            duplicate_count: event.duplicate_count,
            webhook_duplicate_count: event.webhook_duplicate_count,
            failed_count: event.failed_count,
            read_at: event.read_at,
            total_debit: event.total_debit,
            unit_price: event.unit_price,
            supplier_order_id: event.supplier_order_id,
            replayed: event.replayed,
        }
    }
}

fn status_name(status: SupplierEventStatus) -> &'static str {
    status.as_str()
}
