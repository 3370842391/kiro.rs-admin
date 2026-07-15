use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{StreamExt, stream};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;
use tokio::time::timeout;

use crate::anthropic::model_profile::{
    ModelProfileError, ModelProfileFile, ModelProfileStore, ProfileCandidate, ProfileFieldName,
    ProfilePreview, ProfilePreviewChange, canonical_model_id,
};
use crate::kiro::model::available_models::ListAvailableModelsResponse;
use crate::kiro::token_manager::MultiTokenManager;

const MODELS_DEV_URL: &str = "https://models.dev/api.json";
const MODELS_DEV_CACHE_TTL: Duration = Duration::from_secs(30 * 60);
const MODELS_DEV_TIMEOUT: Duration = Duration::from_secs(10);
const KIRO_TIMEOUT: Duration = Duration::from_secs(15);
const PREVIEW_TTL: Duration = Duration::from_secs(5 * 60);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelsDevItem {
    pub model_id: String,
    pub context: Option<i64>,
    pub output: Option<i64>,
    pub knowledge: Option<String>,
    pub release_date: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelsDevCatalog {
    entries: BTreeMap<String, ModelsDevItem>,
    warnings: Vec<String>,
}

impl ModelsDevCatalog {
    pub fn parse(input: &str) -> Result<Self, SyncError> {
        let root: Value = serde_json::from_str(input)
            .map_err(|error| SyncError::PublicCatalog(error.to_string()))?;
        let models = root
            .get("anthropic")
            .and_then(|provider| provider.get("models"))
            .and_then(Value::as_object)
            .ok_or_else(|| SyncError::PublicCatalog("缺少 anthropic.models 对象".to_string()))?;
        let mut catalog = Self::default();
        let mut conflicted = BTreeSet::new();
        for (raw_id, raw) in models {
            let declared = raw.get("id").and_then(Value::as_str).unwrap_or(raw_id);
            let Ok(model_id) = canonical_model_id(declared) else {
                catalog
                    .warnings
                    .push(format!("models.dev 忽略非法模型 ID: {declared}"));
                continue;
            };
            if conflicted.contains(&model_id) {
                continue;
            }
            let limit = raw.get("limit");
            let item = ModelsDevItem {
                model_id: model_id.clone(),
                context: limit
                    .and_then(|value| value.get("context"))
                    .and_then(Value::as_i64),
                output: limit
                    .and_then(|value| value.get("output"))
                    .and_then(Value::as_i64),
                knowledge: raw
                    .get("knowledge")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
                release_date: raw
                    .get("release_date")
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned),
            };
            if catalog.entries.insert(model_id.clone(), item).is_some() {
                catalog.entries.remove(&model_id);
                conflicted.insert(model_id.clone());
                catalog.warnings.push(format!(
                    "models.dev anthropic 中多个条目规范化为 {model_id}，已跳过"
                ));
            }
        }
        Ok(catalog)
    }

    pub fn get(&self, model_id: &str) -> Option<&ModelsDevItem> {
        let canonical = canonical_model_id(model_id).ok()?;
        self.entries.get(&canonical)
    }

    fn candidates(&self) -> Vec<ProfileCandidate> {
        self.entries
            .values()
            .flat_map(models_dev_item_candidates)
            .collect()
    }
}

fn models_dev_item_candidates(item: &ModelsDevItem) -> Vec<ProfileCandidate> {
    let mut values = Vec::new();
    if let Some(value) = item.context.filter(|value| *value > 0) {
        values.push(ProfileCandidate::integer(
            &item.model_id,
            ProfileFieldName::ContextWindowTokens,
            value,
            "models.dev:anthropic",
        ));
    }
    if let Some(value) = item.output.filter(|value| *value > 0) {
        values.push(ProfileCandidate::integer(
            &item.model_id,
            ProfileFieldName::MaxOutputTokens,
            value,
            "models.dev:anthropic",
        ));
    }
    if let Some(value) = item.knowledge.as_deref() {
        values.push(ProfileCandidate::string(
            &item.model_id,
            ProfileFieldName::KnowledgeCutoff,
            value,
            "models.dev:anthropic",
        ));
    }
    if let Some(value) = item.release_date.as_deref() {
        values.push(ProfileCandidate::string(
            &item.model_id,
            ProfileFieldName::ReleaseDate,
            value,
            "models.dev:anthropic",
        ));
    }
    values
}

pub fn candidates_from_kiro(response: &ListAvailableModelsResponse) -> Vec<ProfileCandidate> {
    response
        .models
        .iter()
        .filter_map(|model| {
            let value = model.token_limits.as_ref()?.max_input_tokens?;
            (value > 0).then(|| {
                ProfileCandidate::integer(
                    &model.model_id,
                    ProfileFieldName::ContextWindowTokens,
                    value,
                    "kiro:list-available-models",
                )
            })
        })
        .collect()
}

