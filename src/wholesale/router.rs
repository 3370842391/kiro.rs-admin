//! 批发系统 HTTP 路由（P5 客户接口 / P6 管理端接口）
//!
//! - 客户路由 `/wholesale/*`：register/login/sync/pool/balance/redeem/mothers/report-ban
//!   鉴权用 `wsk_` key（register/login 除外）。
//! - 管理路由挂在 admin 下：客户看板、余额调整、CDK 生成、母号看板、上号测活。
//!
//! 三套命名空间隔离：`wsk_`(批发) / `csk_`(网关) / `sk-admin`(管理)。

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex as AsyncMutex;

use super::service::WholesaleService;
use super::store::SharedWholesaleStore;

/// 批发路由共享状态
#[derive(Clone)]
pub struct WholesaleState {
    pub service: Arc<WholesaleService>,
    pub store: SharedWholesaleStore,
    /// 每客户一把异步锁：sync 串行化，防并发补成 2N
    pub sync_locks: Arc<Mutex<HashMap<i64, Arc<AsyncMutex<()>>>>>,
    /// 复用 admin key 做管理端鉴权
    pub admin_api_key: Arc<parking_lot::RwLock<String>>,
}

impl WholesaleState {
    pub fn new(
        service: Arc<WholesaleService>,
        store: SharedWholesaleStore,
        admin_api_key: Arc<parking_lot::RwLock<String>>,
    ) -> Self {
        Self {
            service,
            store,
            sync_locks: Arc::new(Mutex::new(HashMap::new())),
            admin_api_key,
        }
    }

    fn sync_lock_for(&self, customer_id: i64) -> Arc<AsyncMutex<()>> {
        let mut map = self.sync_locks.lock();
        map.entry(customer_id).or_insert_with(|| Arc::new(AsyncMutex::new(()))).clone()
    }
}

// ───────────────────────── 密码哈希（salted 迭代 SHA-256）─────────────────────────
// 无 argon2/bcrypt 依赖，用 100k 轮 SHA-256 + 随机盐。格式：`sha256$<salt_hex>$<hash_hex>`。
// MVP 够用；后续可平滑升级为 argon2（校验时按前缀分派）。

const PW_ROUNDS: u32 = 100_000;

pub fn hash_password(password: &str) -> String {
    use sha2::{Digest, Sha256};
    let salt: [u8; 16] = std::array::from_fn(|_| fastrand::u8(..));
    let mut buf = Vec::with_capacity(salt.len() + password.len());
    buf.extend_from_slice(&salt);
    buf.extend_from_slice(password.as_bytes());
    let mut digest = Sha256::digest(&buf).to_vec();
    for _ in 0..PW_ROUNDS {
        let mut h = Sha256::new();
        h.update(&digest);
        h.update(&salt);
        digest = h.finalize().to_vec();
    }
    format!("sha256${}${}", hex::encode(salt), hex::encode(digest))
}

