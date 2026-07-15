# 动态模型目录实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 将公共 `GET /v1/models` 改为按客户端 Key 分组实时汇总 Kiro `ListAvailableModels`，动态暴露 GPT 等真实模型，同时继续提供 Claude 点号、连字符及 thinking 兼容入口。

**Architecture:** 上游层为单凭据模型查询补齐 `nextToken` 分页；Kiro 层新增独立 `DynamicModelCatalog`，负责严格分组候选、有界并发、五分钟新鲜缓存、三十分钟陈旧回退和单飞刷新，并复用查询结果更新现有模型能力缓存；Anthropic 层只负责把真实上游模型转换为稳定的公共 `Model` 列表。生产路径不再调用静态 `available_models()`，也不会从模型映射配置中虚构目录项。

**Tech Stack:** Rust 2024、Axum、Tokio、Reqwest、Futures、Serde、parking_lot、thiserror、Cargo 内置测试。

---

## 文件结构

- `src/kiro/model/available_models.rs`：接收上游 `nextToken`，继续承载单页响应结构。
- `src/kiro/token_manager.rs`：构造分页 URL、完整拉取单凭据目录，并提供严格分组的未禁用凭据 ID 快照。
- `src/kiro/model_catalog.rs`：新增动态目录核心；只处理查询编排、合并、缓存、并发和模型能力缓存。
- `src/kiro/provider.rs`：把 `DynamicModelCatalog` 接到现有 `MultiTokenManager`，向协议层暴露按分组目录方法。
- `src/kiro/mod.rs`：注册新的 Kiro 模型目录模块。
- `src/anthropic/model_catalog.rs`：新增公共模型展示转换器；只处理 owner、显示名、Claude 别名与稳定排序。
- `src/anthropic/mod.rs`：注册 Anthropic 展示转换模块。
- `src/anthropic/handlers.rs`：将 `/v1/models` 改为读取 `State<AppState>`、`Extension<KeyContext>` 的动态异步 handler，删除静态生产目录。
- `src/anthropic/router.rs`：增加经过真实认证中间件的 `/v1/models` 路由回归测试。
- `src/admin/model_profile_sync.rs`：给测试构造的 `ListAvailableModelsResponse` 补上新增字段。
- `CHANGELOG.md`：记录动态目录及客户可感知变化。

## Task 1：补齐 `ListAvailableModels` 完整分页

**Files:**
- Modify: `src/kiro/model/available_models.rs:10-23`
- Modify: `src/kiro/token_manager.rs:512-516, 614-708, 6470-6478`
- Modify: `src/admin/model_profile_sync.rs:543-555`
- Test: `src/kiro/model/available_models.rs:54-112`
- Test: `src/kiro/token_manager.rs:6470-6478`

- [ ] **Step 1: 先写 `nextToken` 反序列化失败测试**

在 `src/kiro/model/available_models.rs` 的测试模块加入：

```rust
#[test]
fn deserialize_next_token() {
    let response: ListAvailableModelsResponse = serde_json::from_str(
        r#"{"models":[],"nextToken":"page / two"}"#,
    )
    .unwrap();

    assert_eq!(response.next_token.as_deref(), Some("page / two"));
}
```

- [ ] **Step 2: 运行测试并确认因字段不存在失败**

Run: `cargo test -j 1 kiro::model::available_models::tests::deserialize_next_token -- --exact`

Expected: 编译失败，错误包含 `no field next_token on type ListAvailableModelsResponse`。

- [ ] **Step 3: 给单页响应增加可选 `nextToken`**

在 `ListAvailableModelsResponse.models` 后加入：

```rust
/// 下一页游标；缺失或空字符串表示已经到最后一页。
#[serde(default, skip_serializing_if = "Option::is_none")]
pub next_token: Option<String>,
```

同时在 `src/admin/model_profile_sync.rs` 的结构体字面量中加入：

```rust
next_token: None,
```

- [ ] **Step 4: 运行响应类型测试并确认通过**

Run: `cargo test -j 1 kiro::model::available_models::tests -- --nocapture`

Expected: `available_models` 模块全部测试通过。

- [ ] **Step 5: 先写 URL 编码、重复游标和页数上限测试**

把原 `list_available_models_url_requests_fifty_models_without_fake_profile` 更新为三参数 URL builder，并在 `src/kiro/token_manager.rs` 测试模块加入：

```rust
#[test]
fn list_available_models_url_encodes_profile_and_next_token() {
    let url = build_list_available_models_url(
        "q.eu-central-1.amazonaws.com",
        Some("arn:aws:codewhisperer:eu-central-1:1:profile/a b"),
        Some("page / two"),
    )
    .unwrap();
    let query: HashMap<String, String> = url.query_pairs().into_owned().collect();

    assert_eq!(query.get("origin").map(String::as_str), Some("AI_EDITOR"));
    assert_eq!(query.get("maxResults").map(String::as_str), Some("50"));
    assert_eq!(
        query.get("profileArn").map(String::as_str),
        Some("arn:aws:codewhisperer:eu-central-1:1:profile/a b")
    );
    assert_eq!(query.get("nextToken").map(String::as_str), Some("page / two"));
}

#[tokio::test]
async fn pagination_rejects_repeated_next_token_without_returning_partial_models() {
    let calls = AtomicUsize::new(0);
    let result = collect_available_model_pages(|_| {
        calls.fetch_add(1, Ordering::SeqCst);
        async {
            Ok(ListAvailableModelsResponse {
                models: vec![upstream_model("claude-opus-4.8")],
                next_token: Some("same-token".to_string()),
                resolved_api_region: None,
                resolved_host: None,
                kiro_version: None,
            })
        }
    })
    .await;

    assert!(result.unwrap_err().to_string().contains("重复 nextToken"));
    assert_eq!(calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn pagination_rejects_an_eleventh_page_without_returning_partial_models() {
    let calls = AtomicUsize::new(0);
    let result = collect_available_model_pages(|_| {
        let page = calls.fetch_add(1, Ordering::SeqCst) + 1;
        async move {
            Ok(ListAvailableModelsResponse {
                models: vec![upstream_model(&format!("model-{page}"))],
                next_token: Some(format!("page-{page}")),
                resolved_api_region: None,
                resolved_host: None,
                kiro_version: None,
            })
        }
    })
    .await;

    assert!(result.unwrap_err().to_string().contains("超过 10 页"));
    assert_eq!(calls.load(Ordering::SeqCst), 10);
}
```

