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
    config::SupplierConfigUpdate,
    service::{KeySupplierService, SupplierServiceError},
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
    match tokio::task::spawn_blocking(move || service.ingest(&token, body)).await {
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
        Ok(overview) => Json(OverviewResponse {
            profile: ProfileView {
                name: overview.profile.name,
                quota: overview.profile.quota,
                remaining: overview.profile.remaining,
                used_quota: overview.profile.used_quota,
            },
            stock_max: overview.stock.max,
            status: StatusView {
                keys_active: overview.status.keys_active,
                keys_dead: overview.status.keys_dead,
                keys_stock: overview.status.keys_stock,
                generating: overview.status.generating,
            },
        })
        .into_response(),
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
        Ok(result) => Json(PurchaseResponse {
            order_id: result.order_id,
            requested: result.requested,
            purchased: result.purchased,
            imported: result.imported,
            duplicate: result.duplicate,
            failed: result.failed,
        })
        .into_response(),
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
    let store = service.store();
    match tokio::task::spawn_blocking(move || {
        let page = store.list(limit, query.before)?;
        let unread_count = store.unread_count()?;
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
        MarkReadOperation::All => store.mark_all_read(),
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

fn service_error_response(error_value: SupplierServiceError) -> Response {
    match error_value {
        SupplierServiceError::Store
        | SupplierServiceError::ConfigPathUnavailable
        | SupplierServiceError::ConfigPersistence => unavailable(),
        SupplierServiceError::SupplierApi { .. } => error(
            StatusCode::BAD_GATEWAY,
            "api_error",
            "supplier API request failed",
        ),
        SupplierServiceError::InvalidPurchaseQuantity
        | SupplierServiceError::SupplierConfiguration
        | SupplierServiceError::InvalidEvent => error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "invalid key supplier request",
        ),
        SupplierServiceError::ImporterUnavailable | SupplierServiceError::ImportFailed => error(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_unavailable",
            "key supplier processing is unavailable",
        ),
        SupplierServiceError::Unauthorized
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
    order_id: String,
    requested: u32,
    purchased: u32,
    imported: u32,
    duplicate: u32,
    failed: u32,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OverviewResponse {
    profile: ProfileView,
    stock_max: u64,
    status: StatusView,
}

#[derive(Serialize)]
struct ProfileView {
    name: String,
    quota: u64,
    remaining: u64,
    used_quota: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusView {
    keys_active: u64,
    keys_dead: u64,
    keys_stock: u64,
    generating: u64,
}

#[derive(Deserialize)]
pub(crate) struct EventQuery {
    limit: Option<usize>,
    before: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MarkReadRequest {
    ids: Option<Vec<i64>>,
    #[serde(default)]
    mark_all: bool,
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
    event_id: String,
    event_type: String,
    purchase_order_id: Option<String>,
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
}

impl From<StoredSupplierEvent> for EventView {
    fn from(event: StoredSupplierEvent) -> Self {
        Self {
            id: event.id,
            event_id: event.event_id,
            event_type: event.event_type,
            purchase_order_id: event.purchase_order_id,
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
        }
    }
}

fn status_name(status: SupplierEventStatus) -> &'static str {
    status.as_str()
}