pub fn verify_password(password: &str, stored: &str) -> bool {
    use sha2::{Digest, Sha256};
    let parts: Vec<&str> = stored.split('$').collect();
    if parts.len() != 3 || parts[0] != "sha256" {
        return false;
    }
    let salt = match hex::decode(parts[1]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut buf = Vec::with_capacity(salt.len() + password.len());
    buf.extend_from_slice(&salt);
    buf.extend_from_slice(password.as_bytes());
    let mut digest = Sha256::digest(&buf).to_vec();
    for _ in 0..PW_ROUNDS {
        let mut h = Sha256::new();
        h.update(&digest);
        h.update(&salt);
        digest = h.finalize().to_vec();
    }
    let want = hex::encode(digest);
    crate::common::auth::constant_time_eq(&want, parts[2])
}

/// 生成 wsk_ key / uid
fn gen_wsk_key() -> String {
    let raw: String = (0..32).map(|_| {
        let c = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"[fastrand::usize(..62)];
        c as char
    }).collect();
    format!("wsk_{raw}")
}
fn gen_uid() -> String {
    let raw: String = (0..8).map(|_| {
        let c = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[fastrand::usize(..32)];
        c as char
    }).collect();
    format!("u_{raw}")
}

// 从请求头取 wsk_ key
fn extract_wsk(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("authorization").and_then(|h| h.to_str().ok()) {
        if let Some(rest) = v.strip_prefix("Bearer ") {
            return Some(rest.trim().to_string());
        }
    }
    headers.get("x-api-key").and_then(|h| h.to_str().ok()).map(|s| s.to_string())
}

fn err(code: StatusCode, msg: &str) -> (StatusCode, Json<serde_json::Value>) {
    (code, Json(json!({ "error": msg })))
}

// ───────────────────────── 客户接口 ─────────────────────────

#[derive(Deserialize)]
struct RegisterReq { username: String, password: String, email: Option<String> }

async fn register(
    State(st): State<WholesaleState>,
    Json(req): Json<RegisterReq>,
) -> impl IntoResponse {
    let username = req.username.trim();
    if username.len() < 3 || req.password.len() < 6 {
        return err(StatusCode::BAD_REQUEST, "用户名至少3位、密码至少6位").into_response();
    }
    if st.store.customer_by_username(username).ok().flatten().is_some() {
        return err(StatusCode::CONFLICT, "用户名已存在").into_response();
    }
    let uid = gen_uid();
    let key = gen_wsk_key();
    let pw = hash_password(&req.password);
    match st.store.create_customer(&uid, &key, username, &pw, req.email.as_deref()) {
        Ok(c) => Json(json!({
            "uid": c.uid, "apiKey": key, "username": c.username,
            "target": c.target, "balanceCents": 0,
            "note": "apiKey 只显示一次，请妥善保存"
        })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("注册失败: {e}")).into_response(),
    }
}

#[derive(Deserialize)]
struct LoginReq { username: String, password: String }

async fn login(
    State(st): State<WholesaleState>,
    Json(req): Json<LoginReq>,
) -> impl IntoResponse {
    let c = match st.store.customer_by_username(req.username.trim()).ok().flatten() {
        Some(c) => c,
        None => return err(StatusCode::UNAUTHORIZED, "用户名或密码错误").into_response(),
    };
    if !verify_password(&req.password, &c.password_hash) {
        return err(StatusCode::UNAUTHORIZED, "用户名或密码错误").into_response();
    }
    if c.disabled {
        return err(StatusCode::FORBIDDEN, "账户已停用").into_response();
    }
    // MVP：登录直接返回 uid + apiKey（便于界面展示 / 程序调用）
    Json(json!({
        "uid": c.uid, "apiKey": c.api_key, "username": c.username,
        "target": c.target, "balanceCents": c.balance_cents
    })).into_response()
}

// 鉴权：从 wsk_ key 定位客户；uid 若提供必须匹配
fn auth_customer(
    st: &WholesaleState,
    headers: &HeaderMap,
    uid_hint: Option<&str>,
) -> Result<super::store::Customer, (StatusCode, Json<serde_json::Value>)> {
    let key = extract_wsk(headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "缺少 wsk_ key"))?;
    let c = st.store.customer_by_api_key(&key).ok().flatten()
        .ok_or_else(|| err(StatusCode::UNAUTHORIZED, "无效 key"))?;
    if c.disabled {
        return Err(err(StatusCode::FORBIDDEN, "账户已停用"));
    }
    if let Some(uid) = uid_hint {
        if !uid.is_empty() && uid != c.uid {
            return Err(err(StatusCode::FORBIDDEN, "uid 与 key 不匹配"));
        }
    }
    Ok(c)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SyncReq {
    uid: Option<String>,
    target: Option<i64>,
    region: Option<String>,
    #[serde(default)]
    exclude_mothers: Vec<String>,
}

async fn sync(
    State(st): State<WholesaleState>,
    headers: HeaderMap,
    Json(req): Json<SyncReq>,
) -> impl IntoResponse {
    let c = match auth_customer(&st, &headers, req.uid.as_deref()) {
        Ok(c) => c,
        Err(e) => return e.into_response(),
    };
    // 客户级串行锁：防并发把号池补成 2N
    let lock = st.sync_lock_for(c.id);
    let _guard = lock.lock().await;

    // 归一化排除母号（接受 directory id 或 start url）
    let excludes = normalize_mothers(&req.exclude_mothers);
    let label = format!("wsk{}", c.id);
    match st.service.sync_pool(c.id, req.target, req.region.as_deref(), &excludes, &label).await {
        Ok(res) => Json(res).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &format!("同步失败: {e}")).into_response(),
    }
}