测试模块内增加精确构造 helper：

```rust
fn upstream_model(id: &str) -> crate::kiro::model::available_models::UpstreamModel {
    crate::kiro::model::available_models::UpstreamModel {
        model_id: id.to_string(),
        model_name: None,
        description: None,
        token_limits: None,
    }
}
```

- [ ] **Step 6: 运行分页测试并确认新接口尚未实现**

Run: `cargo test -j 1 pagination_ -- --nocapture`

Expected: 编译失败，包含 `collect_available_model_pages` 未定义或 URL builder 参数数量不匹配。

- [ ] **Step 7: 实现编码 URL、单页错误和最多十页的完整收集器**

将 URL builder 改为：

```rust
fn build_list_available_models_url(
    host: &str,
    profile_arn: Option<&str>,
    next_token: Option<&str>,
) -> anyhow::Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("https://{host}/ListAvailableModels"))?;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("origin", "AI_EDITOR")
            .append_pair("maxResults", "50");
        if let Some(profile_arn) = profile_arn {
            query.append_pair("profileArn", profile_arn);
        }
        if let Some(next_token) = next_token {
            query.append_pair("nextToken", next_token);
        }
    }
    Ok(url)
}
```

在 `get_available_models` 前增加：

```rust
const MAX_AVAILABLE_MODEL_PAGES: usize = 10;

#[derive(Debug, thiserror::Error)]
enum ListAvailableModelsPageError {
    #[error("ListAvailableModels HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("ListAvailableModels 协议错误: {0}")]
    Protocol(String),
    #[error(transparent)]
    Request(#[from] reqwest::Error),
}

impl ListAvailableModelsPageError {
    fn can_try_alternate_region(&self) -> bool {
        matches!(self, Self::Http { status: 400 | 403, .. })
    }
}

async fn collect_available_model_pages<F, Fut>(
    mut fetch_page: F,
) -> Result<Vec<crate::kiro::model::available_models::UpstreamModel>, ListAvailableModelsPageError>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: std::future::Future<
        Output = Result<ListAvailableModelsResponse, ListAvailableModelsPageError>,
    >,
{
    let mut models = Vec::new();
    let mut next_token: Option<String> = None;
    let mut seen_tokens = HashSet::new();

    for _ in 0..MAX_AVAILABLE_MODEL_PAGES {
        let page = fetch_page(next_token.clone()).await?;
        models.extend(page.models);
        let Some(token) = page.next_token.filter(|value| !value.trim().is_empty()) else {
            return Ok(models);
        };
        if !seen_tokens.insert(token.clone()) {
            return Err(ListAvailableModelsPageError::Protocol(format!(
                "上游返回重复 nextToken: {token}"
            )));
        }
        next_token = Some(token);
    }

    Err(ListAvailableModelsPageError::Protocol(
        "模型目录超过 10 页（500 个模型）".to_string(),
    ))
}
```

把 `get_available_models` 中每个 region 的单次请求改为完整收集：

```rust
let page_result = collect_available_model_pages(|next_token| {
    let url = build_list_available_models_url(
        &host,
        rest_profile_arn(credentials),
        next_token.as_deref(),
    );
    let client = &client;
    let amz_user_agent = &amz_user_agent;
    let user_agent = &user_agent;
    let host = &host;
    async move {
        let url = url.map_err(|error| {
            ListAvailableModelsPageError::Protocol(error.to_string())
        })?;
        let mut request = client
            .get(url)
            .header("x-amz-user-agent", amz_user_agent)
            .header("user-agent", user_agent)
            .header("host", host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("Authorization", format!("Bearer {token}"))
            .header("Connection", "close");
        if let Some(token_type) = credentials.token_type_header() {
            request = request.header("tokentype", token_type);
        }
        let response = request.send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response.json::<ListAvailableModelsResponse>().await?);
        }
        Err(ListAvailableModelsPageError::Http {
            status: status.as_u16(),
            body: response.text().await.unwrap_or_default(),
        })
    }
})
.await;

match page_result {
    Ok(models) => {
        return Ok(ListAvailableModelsResponse {
            models,
            next_token: None,
            resolved_api_region: Some(region.to_string()),
            resolved_host: Some(host),
            kiro_version: Some(kiro_version.clone()),
        });
    }
    Err(error) if error.can_try_alternate_region() && idx + 1 < candidates.len() => {
        tracing::debug!(
            region,
            error = %error,
            alternate_region = %candidates[idx + 1],
            "ListAvailableModels 完整分页失败，尝试备用端点"
        );
        last_error = Some(error.to_string());
    }
    Err(error) => bail!(error),
}
```

该 `match` 替换旧的单页 success/error 分支，确保任何后续页失败都不返回前面已经收到的半份目录。

- [ ] **Step 8: 运行分页与管理端相关测试**

Run: `cargo test -j 1 pagination_ list_available_models -- --nocapture`

Expected: 新增分页测试、URL 测试及现有单凭据模型测试通过；若 Cargo 不接受两个 filter，则分别运行 `cargo test -j 1 pagination_` 和 `cargo test -j 1 list_available_models`。

- [ ] **Step 9: 格式化并提交分页实现**

Run: `cargo fmt --all`

Run: `git add -- src/kiro/model/available_models.rs src/kiro/token_manager.rs src/admin/model_profile_sync.rs && git commit -m "feat(models): 补齐上游模型目录分页"`

Expected: 本地生成一个只包含分页与响应类型的提交。

## Task 2：实现严格分组、并发、缓存和部分失败目录

**Files:**
- Create: `src/kiro/model_catalog.rs`
- Modify: `src/kiro/mod.rs:1-12`
- Modify: `src/kiro/token_manager.rs:1475-1481, 7054-7180`
- Modify: `src/kiro/provider.rs:30-33, 202-228, 275-286, 298-321`
- Test: `src/kiro/model_catalog.rs`
- Test: `src/kiro/token_manager.rs`

