use std::{
    collections::HashMap,
    future::Future,
    time::{Duration, Instant},
};

use futures::{StreamExt, stream};
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

    pub fn availability(&self, credential_id: u64, model: &str, now: Instant) -> ModelAvailability {
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
        (now.saturating_duration_since(cached.fetched_at) <= max_age).then(|| cached.models.clone())
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

        let mut results = stream::iter(credential_ids.into_iter().enumerate().map(
            |(position, credential_id)| {
                let fetch = fetch.clone();
                async move { (position, credential_id, fetch(credential_id).await) }
            },
        ))
        .buffer_unordered(self.query_concurrency)
        .collect::<Vec<_>>()
        .await;
        results.sort_by_key(|(position, _, _)| *position);

        let mut successful_models = Vec::new();
        let mut successes = 0;
        let mut failures = 0;
        for (_, credential_id, result) in results {
            match result {
                Ok(response) => {
                    successes += 1;
                    self.record_credential_models(credential_id, &response.models, now);
                    successful_models.extend(response.models);
                }
                Err(_) => {
                    failures += 1;
                    tracing::warn!(credential_id, "凭据模型目录查询失败");
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
}

fn positive_max(left: Option<i64>, right: Option<i64>) -> Option<i64> {
    [left, right]
        .into_iter()
        .flatten()
        .filter(|value| *value > 0)
        .max()
}

fn non_empty(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kiro::model::available_models::{
        ListAvailableModelsResponse, TokenLimits, UpstreamModel,
    };
    use crate::kiro::model_capabilities::ModelAvailability;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::time::{Duration, Instant};

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
            models[0]
                .token_limits
                .as_ref()
                .and_then(|limits| limits.max_input_tokens),
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
    async fn fresh_cache_prevents_duplicate_queries_and_is_group_isolated() {
        let catalog = DynamicModelCatalog::default();
        let now = Instant::now();
        let calls = Arc::new(AtomicUsize::new(0));
        for at in [now, now + Duration::from_secs(299)] {
            let calls = Arc::clone(&calls);
            catalog
                .models_for_at(Some("g1"), vec![1], at, move |_| {
                    let calls = Arc::clone(&calls);
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(response(vec![model("gpt-5.6-sol", None, None)]))
                    }
                })
                .await
                .unwrap();
        }
        let calls_for_g2 = Arc::clone(&calls);
        catalog
            .models_for_at(Some("g2"), vec![2], now, move |_| {
                let calls = Arc::clone(&calls_for_g2);
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    Ok(response(vec![model("gpt-5.6-terra", None, None)]))
                }
            })
            .await
            .unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 2);
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
        assert!(matches!(
            result,
            Err(ModelCatalogError::NoAvailableCredentials)
        ));
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

    #[tokio::test]
    async fn credential_queries_respect_configured_concurrency_limit() {
        let catalog =
            DynamicModelCatalog::new(Duration::from_secs(300), Duration::from_secs(1800), 3);
        let current = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let observed_peak = Arc::clone(&peak);

        catalog
            .models_for_at(
                None,
                (1..=12).collect(),
                Instant::now(),
                move |credential_id| {
                    let current = Arc::clone(&current);
                    let peak = Arc::clone(&peak);
                    async move {
                        let active = current.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(active, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        current.fetch_sub(1, Ordering::SeqCst);
                        Ok(response(vec![model(
                            &format!("model-{credential_id}"),
                            None,
                            None,
                        )]))
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(observed_peak.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn merged_metadata_follows_candidate_order_not_completion_order() {
        let catalog = DynamicModelCatalog::default();
        let models = catalog
            .models_for_at(
                None,
                vec![1, 2],
                Instant::now(),
                |credential_id| async move {
                    if credential_id == 1 {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        Ok(response(vec![model(
                            "gpt-5.6-sol",
                            Some("Preferred First Credential Name"),
                            None,
                        )]))
                    } else {
                        Ok(response(vec![model(
                            "gpt-5.6-sol",
                            Some("Faster Second Credential Name"),
                            None,
                        )]))
                    }
                },
            )
            .await
            .unwrap();

        assert_eq!(
            models[0].model_name.as_deref(),
            Some("Preferred First Credential Name")
        );
    }
}