async fn pool(State(st): State<WholesaleState>, headers: HeaderMap) -> impl IntoResponse {
    let c = match auth_customer(&st, &headers, None) {
        Ok(c) => c, Err(e) => return e.into_response(),
    };
    match st.service.current_pool(c.id, true) {
        Ok(items) => Json(json!({
            "uid": c.uid, "target": c.target,
            "alive": items.iter().filter(|i| i.status == "active").count(),
            "balanceCents": c.balance_cents,
            "pool": items
        })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e).into_response(),
    }
}

async fn balance(State(st): State<WholesaleState>, headers: HeaderMap) -> impl IntoResponse {
    let c = match auth_customer(&st, &headers, None) {
        Ok(c) => c, Err(e) => return e.into_response(),
    };
    Json(json!({ "balanceCents": c.balance_cents, "uid": c.uid })).into_response()
}

#[derive(Deserialize)]
struct RedeemReq { code: String }

async fn redeem(
    State(st): State<WholesaleState>,
    headers: HeaderMap,
    Json(req): Json<RedeemReq>,
) -> impl IntoResponse {
    let c = match auth_customer(&st, &headers, None) {
        Ok(c) => c, Err(e) => return e.into_response(),
    };
    match st.store.redeem_cdk(req.code.trim(), c.id) {
        Ok(bal) => Json(json!({ "ok": true, "balanceCents": bal })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e).into_response(),
    }
}