- [ ] **Step 1: 先写未禁用凭据严格分组测试**

在 `src/kiro/token_manager.rs` 的账号分组测试区加入：

```rust
#[test]
fn enabled_credential_ids_in_group_ignore_runtime_throttle_but_not_disabled() {
    let manager = MultiTokenManager::new(
        Config::default(),
        vec![
            grouped_cred("a", &["g1"]),
            grouped_cred("b", &["g1", "g2"]),
            grouped_cred("c", &[]),
        ],
        None,
        None,
        false,
    )
    .unwrap();
    manager.report_rate_limited(1, StdDuration::from_secs(60));
    manager.set_disabled(2, true).unwrap();

    assert_eq!(manager.enabled_credential_ids_in_group(Some("g1")), vec![1]);
    assert_eq!(manager.enabled_credential_ids_in_group(Some("g2")), Vec::<u64>::new());
    assert_eq!(manager.enabled_credential_ids_in_group(None), vec![1, 3]);
}
```

- [ ] **Step 2: 运行测试并确认 helper 尚不存在**

Run: `cargo test -j 1 enabled_credential_ids_in_group_ignore_runtime_throttle_but_not_disabled -- --exact`

Expected: 编译失败，包含 `no method named enabled_credential_ids_in_group`。

- [ ] **Step 3: 实现只过滤禁用态与分组的候选快照**

在 `MultiTokenManager::total_count_in_group` 前加入：

```rust
/// 返回模型目录查询可使用的未禁用凭据 ID。
///
/// 目录代表账号权限，不受临时 429、RPM 窗口或当前在途数影响。
pub fn enabled_credential_ids_in_group(&self, group: Option<&str>) -> Vec<u64> {
    self.entries
        .lock()
        .iter()
        .filter(|entry| {
            !entry.disabled && group_matches(&entry.credentials.groups, group)
        })
        .map(|entry| entry.id)
        .collect()
}
```

- [ ] **Step 4: 运行严格分组测试并确认通过**

Run: `cargo test -j 1 enabled_credential_ids_in_group -- --nocapture`

Expected: 测试通过，临时冷却的凭据仍在目录候选中，禁用和跨组凭据被排除。

- [ ] **Step 5: 先创建动态目录行为测试**

创建 `src/kiro/model_catalog.rs`，先写类型骨架与 `#[cfg(test)]` 测试。测试使用以下 helper：

```rust
fn model(id: &str, name: Option<&str>, max_input_tokens: Option<i64>) -> UpstreamModel {
    UpstreamModel {
        model_id: id.to_string(),
        model_name: name.map(str::to_string),
        description: None,
        token_limits: max_input_tokens.map(|value| TokenLimits {
            max_input_tokens: Some(value),
        }),
    }
}

fn response(models: Vec<UpstreamModel>) -> ListAvailableModelsResponse {
    ListAvailableModelsResponse {
        models,
        next_token: None,
        resolved_api_region: None,
        resolved_host: None,
        kiro_version: None,
    }
}
```

加入以下测试，时间全部由 `models_for_at` 参数注入，避免真实 sleep：

```rust
#[tokio::test]
async fn merges_successes_deduplicates_and_keeps_max_positive_input_limit() {
    let catalog = DynamicModelCatalog::default();
    let now = Instant::now();
    let models = catalog
        .models_for_at(None, vec![1, 2], now, |id| async move {
            Ok(match id {
                1 => response(vec![model("gpt-5.6-sol", None, Some(200_000))]),
                2 => response(vec![model("gpt-5.6-sol", Some("GPT Sol"), Some(1_000_000))]),
                _ => unreachable!(),
            })
        })
        .await
        .unwrap();

    assert_eq!(models.len(), 1);
    assert_eq!(models[0].model_id, "gpt-5.6-sol");
    assert_eq!(models[0].model_name.as_deref(), Some("GPT Sol"));
    assert_eq!(
        models[0].token_limits.as_ref().and_then(|v| v.max_input_tokens),
        Some(1_000_000)
    );
}

#[tokio::test]
async fn partial_failure_returns_success_union_and_updates_availability() {
    let catalog = DynamicModelCatalog::default();
    let now = Instant::now();
    let models = catalog
        .models_for_at(Some("g1"), vec![1, 2], now, |id| async move {
            if id == 1 {
                Ok(response(vec![model("claude-opus-4.8", None, None)]))
            } else {
                anyhow::bail!("simulated failure")
            }
        })
        .await
        .unwrap();

    assert_eq!(models[0].model_id, "claude-opus-4.8");
    assert_eq!(
        catalog.availability(1, "claude-opus-4.8", now),
        ModelAvailability::Available
    );
}

#[tokio::test]
async fn fresh_cache_prevents_duplicate_queries() {
    let catalog = DynamicModelCatalog::default();
    let now = Instant::now();
    let calls = Arc::new(AtomicUsize::new(0));
    for at in [now, now + Duration::from_secs(299)] {
        let calls = Arc::clone(&calls);
        catalog
            .models_for_at(None, vec![1], at, move |_| {
                let calls = Arc::clone(&calls);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(response(vec![model("gpt-5.6-sol", None, None)]))
                }
            })
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn all_failures_use_cache_only_within_stale_window() {
    let catalog = DynamicModelCatalog::default();
    let now = Instant::now();
    catalog
        .models_for_at(None, vec![1], now, |_| async {
            Ok(response(vec![model("gpt-5.6-sol", None, None)]))
        })
        .await
        .unwrap();

    let stale = catalog
        .models_for_at(None, vec![1], now + Duration::from_secs(301), |_| async {
            anyhow::bail!("offline")
        })
        .await
        .unwrap();
    assert_eq!(stale[0].model_id, "gpt-5.6-sol");

    let expired = catalog
        .models_for_at(None, vec![1], now + Duration::from_secs(1801), |_| async {
            anyhow::bail!("offline")
        })
        .await;
    assert!(matches!(
        expired,
        Err(ModelCatalogError::UpstreamModelCatalog { failures: 1 })
    ));
}

#[tokio::test]
async fn empty_candidates_return_no_available_credentials() {
    let result = DynamicModelCatalog::default()
        .models_for_at(None, Vec::new(), Instant::now(), |_| async {
            unreachable!()
        })
        .await;
    assert!(matches!(result, Err(ModelCatalogError::NoAvailableCredentials)));
}

#[tokio::test]
async fn concurrent_misses_share_one_refresh() {
    let catalog = Arc::new(DynamicModelCatalog::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let now = Instant::now();
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let catalog = Arc::clone(&catalog);
        let calls = Arc::clone(&calls);
        tasks.push(tokio::spawn(async move {
            catalog
                .models_for_at(None, vec![1], now, move |_| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        Ok(response(vec![model("gpt-5.6-sol", None, None)]))
                    }
                })
                .await
                .unwrap()
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
```