struct ModelsDevClient {
    client: reqwest::Client,
    cache: Mutex<Option<(Instant, ModelsDevCatalog)>>,
}

impl ModelsDevClient {
    fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(MODELS_DEV_TIMEOUT)
                .build()
                .expect("创建 models.dev HTTP client 失败"),
            cache: Mutex::new(None),
        }
    }

    async fn fetch(&self, force: bool) -> Result<ModelsDevCatalog, SyncError> {
        if !force
            && let Some((cached_at, catalog)) = self.cache.lock().as_ref()
            && cached_at.elapsed() < MODELS_DEV_CACHE_TTL
        {
            return Ok(catalog.clone());
        }
        let body = timeout(MODELS_DEV_TIMEOUT, self.client.get(MODELS_DEV_URL).send())
            .await
            .map_err(|_| SyncError::PublicCatalog("请求超时".to_string()))?
            .map_err(|error| SyncError::PublicCatalog(error.to_string()))?
            .error_for_status()
            .map_err(|error| SyncError::PublicCatalog(error.to_string()))?
            .text()
            .await
            .map_err(|error| SyncError::PublicCatalog(error.to_string()))?;
        let catalog = ModelsDevCatalog::parse(&body)?;
        *self.cache.lock() = Some((Instant::now(), catalog.clone()));
        Ok(catalog)
    }
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCollection {
    pub candidates: Vec<ProfileCandidate>,
    pub warnings: Vec<String>,
    pub successful_sources: usize,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PreviewEnvelope {
    pub preview_id: String,
    pub revision: u64,
    pub changes: Vec<ProfilePreviewChange>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ApplyPreviewChange {
    pub id: String,
    pub model_id: String,
    pub field: ProfileFieldName,
    pub value: Value,
    pub source: String,
    #[serde(default)]
    pub lock: bool,
}

struct PreviewCache {
    entries: Mutex<HashMap<String, (Instant, ProfilePreview)>>,
    ttl: Duration,
}

impl PreviewCache {
    fn new(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
        }
    }

    fn insert(&self, preview: ProfilePreview) -> String {
        let id = format!("preview_{}", uuid::Uuid::new_v4().simple());
        let mut entries = self.entries.lock();
        entries.retain(|_, (created, _)| created.elapsed() <= self.ttl);
        entries.insert(id.clone(), (Instant::now(), preview));
        id
    }

    fn get_valid(&self, id: &str) -> Result<ProfilePreview, PreviewCacheError> {
        let mut entries = self.entries.lock();
        let Some((created, preview)) = entries.get(id) else {
            return Err(PreviewCacheError::Gone);
        };
        if created.elapsed() > self.ttl {
            entries.remove(id);
            return Err(PreviewCacheError::Gone);
        }
        Ok(preview.clone())
    }

    fn consume(&self, id: &str) {
        self.entries.lock().remove(id);
    }
}

pub struct ModelProfileSyncService {
    token_manager: Arc<MultiTokenManager>,
    models_dev: ModelsDevClient,
    previews: PreviewCache,
}

impl ModelProfileSyncService {
    pub fn new(token_manager: Arc<MultiTokenManager>) -> Self {
        Self {
            token_manager,
            models_dev: ModelsDevClient::new(),
            previews: PreviewCache::new(PREVIEW_TTL),
        }
    }

    fn healthy_credential_ids(&self) -> Vec<u64> {
        self.token_manager
            .snapshot()
            .entries
            .into_iter()
            .filter(|entry| {
                !entry.disabled
                    && entry.throttled_remaining_secs.is_none()
                    && entry.rate_limited_remaining_ms.is_none()
            })
            .map(|entry| entry.id)
            .collect()
    }

    pub async fn collect_one(
        &self,
        model_id: &str,
        credential_id: Option<u64>,
        force_public: bool,
    ) -> Result<SyncCollection, SyncError> {
        let target = canonical_model_id(model_id)?;
        let ids = credential_id
            .map(|id| vec![id])
            .unwrap_or_else(|| self.healthy_credential_ids().into_iter().take(1).collect());
        let mut result = SyncCollection::default();
        if let Some(id) = ids.first().copied() {
            match timeout(
                KIRO_TIMEOUT,
                self.token_manager.get_available_models_for(id),
            )
            .await
            {
                Ok(Ok(response)) => {
                    result.successful_sources += 1;
                    result.candidates.extend(
                        candidates_from_kiro(&response)
                            .into_iter()
                            .filter(|candidate| candidate.model_id == target),
                    );
                }
                Ok(Err(error)) => result
                    .warnings
                    .push(format!("Kiro 模型列表查询失败: {error}")),
                Err(_) => result.warnings.push("Kiro 模型列表查询超时".to_string()),
            }
        } else {
            result
                .warnings
                .push("没有健康凭据可用于 Kiro 查询".to_string());
        }
        self.collect_public(&target, force_public, &mut result)
            .await;
        if result.successful_sources == 0 {
            return Err(SyncError::AllSourcesFailed(result.warnings));
        }
        Ok(result)
    }

    pub async fn collect_all(&self, force_public: bool) -> Result<SyncCollection, SyncError> {
        let ids = self.healthy_credential_ids();
        let manager = self.token_manager.clone();
        let observations = stream::iter(ids.into_iter().map(|id| {
            let manager = manager.clone();
            async move {
                (
                    id,
                    timeout(KIRO_TIMEOUT, manager.get_available_models_for(id)).await,
                )
            }
        }))
        .buffer_unordered(4)
        .collect::<Vec<_>>()
        .await;
        let mut result = SyncCollection::default();
        let mut contexts: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for (id, observation) in observations {
            match observation {
                Ok(Ok(response)) => {
                    result.successful_sources += 1;
                    for candidate in candidates_from_kiro(&response) {
                        if let Some(value) = candidate.value.as_i64() {
                            contexts.entry(candidate.model_id).or_default().push(value);
                        }
                    }
                }
                Ok(Err(error)) => result
                    .warnings
                    .push(format!("凭据 #{id} Kiro 查询失败: {error}")),
                Err(_) => result.warnings.push(format!("凭据 #{id} Kiro 查询超时")),
            }
        }
        for (model_id, mut values) in contexts {
            values.sort_unstable();
            values.dedup();
            if values.len() > 1 {
                result.warnings.push(format!(
                    "{model_id} 的 Kiro 上下文观测冲突 {:?}，采用保守最小值",
                    values
                ));
            }
            if let Some(value) = values.first().copied() {
                result.candidates.push(ProfileCandidate::integer(
                    model_id,
                    ProfileFieldName::ContextWindowTokens,
                    value,
                    "kiro:list-available-models",
                ));
            }
        }
        self.collect_public_all(force_public, &mut result).await;
        if result.successful_sources == 0 {
            return Err(SyncError::AllSourcesFailed(result.warnings));
        }
        Ok(result)
    }

    async fn collect_public(&self, model_id: &str, force: bool, result: &mut SyncCollection) {
        match self.models_dev.fetch(force).await {
            Ok(catalog) => {
                result.successful_sources += 1;
                result.warnings.extend(catalog.warnings.clone());
                if let Some(item) = catalog.get(model_id) {
                    result.candidates.extend(models_dev_item_candidates(item));
                }
            }
            Err(error) => result.warnings.push(error.to_string()),
        }
    }

    async fn collect_public_all(&self, force: bool, result: &mut SyncCollection) {
        match self.models_dev.fetch(force).await {
            Ok(catalog) => {
                result.successful_sources += 1;
                result.warnings.extend(catalog.warnings.clone());
                result.candidates.extend(catalog.candidates());
            }
            Err(error) => result.warnings.push(error.to_string()),
        }
    }

    pub fn create_preview(
        &self,
        store: &ModelProfileStore,
        candidates: Vec<ProfileCandidate>,
    ) -> Result<PreviewEnvelope, SyncError> {
        let preview = store.preview(candidates)?;
        let preview_id = self.previews.insert(preview.clone());
        Ok(PreviewEnvelope {
            preview_id,
            revision: preview.revision,
            changes: preview.changes,
        })
    }

    pub fn apply_preview(
        &self,
        store: &ModelProfileStore,
        preview_id: &str,
        base_revision: u64,
        changes: &[ApplyPreviewChange],
    ) -> Result<ModelProfileFile, SyncError> {
        let preview = self.previews.get_valid(preview_id)?;
        if preview.revision != base_revision {
            return Err(SyncError::ModelProfile(
                ModelProfileError::RevisionConflict {
                    expected: base_revision,
                    actual: preview.revision,
                },
            ));
        }
        let mut selected_ids = Vec::with_capacity(changes.len());
        for requested in changes {
            let canonical = canonical_model_id(&requested.model_id)?;
            let Some(change) = preview.changes.iter().find(|change| {
                change.id == requested.id
                    && change.model_id == canonical
                    && change.field == requested.field
                    && change.value == requested.value
                    && change.source == requested.source
                    && change.lock == requested.lock
            }) else {
                return Err(SyncError::PreviewMismatch);
            };
            selected_ids.push(change.id.clone());
        }
        selected_ids.sort();
        selected_ids.dedup();
        if selected_ids.len() != changes.len() {
            return Err(SyncError::PreviewMismatch);
        }
        let updated = store
            .apply_preview(&preview, &selected_ids)
            .map_err(SyncError::from)?;
        self.previews.consume(preview_id);
        Ok(updated)
    }
}

#[derive(Debug, Error)]
pub enum PreviewCacheError {
    #[error("模型资料预览不存在、已过期或已消费")]
    Gone,
}

#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    ModelProfile(#[from] ModelProfileError),
    #[error("models.dev 数据不可用: {0}")]
    PublicCatalog(String),
    #[error("所有模型资料来源均失败: {0:?}")]
    AllSourcesFailed(Vec<String>),
    #[error(transparent)]
    Preview(#[from] PreviewCacheError),
    #[error("应用内容与服务器保存的预览不一致")]
    PreviewMismatch,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anthropic::model_profile::ManualField;
    use crate::kiro::model::available_models::{TokenLimits, UpstreamModel};
    use crate::model::config::Config;

    #[test]
    fn models_dev_parser_selects_only_exact_anthropic_provider() {
        let input = r#"{
          "azure": {"models": {"claude-opus-4-8": {"limit":{"context":123}}}},
          "anthropic": {"models": {"claude-opus-4-8": {
            "id":"claude-opus-4-8", "knowledge":"2026-01", "release_date":"2026-05-28",
            "limit":{"context":1000000,"output":128000}
          }}},
          "aihubmix": {"models": {"claude-opus-4-8": {"limit":{"context":456}}}}
        }"#;
        let catalog = ModelsDevCatalog::parse(input).unwrap();
        let item = catalog.get("claude-opus-4-8").unwrap();
        assert_eq!(item.context, Some(1_000_000));
        assert_eq!(item.output, Some(128_000));
        assert_eq!(item.knowledge.as_deref(), Some("2026-01"));
    }

    #[test]
    fn models_dev_parser_accepts_live_provider_metadata_and_keyed_model_ids() {
        let input = r#"{
          "anthropic": {
            "id":"anthropic", "env":["ANTHROPIC_API_KEY"], "name":"Anthropic",
            "models": {
              "claude-opus-4-8": {
                "name":"Claude Opus 4.8", "knowledge":"2026-01",
                "release_date":"2026-05-28", "modalities":{"input":["text","image"]},
                "limit":{"context":1000000,"output":128000},
                "cost":{"input":5,"output":25}
              }
            }
          }
        }"#;
        let catalog = ModelsDevCatalog::parse(input).unwrap();
        let item = catalog.get("claude-opus-4-8").unwrap();
        assert_eq!(item.model_id, "claude-opus-4-8");
        assert_eq!(item.release_date.as_deref(), Some("2026-05-28"));
    }

    #[test]
    fn kiro_models_only_supply_discovery_and_context() {
        let response = ListAvailableModelsResponse {
            models: vec![UpstreamModel {
                model_id: "claude-opus-4-8".into(),
                model_name: Some("Opus".into()),
                description: None,
                token_limits: Some(TokenLimits {
                    max_input_tokens: Some(1_000_000),
                }),
            }],
            next_token: None,
            resolved_api_region: None,
            resolved_host: None,
            kiro_version: None,
        };
        let candidates = candidates_from_kiro(&response);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].field, ProfileFieldName::ContextWindowTokens);
    }

    #[test]
    fn failed_preview_validation_does_not_consume_but_success_does() {
        let manager = Arc::new(
            MultiTokenManager::new(Config::default(), Vec::new(), None, None, false).unwrap(),
        );
        let service = ModelProfileSyncService::new(manager);
        let store = ModelProfileStore::new_in_memory();
        store
            .patch(crate::anthropic::model_profile::PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set_with_lock(200_000, false)),
                ..Default::default()
            })
            .unwrap();
        let envelope = service
            .create_preview(
                &store,
                vec![ProfileCandidate::integer(
                    "claude-opus-4-8",
                    ProfileFieldName::ContextWindowTokens,
                    1_000_000,
                    "kiro:list-available-models",
                )],
            )
            .unwrap();
        let change = &envelope.changes[0];
        let valid = ApplyPreviewChange {
            id: change.id.clone(),
            model_id: change.model_id.clone(),
            field: change.field,
            value: change.value.clone(),
            source: change.source.clone(),
            lock: change.lock,
        };
        let mut invalid = valid.clone();
        invalid.value = Value::from(999_999);

        assert!(matches!(
            service.apply_preview(&store, &envelope.preview_id, envelope.revision, &[invalid],),
            Err(SyncError::PreviewMismatch)
        ));
        service
            .apply_preview(
                &store,
                &envelope.preview_id,
                envelope.revision,
                &[valid.clone()],
            )
            .unwrap();
        assert!(matches!(
            service.apply_preview(&store, &envelope.preview_id, envelope.revision, &[valid],),
            Err(SyncError::Preview(PreviewCacheError::Gone))
        ));
    }
}