async fn mothers(State(st): State<WholesaleState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth_customer(&st, &headers, None) {
        return e.into_response();
    }
    match st.store.available_by_mother() {
        Ok(rows) => {
            let list: Vec<_> = rows.into_iter().map(|(id, state, region, avail)| json!({
                "motherId": id, "state": state, "region": region, "available": avail
            })).collect();
            Json(json!({ "mothers": list })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
struct ReportBanReq { public_id: String }

async fn report_ban(
    State(st): State<WholesaleState>,
    headers: HeaderMap,
    Json(req): Json<ReportBanReq>,
) -> impl IntoResponse {
    let c = match auth_customer(&st, &headers, None) {
        Ok(c) => c, Err(e) => return e.into_response(),
    };
    // 找到该客户名下这个 public_id 的活 holding
    let holdings = match st.store.holdings_for_customer(c.id, true) {
        Ok(h) => h, Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    };
    let target = holdings.into_iter().find(|h| h.public_id == req.public_id.trim() && h.status != "dead");
    let target = match target {
        Some(t) => t,
        None => return err(StatusCode::NOT_FOUND, "未找到该号或已判死").into_response(),
    };
    // 服务端核实：拿号自己凭据探一次，真死才认
    let health = st.service.probe_credential(target.credential_id as u64).await;
    if !health.is_dead() {
        return Json(json!({ "ok": false, "verified": false, "note": "核实号仍存活，不予质保" })).into_response();
    }
    match st.store.mark_holding_dead(target.id) {
        Ok(refunded) => {
            let _ = st.store.record_child_death(&target.mother_id);
            Json(json!({ "ok": true, "verified": true, "refunded": refunded })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e).into_response(),
    }
}

/// 归一化母号标识：从 start url 抽 directory id，否则原样。
fn normalize_mothers(input: &[String]) -> Vec<String> {
    let re = regex::Regex::new(r"(d-[0-9a-f]{10})").unwrap();
    input.iter().filter_map(|s| {
        let s = s.trim();
        if s.is_empty() { return None; }
        if let Some(cap) = re.captures(s) {
            Some(cap[1].to_string())
        } else {
            Some(s.to_string())
        }
    }).collect()
}

// ───────────────────────── 管理端接口 ─────────────────────────

fn auth_admin(st: &WholesaleState, headers: &HeaderMap) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let key = extract_wsk(headers).ok_or_else(|| err(StatusCode::UNAUTHORIZED, "缺少 admin key"))?;
    let want = st.admin_api_key.read().clone();
    if crate::common::auth::constant_time_eq(&key, &want) {
        Ok(())
    } else {
        Err(err(StatusCode::UNAUTHORIZED, "admin key 无效"))
    }
}

async fn admin_list_customers(State(st): State<WholesaleState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.list_customers() {
        Ok(list) => {
            let rows: Vec<_> = list.into_iter().map(|c| {
                let alive = st.store.active_count(c.id).unwrap_or(0);
                json!({
                    "id": c.id, "uid": c.uid, "username": c.username, "email": c.email,
                    "balanceCents": c.balance_cents, "target": c.target, "disabled": c.disabled,
                    "aliveCount": alive, "createdAt": c.created_at
                })
            }).collect();
            Json(json!({ "customers": rows })).into_response()
        }
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdjustBalanceReq { customer_id: i64, delta_cents: i64, reason: String }

async fn admin_adjust_balance(
    State(st): State<WholesaleState>,
    headers: HeaderMap,
    Json(req): Json<AdjustBalanceReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.apply_wallet_delta(req.customer_id, req.delta_cents, "admin_adjust", Some(&req.reason), Some("admin")) {
        Ok(bal) => Json(json!({ "ok": true, "balanceCents": bal })).into_response(),
        Err(e) => err(StatusCode::BAD_REQUEST, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetTargetReq { customer_id: i64, target: i64 }
async fn admin_set_target(
    State(st): State<WholesaleState>, headers: HeaderMap, Json(req): Json<SetTargetReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.set_customer_target(req.customer_id, req.target) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetDisabledReq { customer_id: i64, disabled: bool }
async fn admin_set_disabled(
    State(st): State<WholesaleState>, headers: HeaderMap, Json(req): Json<SetDisabledReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.set_customer_disabled(req.customer_id, req.disabled) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenCdkReq { value_cents: i64, count: i64, batch: Option<String> }

async fn admin_gen_cdks(
    State(st): State<WholesaleState>, headers: HeaderMap, Json(req): Json<GenCdkReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    if req.value_cents <= 0 || req.count <= 0 || req.count > 10000 {
        return err(StatusCode::BAD_REQUEST, "面额需>0，数量1~10000").into_response();
    }
    let codes: Vec<String> = (0..req.count).map(|_| gen_cdk_code()).collect();
    match st.store.create_cdks(&codes, req.value_cents, Some("admin"), req.batch.as_deref()) {
        Ok(n) => Json(json!({ "ok": true, "generated": n, "codes": codes })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn admin_list_cdks(
    State(st): State<WholesaleState>, headers: HeaderMap, Query(q): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    let only_unused = q.get("unused").map(|v| v == "1" || v == "true").unwrap_or(false);
    let limit: i64 = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(200);
    match st.store.list_cdks(only_unused, limit) {
        Ok(list) => Json(json!({ "cdks": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

async fn admin_mothers(State(st): State<WholesaleState>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.list_mothers() {
        Ok(list) => Json(json!({ "mothers": list })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetMotherStateReq { directory_id: String, state: String }
async fn admin_set_mother_state(
    State(st): State<WholesaleState>, headers: HeaderMap, Json(req): Json<SetMotherStateReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    match st.store.set_mother_state(&req.directory_id, &req.state) {
        Ok(_) => Json(json!({ "ok": true })).into_response(),
        Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()).into_response(),
    }
}

/// 上号测活：把一批 credential_id 入销售池前先探活，活的才 add_sale_account。
/// 前提：这些号已经通过 admin 的凭据导入进了 token_manager（有 credential_id）。
#[derive(Deserialize)]
struct UploadTestReq {
    /// [{credentialId, motherId, startUrl, region, publicId}]
    accounts: Vec<UploadAccount>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UploadAccount {
    credential_id: i64,
    mother_id: Option<String>,
    start_url: Option<String>,
    region: Option<String>,
    public_id: Option<String>,
}

async fn admin_upload_test(
    State(st): State<WholesaleState>, headers: HeaderMap, Json(req): Json<UploadTestReq>,
) -> impl IntoResponse {
    if let Err(e) = auth_admin(&st, &headers) { return e.into_response(); }
    let mut results = Vec::new();
    for a in req.accounts {
        // 母号：优先显式 mother_id，否则从 start_url 抽
        let mother_id = a.mother_id.clone().or_else(|| {
            a.start_url.as_deref().and_then(|u| {
                regex::Regex::new(r"(d-[0-9a-f]{10})").ok()
                    .and_then(|re| re.captures(u).map(|c| c[1].to_string()))
            })
        });
        let mother_id = match mother_id {
            Some(m) => m,
            None => { results.push(json!({ "credentialId": a.credential_id, "ok": false, "note": "缺母号(mother_id/start_url)" })); continue; }
        };
        // 建母号档
        let start_url = a.start_url.clone().unwrap_or_else(|| format!("https://{mother_id}.awsapps.com/start"));
        let _ = st.store.upsert_mother(&mother_id, &start_url, a.region.as_deref());
        // 探活
        let health = st.service.probe_credential(a.credential_id as u64).await;
        if !health.is_alive() {
            results.push(json!({ "credentialId": a.credential_id, "ok": false, "health": health.as_status_str() }));
            continue;
        }
        let public_id = a.public_id.clone().unwrap_or_else(|| format!("{mother_id}-{}", a.credential_id));
        match st.store.add_sale_account(a.credential_id, &mother_id, a.region.as_deref(), &public_id) {
            Ok(_) => results.push(json!({ "credentialId": a.credential_id, "ok": true, "publicId": public_id, "motherId": mother_id })),
            Err(e) => results.push(json!({ "credentialId": a.credential_id, "ok": false, "note": e.to_string() })),
        }
    }
    Json(json!({ "results": results })).into_response()
}

fn gen_cdk_code() -> String {
    let seg = || -> String {
        (0..4).map(|_| b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789"[fastrand::usize(..32)] as char).collect()
    };
    format!("CDK-{}-{}-{}", seg(), seg(), seg())
}

// ───────────────────────── 路由组装 ─────────────────────────

// ───────────────────────── 静态界面 ─────────────────────────

async fn page_dashboard() -> impl IntoResponse {
    axum::response::Html(include_str!("assets/dashboard.html"))
}
async fn asset_dashboard_js() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
     include_str!("assets/dashboard.js"))
}
async fn page_admin() -> impl IntoResponse {
    axum::response::Html(include_str!("assets/admin.html"))
}
async fn asset_admin_js() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript; charset=utf-8")],
     include_str!("assets/admin.js"))
}

/// 客户 + 管理端合并路由，挂在 `/wholesale` 下。
pub fn create_wholesale_router(state: WholesaleState) -> Router {
    Router::new()
        // 界面
        .route("/", get(page_dashboard))
        .route("/dashboard.js", get(asset_dashboard_js))
        .route("/admin-ui", get(page_admin))
        .route("/admin.js", get(asset_admin_js))
        // 客户
        .route("/register", post(register))
        .route("/login", post(login))
        .route("/sync", post(sync))
        .route("/pool", get(pool))
        .route("/balance", get(balance))
        .route("/redeem", post(redeem))
        .route("/mothers", get(mothers))
        .route("/report-ban", post(report_ban))
        // 管理端（复用 admin key 鉴权）
        .route("/admin/customers", get(admin_list_customers))
        .route("/admin/balance", post(admin_adjust_balance))
        .route("/admin/target", post(admin_set_target))
        .route("/admin/disabled", post(admin_set_disabled))
        .route("/admin/cdks", get(admin_list_cdks).post(admin_gen_cdks))
        .route("/admin/mothers", get(admin_mothers))
        .route("/admin/mother-state", post(admin_set_mother_state))
        .route("/admin/upload-test", post(admin_upload_test))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_hash_roundtrip() {
        let h = hash_password("s3cret!");
        assert!(verify_password("s3cret!", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("s3cret!", "garbage"));
    }

    #[test]
    fn key_and_uid_format() {
        assert!(gen_wsk_key().starts_with("wsk_"));
        assert!(gen_uid().starts_with("u_"));
        assert!(gen_cdk_code().starts_with("CDK-"));
    }

    #[test]
    fn normalize_mothers_extracts_dir_id() {
        let out = normalize_mothers(&[
            "https://d-9066765b2d.awsapps.com/start".to_string(),
            "d-1234567890".to_string(),
            "  ".to_string(),
        ]);
        assert_eq!(out, vec!["d-9066765b2d".to_string(), "d-1234567890".to_string()]);
    }
}