- [ ] **Step 6: 运行动态目录测试并确认实现缺失**

Run: `cargo test -j 1 kiro::model_catalog::tests -- --nocapture`

Expected: 编译失败，缺少 `DynamicModelCatalog`、`ModelCatalogError` 和方法实现。

- [ ] **Step 7: 实现目录 key、缓存、错误和模型合并**

在测试前加入以下生产类型与 helper；缓存返回克隆的模型向量，避免持锁跨越 await：

```rust
use std::{
    collections::HashMap,
    future::Future,
    sync::Arc,
    time::{Duration, Instant},
};

use futures::{stream, StreamExt};
use parking_lot::Mutex;
use tokio::sync::Mutex as AsyncMutex;

use crate::kiro::{
    model::available_models::{ListAvailableModelsResponse, TokenLimits, UpstreamModel},
    model_capabilities::{ModelAvailability, ModelAvailabilityCache},
};

const DEFAULT_CATALOG_TTL: Duration = Duration::from_secs(300);
const DEFAULT_STALE_TTL: Duration = Duration::from_secs(1800);
const DEFAULT_QUERY_CONCURRENCY: usize = 8;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CatalogKey {
    Global,
    Group(String),
}

impl CatalogKey {
    fn from_group(group: Option<&str>) -> Self {
        group
            .map(|value| Self::Group(value.to_string()))
            .unwrap_or(Self::Global)
    }
}

#[derive(Clone)]
struct CachedCatalog {
    fetched_at: Instant,
    models: Vec<UpstreamModel>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelCatalogError {
    #[error("没有符合客户端分组且未禁用的凭据")]
    NoAvailableCredentials,
    #[error("全部 {failures} 个凭据的上游模型目录查询失败")]
    UpstreamModelCatalog { failures: usize },
}

pub struct DynamicModelCatalog {
    ttl: Duration,
    stale_ttl: Duration,
    query_concurrency: usize,
    catalogs: Mutex<HashMap<CatalogKey, CachedCatalog>>,
    refresh_lock: AsyncMutex<()>,
    availability: Mutex<ModelAvailabilityCache>,
}

impl Default for DynamicModelCatalog {
    fn default() -> Self {
        Self::new(
            DEFAULT_CATALOG_TTL,
            DEFAULT_STALE_TTL,
            DEFAULT_QUERY_CONCURRENCY,
        )
    }
}

impl DynamicModelCatalog {
    pub fn new(ttl: Duration, stale_ttl: Duration, query_concurrency: usize) -> Self {
        Self {
            ttl,
            stale_ttl,
            query_concurrency: query_concurrency.max(1),
            catalogs: Mutex::new(HashMap::new()),
            refresh_lock: AsyncMutex::new(()),
            availability: Mutex::new(ModelAvailabilityCache::new(ttl)),
        }
    }

    pub fn availability(
        &self,
        credential_id: u64,
        model: &str,
        now: Instant,
    ) -> ModelAvailability {
        self.availability
            .lock()
            .availability(credential_id, model, now)
    }

    pub fn record_credential_models(
        &self,
        credential_id: u64,
        models: &[UpstreamModel],
        now: Instant,
    ) {
        self.availability.lock().insert(
            credential_id,
            models.iter().map(|model| model.model_id.clone()),
            now,
        );
    }

    fn cached_within(
        &self,
        key: &CatalogKey,
        now: Instant,
        max_age: Duration,
    ) -> Option<Vec<UpstreamModel>> {
        let catalogs = self.catalogs.lock();
        let cached = catalogs.get(key)?;
        (now.saturating_duration_since(cached.fetched_at) <= max_age)
            .then(|| cached.models.clone())
    }

    pub async fn models_for<F, Fut>(
        &self,
        group: Option<&str>,
        credential_ids: Vec<u64>,
        fetch: F,
    ) -> Result<Vec<UpstreamModel>, ModelCatalogError>
    where
        F: Fn(u64) -> Fut + Clone,
        Fut: Future<Output = anyhow::Result<ListAvailableModelsResponse>>,
    {
        self.models_for_at(group, credential_ids, Instant::now(), fetch)
            .await
    }

    async fn models_for_at<F, Fut>(
        &self,
        group: Option<&str>,
        credential_ids: Vec<u64>,
        now: Instant,
        fetch: F,
    ) -> Result<Vec<UpstreamModel>, ModelCatalogError>
    where
        F: Fn(u64) -> Fut + Clone,
        Fut: Future<Output = anyhow::Result<ListAvailableModelsResponse>>,
    {
        let key = CatalogKey::from_group(group);
        if let Some(models) = self.cached_within(&key, now, self.ttl) {
            return Ok(models);
        }

        let _refresh_guard = self.refresh_lock.lock().await;
        if let Some(models) = self.cached_within(&key, now, self.ttl) {
            return Ok(models);
        }
        if credential_ids.is_empty() {
            return Err(ModelCatalogError::NoAvailableCredentials);
        }

        let results = stream::iter(credential_ids.into_iter().map(|credential_id| {
            let fetch = fetch.clone();
            async move { (credential_id, fetch(credential_id).await) }
        }))
        .buffer_unordered(self.query_concurrency)
        .collect::<Vec<_>>()
        .await;

        let mut successful_models = Vec::new();
        let mut successes = 0;
        let mut failures = 0;
        for (credential_id, result) in results {
            match result {
                Ok(response) => {
                    successes += 1;
                    self.record_credential_models(credential_id, &response.models, now);
                    successful_models.extend(response.models);
                }
                Err(_) => {
                    failures += 1;
                    tracing::warn!(
                        credential_id,
                        "凭据模型目录查询失败，错误详情未写入公开响应"
                    );
                }
            }
        }

        if successes == 0 {
            if let Some(models) = self.cached_within(&key, now, self.stale_ttl) {
                tracing::warn!(failures, "全部上游目录失败，返回最近成功缓存");
                return Ok(models);
            }
            return Err(ModelCatalogError::UpstreamModelCatalog { failures });
        }

        let models = merge_models(successful_models);
        self.catalogs.lock().insert(
            key,
            CachedCatalog {
                fetched_at: now,
                models: models.clone(),
            },
        );
        Ok(models)
    }
}
```

实现合并 helper：

```rust
fn positive_max(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    [left, right]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .max()
}

fn non_empty(value: &Option<String>) -> bool {
    value.as_deref().is_some_and(|value| !value.trim().is_empty())
}

fn merge_models(models: Vec<UpstreamModel>) -> Vec<UpstreamModel> {
    let mut merged: HashMap<String, UpstreamModel> = HashMap::new();
    for mut incoming in models {
        let id = incoming.model_id.trim().to_string();
        if id.is_empty() {
            continue;
        }
        incoming.model_id = id.clone();
        merged
            .entry(id)
            .and_modify(|current| {
                if !non_empty(&current.model_name) && non_empty(&incoming.model_name) {
                    current.model_name = incoming.model_name.clone();
                }
                if !non_empty(&current.description) && non_empty(&incoming.description) {
                    current.description = incoming.description.clone();
                }
                let max_input_tokens = positive_max(
                    current
                        .token_limits
                        .as_ref()
                        .and_then(|limits| limits.max_input_tokens),
                    incoming
                        .token_limits
                        .as_ref()
                        .and_then(|limits| limits.max_input_tokens),
                );
                current.token_limits = max_input_tokens.map(|value| TokenLimits {
                    max_input_tokens: Some(value),
                });
            })
            .or_insert(incoming);
    }
    let mut models: Vec<_> = merged.into_values().collect();
    models.sort_by(|left, right| left.model_id.cmp(&right.model_id));
    models
}
```

- [ ] **Step 8: 注册模块并把 Provider 接到动态目录**

在 `src/kiro/mod.rs` 加入：

```rust
pub mod model_catalog;
```

在 `src/kiro/provider.rs` 导入：

```rust
use crate::kiro::model_catalog::{DynamicModelCatalog, ModelCatalogError};
```

把字段：

```rust
model_availability: Mutex<ModelAvailabilityCache>,
```

替换为：

```rust
model_catalog: DynamicModelCatalog,
```

构造器对应替换为：

```rust
model_catalog: DynamicModelCatalog::default(),
```

把 `model_availability_for` 的缓存读取与成功写入分别替换为：

```rust
let cached = self
    .model_catalog
    .availability(credential_id, model, Instant::now());
```

```rust
self.model_catalog
    .record_credential_models(credential_id, &response.models, Instant::now());
```

并在 `impl KiroProvider` 中加入公共目录方法：

```rust
pub async fn available_models(
    &self,
    group: Option<&str>,
) -> Result<Vec<crate::kiro::model::available_models::UpstreamModel>, ModelCatalogError> {
    let credential_ids = self
        .token_manager
        .enabled_credential_ids_in_group(group);
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
    self.model_catalog.seed_for_test(group, models, Instant::now());
}
```

为此在 `DynamicModelCatalog` 增加仅测试使用的精确 seed：

```rust
#[cfg(test)]
pub(crate) fn seed_for_test(
    &self,
    group: Option<&str>,
    models: Vec<UpstreamModel>,
    now: Instant,
) {
    self.catalogs.lock().insert(
        CatalogKey::from_group(group),
        CachedCatalog {
            fetched_at: now,
            models,
        },
    );
}
```

删除 `provider.rs` 中不再使用的 `ModelAvailabilityCache` import，但保留 `ModelAvailability`。

- [ ] **Step 9: 运行目录、Provider 和分组测试**

Run: `cargo test -j 1 model_catalog -- --nocapture`

Run: `cargo test -j 1 enabled_credential_ids_in_group -- --nocapture`

Run: `cargo test -j 1 model_availability -- --nocapture`

Expected: 三组测试全部通过；并发测试的 fetch 计数为 1，部分失败仍返回成功目录。

- [ ] **Step 10: 格式化并提交动态目录核心**

Run: `cargo fmt --all`

Run: `git add -- src/kiro/model_catalog.rs src/kiro/mod.rs src/kiro/token_manager.rs src/kiro/provider.rs && git commit -m "feat(models): 新增分组动态模型目录"`

Expected: 本地生成一个只包含动态目录核心与 Provider 接入的提交。

## Task 3：实现 Claude `4.8` / `4-8` / thinking 公共展示转换

**Files:**
- Create: `src/anthropic/model_catalog.rs`
- Modify: `src/anthropic/mod.rs:25-38`
- Test: `src/anthropic/model_catalog.rs`

- [ ] **Step 1: 先写动态模型展示测试**

创建 `src/anthropic/model_catalog.rs`，先加入以下测试 helper 与用例：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn upstream(id: &str, name: Option<&str>) -> UpstreamModel {
        UpstreamModel {
            model_id: id.to_string(),
            model_name: name.map(str::to_string),
            description: None,
            token_limits: None,
        }
    }

    #[test]
    fn claude_dot_version_generates_four_compatible_entries() {
        let ids: Vec<_> = public_models(vec![upstream("claude-opus-4.8", Some("Claude Opus 4.8"))])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(
            ids,
            vec![
                "claude-opus-4.8",
                "claude-opus-4-8",
                "claude-opus-4.8-thinking",
                "claude-opus-4-8-thinking",
            ]
        );
    }

    #[test]
    fn claude_hyphen_input_is_normalized_without_duplicates() {
        let models = public_models(vec![
            upstream("claude-opus-4-8", None),
            upstream("claude-opus-4.8", None),
        ]);
        let unique: std::collections::HashSet<_> =
            models.iter().map(|model| model.id.as_str()).collect();
        assert_eq!(models.len(), 4);
        assert_eq!(unique.len(), 4);
    }

    #[test]
    fn existing_thinking_model_does_not_get_double_suffix() {
        let ids: Vec<_> = public_models(vec![upstream("claude-opus-4.8-thinking", None)])
            .into_iter()
            .map(|model| model.id)
            .collect();
        assert_eq!(
            ids,
            vec!["claude-opus-4.8-thinking", "claude-opus-4-8-thinking"]
        );
    }

    #[test]
    fn gpt_and_unknown_models_keep_upstream_ids() {
        let models = public_models(vec![
            upstream("gpt-5.6-sol", Some("GPT Sol")),
            upstream("vendor-new-model", None),
        ]);
        assert_eq!(models[0].id, "gpt-5.6-sol");
        assert_eq!(models[0].owned_by, "openai");
        assert_eq!(models[1].id, "vendor-new-model");
        assert_eq!(models[1].owned_by, "kiro");
    }

    #[test]
    fn public_max_tokens_never_uses_upstream_input_limit() {
        let mut input = upstream("gpt-5.6-sol", None);
        input.token_limits = Some(TokenLimits {
            max_input_tokens: Some(1_000_000),
        });
        assert_eq!(public_models(vec![input])[0].max_tokens, 64_000);
    }
}
```

- [ ] **Step 2: 运行展示测试并确认生产函数缺失**

Run: `cargo test -j 1 anthropic::model_catalog::tests -- --nocapture`

Expected: 编译失败，缺少 `public_models`。

- [ ] **Step 3: 实现精确 Claude 解析、owner 和稳定排序**

在测试模块前加入：

```rust
use std::collections::HashMap;

use crate::{
    anthropic::types::Model,
    kiro::model::available_models::UpstreamModel,
};

const PUBLIC_CREATED_AT: i64 = 1_781_481_600;
const PUBLIC_MAX_OUTPUT_TOKENS: i32 = 64_000;
const CLAUDE_FAMILIES: &[&str] = &["opus", "sonnet", "haiku", "fable", "mythos"];

#[derive(Clone)]
struct PublicCandidate {
    canonical_sort_id: String,
    rank: u8,
    id: String,
    display_name: String,
    owned_by: String,
}

fn all_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn claude_version_ids(id: &str) -> Option<(String, String, bool)> {
    let lower = id.to_ascii_lowercase();
    let (base, thinking) = lower
        .strip_suffix("-thinking")
        .map(|base| (base, true))
        .unwrap_or((&lower, false));
    let parts: Vec<_> = base.split('-').collect();
    if parts.len() < 3
        || parts[0] != "claude"
        || !CLAUDE_FAMILIES.contains(&parts[1])
    {
        return None;
    }

    let (major, minor) = match parts.as_slice() {
        ["claude", _, version] if all_digits(version) => (*version, None),
        ["claude", _, version] => {
            let (major, minor) = version.split_once('.')?;
            (all_digits(major) && all_digits(minor)).then_some((major, Some(minor)))?
        }
        ["claude", _, major, minor] if all_digits(major) && all_digits(minor) => {
            (*major, Some(*minor))
        }
        _ => return None,
    };
    let prefix = format!("claude-{}", parts[1]);
    let canonical = minor
        .map(|minor| format!("{prefix}-{major}.{minor}"))
        .unwrap_or_else(|| format!("{prefix}-{major}"));
    let hyphen = minor
        .map(|minor| format!("{prefix}-{major}-{minor}"))
        .unwrap_or_else(|| canonical.clone());
    Some((canonical, hyphen, thinking))
}

fn owner_for(id: &str) -> &'static str {
    match id {
        value if value.starts_with("claude-") => "anthropic",
        value if value.starts_with("gpt-") => "openai",
        value if value.starts_with("deepseek-") => "deepseek",
        value if value.starts_with("minimax-") => "minimax",
        value if value.starts_with("glm-") => "zhipu",
        value if value.starts_with("qwen") => "qwen",
        _ => "kiro",
    }
}

fn candidate(
    canonical_sort_id: &str,
    rank: u8,
    id: String,
    base_name: &str,
    thinking: bool,
) -> PublicCandidate {
    PublicCandidate {
        canonical_sort_id: canonical_sort_id.to_string(),
        rank,
        owned_by: owner_for(&id).to_string(),
        display_name: if thinking {
            format!("{base_name} (Thinking)")
        } else {
            base_name.to_string()
        },
        id,
    }
}

pub fn public_models(upstream: Vec<UpstreamModel>) -> Vec<Model> {
    let mut candidates = Vec::new();
    for model in upstream {
        let id = model.model_id.trim();
        if id.is_empty() {
            continue;
        }
        let base_name = model
            .model_name
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(id);
        if let Some((canonical, hyphen, already_thinking)) = claude_version_ids(id) {
            if already_thinking {
                candidates.push(candidate(
                    &canonical,
                    2,
                    format!("{canonical}-thinking"),
                    base_name.trim_end_matches(" (Thinking)"),
                    true,
                ));
                if hyphen != canonical {
                    candidates.push(candidate(
                        &canonical,
                        3,
                        format!("{hyphen}-thinking"),
                        base_name.trim_end_matches(" (Thinking)"),
                        true,
                    ));
                }
            } else {
                candidates.push(candidate(&canonical, 0, canonical.clone(), base_name, false));
                if hyphen != canonical {
                    candidates.push(candidate(&canonical, 1, hyphen.clone(), base_name, false));
                }
                candidates.push(candidate(
                    &canonical,
                    2,
                    format!("{canonical}-thinking"),
                    base_name,
                    true,
                ));
                if hyphen != canonical {
                    candidates.push(candidate(
                        &canonical,
                        3,
                        format!("{hyphen}-thinking"),
                        base_name,
                        true,
                    ));
                }
            }
        } else {
            candidates.push(candidate(id, 0, id.to_string(), base_name, false));
        }
    }

    let mut unique: HashMap<String, PublicCandidate> = HashMap::new();
    for candidate in candidates {
        unique.entry(candidate.id.clone()).or_insert(candidate);
    }
    let mut unique: Vec<_> = unique.into_values().collect();
    unique.sort_by(|left, right| {
        left.canonical_sort_id
            .cmp(&right.canonical_sort_id)
            .then(left.rank.cmp(&right.rank))
            .then(left.id.cmp(&right.id))
    });
    unique
        .into_iter()
        .map(|candidate| Model {
            id: candidate.id,
            object: "model".to_string(),
            created: PUBLIC_CREATED_AT,
            owned_by: candidate.owned_by,
            display_name: candidate.display_name,
            model_type: "chat".to_string(),
            max_tokens: PUBLIC_MAX_OUTPUT_TOKENS,
        })
        .collect()
}
```

在 `src/anthropic/mod.rs` 注册：

```rust
pub(crate) mod model_catalog;
```

- [ ] **Step 4: 运行展示转换测试并确认通过**

Run: `cargo test -j 1 anthropic::model_catalog::tests -- --nocapture`

Expected: 五个测试全部通过；GPT 不产生别名，Claude `4.8` 和 `4-8` 去重为四个入口。

- [ ] **Step 5: 格式化并提交协议展示转换**

Run: `cargo fmt --all`

Run: `git add -- src/anthropic/model_catalog.rs src/anthropic/mod.rs && git commit -m "feat(models): 生成动态模型兼容别名"`

Expected: 本地生成一个只包含协议展示层的提交。

## Task 4：将 `/v1/models` 接到认证分组和动态 Provider

**Files:**
- Modify: `src/anthropic/handlers.rs:1714-1940, 6554-6589, 6648-6656`
- Modify: `src/anthropic/router.rs:115-end`
- Test: `src/anthropic/router.rs`

- [ ] **Step 1: 先写无 Provider 稳定错误路由测试**

在 `src/anthropic/router.rs` 测试模块加入：

```rust
#[tokio::test]
async fn models_route_without_provider_returns_stable_503() {
    let keys = Arc::new(crate::admin::ClientKeyManager::new());
    keys.create_with_key(
        "models".to_string(),
        None,
        None,
        "csk_models-test".to_string(),
    );
    let app = create_router(
        None,
        false,
        ToolCompatibilityMode::default(),
        Some(keys),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("x-api-key", "csk_models-test")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["type"], "no_available_credentials");
}
```

- [ ] **Step 2: 运行路由测试并确认旧静态目录返回 200**

Run: `cargo test -j 1 models_route_without_provider_returns_stable_503 -- --exact`

Expected: 断言失败，实际状态仍为 `200 OK`，证明旧 handler 不读取 Provider。

- [ ] **Step 3: 删除静态目录并实现动态 handler**

从 `src/anthropic/handlers.rs` 完整删除 `fn available_models() -> Vec<Model>` 及仅验证该静态函数的四个测试：

- `available_models_include_opus_4_7_variants`
- `available_models_include_native_kiro_models`
- `available_models_have_unique_ids`
- `available_models_include_4_8_variants`

将 `get_models` 替换为：

```rust
pub async fn get_models(
    State(state): State<AppState>,
    Extension(key_ctx): Extension<KeyContext>,
) -> Response {
    tracing::info!(group = ?key_ctx.group, "Received GET /v1/models request");

    let Some(provider) = state.kiro_provider else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "no_available_credentials",
                "No Kiro provider is configured.",
            )),
        )
            .into_response();
    };

    match provider.available_models(key_ctx.group.as_deref()).await {
        Ok(upstream) => Json(ModelsResponse {
            object: "list".to_string(),
            data: super::model_catalog::public_models(upstream),
        })
        .into_response(),
        Err(crate::kiro::model_catalog::ModelCatalogError::NoAvailableCredentials) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse::new(
                "no_available_credentials",
                "No enabled credentials are available for this key group.",
            )),
        )
            .into_response(),
        Err(crate::kiro::model_catalog::ModelCatalogError::UpstreamModelCatalog {
            failures,
        }) => {
            tracing::error!(failures, "全部动态模型目录查询失败且没有可用缓存");
            (
                StatusCode::BAD_GATEWAY,
                Json(ErrorResponse::new(
                    "upstream_model_catalog_error",
                    "Upstream model catalog is temporarily unavailable.",
                )),
            )
                .into_response()
        }
    }
}
```

同步删除 handlers 顶部仅为静态目录使用的 `Model` import；保留 `ModelsResponse`。

- [ ] **Step 4: 运行无 Provider 路由测试并确认通过**

Run: `cargo test -j 1 models_route_without_provider_returns_stable_503 -- --exact`

Expected: 测试通过，响应 error type 为 `no_available_credentials`。

- [ ] **Step 5: 写经过 Key 分组认证并返回动态 GPT 的路由测试**

在 router 测试模块增加构造 helper：

```rust
fn provider_with_grouped_credential(group: &str) -> Arc<KiroProvider> {
    use std::collections::HashMap;
    use crate::kiro::{
        endpoint::{IdeEndpoint, KiroEndpoint},
        model::credentials::KiroCredentials,
        token_manager::MultiTokenManager,
    };

    let mut credential = KiroCredentials::default();
    credential.access_token = Some("test-access-token".to_string());
    credential.groups = vec![group.to_string()];
    let manager = Arc::new(
        MultiTokenManager::new(
            crate::model::config::Config::default(),
            vec![credential],
            None,
            None,
            false,
        )
        .unwrap(),
    );
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    endpoints.insert("ide".to_string(), Arc::new(IdeEndpoint::new()));
    Arc::new(KiroProvider::with_proxy(
        manager,
        None,
        endpoints,
        "ide".to_string(),
        None,
    ))
}
```

加入测试：

```rust
#[tokio::test]
async fn models_route_uses_authenticated_key_group_and_returns_dynamic_gpt() {
    let provider = provider_with_grouped_credential("gpt-group");
    provider.seed_model_catalog_for_test(
        Some("gpt-group"),
        vec![crate::kiro::model::available_models::UpstreamModel {
            model_id: "gpt-5.6-sol".to_string(),
            model_name: Some("GPT Sol".to_string()),
            description: None,
            token_limits: None,
        }],
    );
    let keys = Arc::new(crate::admin::ClientKeyManager::new());
    keys.create_with_key(
        "gpt".to_string(),
        Some("gpt-group".to_string()),
        None,
        "csk_gpt-models".to_string(),
    );
    let app = create_router(
        Some(provider),
        false,
        ToolCompatibilityMode::default(),
        Some(keys),
        None,
        None,
        None,
        None,
        None,
        None,
        None,
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("x-api-key", "csk_gpt-models")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["object"], "list");
    assert!(
        json["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|model| model["id"] == "gpt-5.6-sol")
    );
}
```

- [ ] **Step 6: 运行动态 GPT 路由测试并确认通过**

Run: `cargo test -j 1 models_route_uses_authenticated_key_group_and_returns_dynamic_gpt -- --exact`

Expected: 状态为 200，响应中包含 `gpt-5.6-sol`，测试不访问公网。

- [ ] **Step 7: 验证静态列表不再是生产来源**

Run: `rg -n "fn available_models|available_models\(\)" src/anthropic/handlers.rs`

Expected: 没有输出。

Run: `rg -n "gpt-5\.6-sol" src/anthropic src/kiro --glob '!model_catalog.rs' --glob '!*.md'`

Expected: 只允许测试夹具命中；生产数组中没有 GPT 静态条目。

- [ ] **Step 8: 格式化并提交动态路由接入**

Run: `cargo fmt --all`

Run: `git add -- src/anthropic/handlers.rs src/anthropic/router.rs && git commit -m "feat(models): 动态返回分组可用模型"`

Expected: 本地生成一个只包含 `/v1/models` 动态路由与集成测试的提交。

## Task 5：文档、回归验证与最终本地提交检查

**Files:**
- Modify: `CHANGELOG.md:7-20`
- Verify: `src/kiro/model/available_models.rs`
- Verify: `src/kiro/model_catalog.rs`
- Verify: `src/kiro/token_manager.rs`
- Verify: `src/kiro/provider.rs`
- Verify: `src/anthropic/model_catalog.rs`
- Verify: `src/anthropic/handlers.rs`
- Verify: `src/anthropic/router.rs`

- [ ] **Step 1: 在 Unreleased 记录行为和客户影响**

在 `CHANGELOG.md` 的 `## [Unreleased]` 后加入：

```markdown
### ✨ 优化 — 公共模型目录改为动态上游发现

- **GPT 等真实模型可自动发现**：`GET /v1/models` 按客户端 Key 的账号分组汇总未禁用凭据实际返回的 `ListAvailableModels`，NewAPI 不再只能看到内置 Claude 静态数组。
- **完整分页与稳定缓存**：单凭据最多读取 10 页 / 500 个模型，公共目录最多 8 路并发刷新；正常缓存 5 分钟，上游全部失败时仅回退 30 分钟内的最近成功结果。
- **严格分组隔离**：绑定分组的 Key 只看到该组账号真实可路由的模型；未绑定分组的 Key 汇总全部未禁用账号。临时 429、RPM 窗口和在途数不会让已有模型短暂消失。
- **Claude 命名继续兼容**：上游 Claude 版本动态生成点号、连字符及 thinking 入口；GPT 与未知模型保持上游原始 ID，不生成虚构快照或 GPT 别名。
- **客户可感知变化**：第一次读取目录可能等待上游刷新；全部上游失败且没有近期成功缓存时返回明确的 502/503，而不是看似正常但可能不可用的静态列表。本改动不修改对话、首字节、计费、工具调用或凭据请求调度。
```

- [ ] **Step 2: 运行格式与静态检查**

Run: `cargo fmt --all -- --check`

Expected: 退出码 0，无格式差异。

Run: `cargo check -j 1 --all-targets`

Expected: 退出码 0，无编译错误。

- [ ] **Step 3: 运行模型目录全部聚焦测试**

Run: `cargo test -j 1 available_models -- --nocapture`

Run: `cargo test -j 1 model_catalog -- --nocapture`

Run: `cargo test -j 1 models_route_ -- --nocapture`

Expected: 所有聚焦测试通过；没有真实公网请求。

- [ ] **Step 4: 运行完整 Rust 测试集**

Run: `cargo test -j 1`

Expected: 退出码 0，全部单元和集成测试通过。若本机再次出现 Windows 分页文件不足，记录失败输出并在 1GB 内存限制的服务器构建器上用同一 commit 执行 `cargo test -j 1`；不得把资源不足误报为代码通过。

- [ ] **Step 5: 提交 CHANGELOG**

Run: `git add -- CHANGELOG.md && git commit -m "docs(models): 记录动态模型发现"`

Expected: 本地生成一个只包含 CHANGELOG 的提交。

- [ ] **Step 6: 审查最终变更范围和提交历史**

Run: `git status --short`

Expected: 没有本任务遗留的未提交文件；若存在用户原有文件，保持未暂存并在交付中逐项说明。

Run: `git diff HEAD~5..HEAD --check`

Expected: 退出码 0，无尾随空格或冲突标记。

Run: `git log -5 --oneline`

Expected: 能看到分页、动态目录、兼容别名、动态路由和 CHANGELOG 五个小提交；不执行 `git push`。

## 完成标准

- 公共 `/v1/models` 不再调用任何静态模型数组。
- 正式上游返回的 `gpt-5.6-sol`、`gpt-5.6-terra`、`gpt-5.6-luna` 能原名出现在对应分组目录。
- `claude-opus-4.8` 与 `claude-opus-4-8` 请求兼容保持不变，目录同时展示两种命名及各自 thinking 入口。
- 单凭据后续页失败、重复游标或超过 10 页时不合并半份目录。
- 分组严格隔离，禁用凭据不参与；临时 429/RPM/在途状态不改变模型权限目录。
- 五分钟内缓存命中不访问上游，全部失败只在三十分钟内回退最近成功缓存。
- 所有上游失败且没有可用缓存时为 `502 upstream_model_catalog_error`；无 Provider 或无候选凭据时为 `503 no_available_credentials`。
- 聚焦测试、完整测试、格式和编译检查均有真实通过证据。
- 所有变更只在本地提交，未获得用户明确指令前不推送 GitHub、不部署服务器。
