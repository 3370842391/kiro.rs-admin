use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use atomicwrites::{AllowOverwrite, AtomicFile};
use chrono::{Datelike, NaiveDate, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

const PROFILE_FILE_VERSION: u32 = 1;
const MAX_PROFILE_TOKENS: i64 = 10_000_000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileField<T> {
    pub value: T,
    pub source: String,
    pub locked: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct StoredModelProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window_tokens: Option<ProfileField<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<ProfileField<i64>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub knowledge_cutoff: Option<ProfileField<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<ProfileField<String>>,
}

impl StoredModelProfile {
    fn is_empty(&self) -> bool {
        self.context_window_tokens.is_none()
            && self.max_output_tokens.is_none()
            && self.knowledge_cutoff.is_none()
            && self.release_date.is_none()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfileFile {
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub profiles: BTreeMap<String, StoredModelProfile>,
}

impl Default for ModelProfileFile {
    fn default() -> Self {
        Self {
            version: PROFILE_FILE_VERSION,
            revision: 0,
            profiles: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualField<T> {
    pub value: Option<T>,
    pub lock: bool,
}

impl<T> ManualField<T> {
    #[cfg(test)]
    pub fn set(value: T) -> Self {
        Self {
            value: Some(value),
            lock: true,
        }
    }

    pub fn set_with_lock(value: T, lock: bool) -> Self {
        Self {
            value: Some(value),
            lock,
        }
    }

    pub fn clear() -> Self {
        Self {
            value: None,
            lock: false,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PatchProfile {
    pub base_revision: u64,
    pub model_id: String,
    pub context_window_tokens: Option<ManualField<i64>>,
    pub max_output_tokens: Option<ManualField<i64>>,
    pub knowledge_cutoff: Option<ManualField<String>>,
    pub release_date: Option<ManualField<String>>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedModelProfile {
    pub model_id: String,
    pub context_window_tokens: Option<i64>,
    pub max_output_tokens: Option<i64>,
    pub knowledge_cutoff: Option<String>,
    pub release_date: Option<String>,
    pub context_window_field: Option<ProfileField<i64>>,
    pub max_output_field: Option<ProfileField<i64>>,
    pub knowledge_cutoff_field: Option<ProfileField<String>>,
    pub release_date_field: Option<ProfileField<String>>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[serde(rename_all = "camelCase")]
pub enum ProfileFieldName {
    ContextWindowTokens,
    MaxOutputTokens,
    KnowledgeCutoff,
    ReleaseDate,
}

impl ProfileFieldName {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ContextWindowTokens => "contextWindowTokens",
            Self::MaxOutputTokens => "maxOutputTokens",
            Self::KnowledgeCutoff => "knowledgeCutoff",
            Self::ReleaseDate => "releaseDate",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileCandidate {
    pub model_id: String,
    pub field: ProfileFieldName,
    pub value: Value,
    pub source: String,
}

impl ProfileCandidate {
    pub fn integer(
        model_id: impl Into<String>,
        field: ProfileFieldName,
        value: i64,
        source: impl Into<String>,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            field,
            value: Value::from(value),
            source: source.into(),
        }
    }

    pub fn string(
        model_id: impl Into<String>,
        field: ProfileFieldName,
        value: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            field,
            value: Value::from(value.into()),
            source: source.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileChangeResult {
    pub model_id: String,
    pub field: ProfileFieldName,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FillEmptyResult {
    pub applied: Vec<ProfileChangeResult>,
    pub skipped: Vec<ProfileChangeResult>,
    pub revision: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePreviewChange {
    pub id: String,
    pub model_id: String,
    pub field: ProfileFieldName,
    pub current_value: Option<Value>,
    pub current_source: Option<String>,
    pub value: Value,
    pub source: String,
    pub locked: bool,
    pub lock: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ProfilePreview {
    pub revision: u64,
    pub changes: Vec<ProfilePreviewChange>,
}

#[derive(Debug, Error)]
pub enum ModelProfileError {
    #[error("模型资料文件读取失败: {0}")]
    Read(String),
    #[error("模型资料文件解析失败: {0}")]
    Parse(String),
    #[error("不支持的模型资料文件版本: {0}")]
    UnsupportedVersion(u32),
    #[error("模型资料写入失败: {0}")]
    Persist(String),
    #[error("无效模型 ID: {0}")]
    InvalidModelId(String),
    #[error("字段 {field} 无效: {message}")]
    InvalidField { field: String, message: String },
    #[error("资料 revision 冲突: expected={expected}, actual={actual}")]
    RevisionConflict { expected: u64, actual: u64 },
    #[error("字段已锁定: {model_id}.{field}")]
    LockedField { model_id: String, field: String },
    #[error("预览变更不存在: {0}")]
    UnknownPreviewChange(String),
}

pub struct ModelProfileStore {
    path: Option<PathBuf>,
    state: Mutex<ModelProfileFile>,
    exact_answers_enabled: AtomicBool,
}

impl ModelProfileStore {
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ModelProfileError> {
        let path = path.as_ref().to_path_buf();
        let state = if path.exists() {
            let bytes =
                std::fs::read(&path).map_err(|error| ModelProfileError::Read(error.to_string()))?;
            let parsed: ModelProfileFile = serde_json::from_slice(&bytes)
                .map_err(|error| ModelProfileError::Parse(error.to_string()))?;
            validate_file(&parsed)?;
            parsed
        } else {
            ModelProfileFile::default()
        };
        Ok(Self {
            path: Some(path),
            state: Mutex::new(state),
            exact_answers_enabled: AtomicBool::new(true),
        })
    }

    #[cfg(test)]
    pub fn new_in_memory() -> Self {
        Self {
            path: None,
            state: Mutex::new(ModelProfileFile::default()),
            exact_answers_enabled: AtomicBool::new(true),
        }
    }

    /// 在保留指定持久化路径的前提下，从空资料启动。
    ///
    /// 仅用于已有文件读取或解析失败时的降级路径；这样后续管理员修复资料时仍会
    /// 原子写回原来的 `model_profiles.json`，而不会悄悄退化成只存在于内存。
    pub fn new_empty_at(path: impl AsRef<Path>) -> Self {
        Self {
            path: Some(path.as_ref().to_path_buf()),
            state: Mutex::new(ModelProfileFile::default()),
            exact_answers_enabled: AtomicBool::new(true),
        }
    }

    pub fn snapshot(&self) -> ModelProfileFile {
        self.state.lock().clone()
    }

    pub fn exact_answers_enabled(&self) -> bool {
        self.exact_answers_enabled.load(Ordering::Relaxed)
    }

    pub fn set_exact_answers_enabled(&self, enabled: bool) {
        self.exact_answers_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn patch(&self, patch: PatchProfile) -> Result<ModelProfileFile, ModelProfileError> {
        let canonical = canonical_model_id(&patch.model_id)?;
        self.transaction(patch.base_revision, |next| {
            let profile = next.profiles.entry(canonical.clone()).or_default();
            apply_manual_integer(
                &mut profile.context_window_tokens,
                patch.context_window_tokens,
                ProfileFieldName::ContextWindowTokens,
            )?;
            apply_manual_integer(
                &mut profile.max_output_tokens,
                patch.max_output_tokens,
                ProfileFieldName::MaxOutputTokens,
            )?;
            apply_manual_string(
                &mut profile.knowledge_cutoff,
                patch.knowledge_cutoff,
                ProfileFieldName::KnowledgeCutoff,
            )?;
            apply_manual_string(
                &mut profile.release_date,
                patch.release_date,
                ProfileFieldName::ReleaseDate,
            )?;
            if profile.is_empty() {
                next.profiles.remove(&canonical);
            }
            Ok(())
        })
    }

    pub fn delete(
        &self,
        base_revision: u64,
        model_id: &str,
    ) -> Result<ModelProfileFile, ModelProfileError> {
        let canonical = canonical_model_id(model_id)?;
        self.transaction(base_revision, |next| {
            next.profiles.remove(&canonical);
            Ok(())
        })
    }

    pub fn resolve(&self, model_id: &str) -> ResolvedModelProfile {
        let canonical =
            canonical_model_id(model_id).unwrap_or_else(|_| model_id.trim().to_ascii_lowercase());
        let persisted = self
            .state
            .lock()
            .profiles
            .get(&canonical)
            .cloned()
            .unwrap_or_default();
        let builtin = builtin_profile(&canonical);
        let context_window_field = persisted
            .context_window_tokens
            .or(builtin.context_window_tokens);
        let max_output_field = persisted.max_output_tokens.or(builtin.max_output_tokens);
        let knowledge_cutoff_field = persisted.knowledge_cutoff.or(builtin.knowledge_cutoff);
        let release_date_field = persisted.release_date.or(builtin.release_date);
        ResolvedModelProfile {
            model_id: canonical,
            context_window_tokens: context_window_field.as_ref().map(|field| field.value),
            max_output_tokens: max_output_field.as_ref().map(|field| field.value),
            knowledge_cutoff: knowledge_cutoff_field
                .as_ref()
                .map(|field| field.value.clone()),
            release_date: release_date_field.as_ref().map(|field| field.value.clone()),
            context_window_field,
            max_output_field,
            knowledge_cutoff_field,
            release_date_field,
        }
    }

    pub fn list_resolved(&self) -> Vec<ResolvedModelProfile> {
        let mut ids: BTreeSet<String> = self.state.lock().profiles.keys().cloned().collect();
        ids.extend(
            [
                "claude-haiku-4-5",
                "claude-sonnet-4-5",
                "claude-opus-4-5",
                "claude-sonnet-4-6",
                "claude-opus-4-6",
                "claude-opus-4-7",
                "claude-sonnet-4-8",
                "claude-opus-4-8",
            ]
            .into_iter()
            .map(ToOwned::to_owned),
        );
        ids.into_iter().map(|id| self.resolve(&id)).collect()
    }

    #[cfg(test)]
    pub fn fill_empty(
        &self,
        candidates: Vec<ProfileCandidate>,
    ) -> Result<FillEmptyResult, ModelProfileError> {
        let revision = self.snapshot().revision;
        self.fill_empty_at(revision, candidates)
    }

    pub fn fill_empty_at(
        &self,
        base_revision: u64,
        candidates: Vec<ProfileCandidate>,
    ) -> Result<FillEmptyResult, ModelProfileError> {
        let best = best_candidates(candidates)?;
        let mut state = self.state.lock();
        if state.revision != base_revision {
            return Err(ModelProfileError::RevisionConflict {
                expected: base_revision,
                actual: state.revision,
            });
        }
        let mut next = state.clone();
        let mut result = FillEmptyResult::default();
        for candidate in best.into_values() {
            let profile = next.profiles.entry(candidate.model_id.clone()).or_default();
            if profile_has_field(profile, candidate.field) {
                result.skipped.push(ProfileChangeResult {
                    model_id: candidate.model_id,
                    field: candidate.field,
                    source: candidate.source,
                    reason: Some("persisted_value_exists".into()),
                });
                continue;
            }
            set_candidate(profile, &candidate, false)?;
            result.applied.push(ProfileChangeResult {
                model_id: candidate.model_id,
                field: candidate.field,
                source: candidate.source,
                reason: None,
            });
        }
        if result.applied.is_empty() {
            result.revision = state.revision;
            return Ok(result);
        }
        next.revision = next.revision.saturating_add(1);
        self.persist(&next)?;
        *state = next;
        result.revision = state.revision;
        Ok(result)
    }

    pub fn preview(
        &self,
        candidates: Vec<ProfileCandidate>,
    ) -> Result<ProfilePreview, ModelProfileError> {
        let best = best_candidates(candidates)?;
        let state = self.state.lock();
        let changes = best
            .into_values()
            .filter_map(|candidate| {
                let profile = state.profiles.get(&candidate.model_id);
                let (current_value, current_source, locked) =
                    current_field(profile, candidate.field);
                if current_value.as_ref() == Some(&candidate.value) {
                    return None;
                }
                Some(ProfilePreviewChange {
                    id: format!("{}:{}", candidate.model_id, candidate.field.as_str()),
                    model_id: candidate.model_id,
                    field: candidate.field,
                    current_value,
                    current_source,
                    value: candidate.value,
                    source: candidate.source,
                    locked,
                    lock: false,
                })
            })
            .collect();
        Ok(ProfilePreview {
            revision: state.revision,
            changes,
        })
    }

    pub fn apply_preview(
        &self,
        preview: &ProfilePreview,
        selected_ids: &[String],
    ) -> Result<ModelProfileFile, ModelProfileError> {
        let selected: BTreeSet<&str> = selected_ids.iter().map(String::as_str).collect();
        self.transaction(preview.revision, |next| {
            for id in &selected {
                if !preview.changes.iter().any(|change| change.id == *id) {
                    return Err(ModelProfileError::UnknownPreviewChange((*id).to_string()));
                }
            }
            for change in preview
                .changes
                .iter()
                .filter(|change| selected.contains(change.id.as_str()))
            {
                let profile = next.profiles.entry(change.model_id.clone()).or_default();
                if profile_field_locked(profile, change.field) {
                    return Err(ModelProfileError::LockedField {
                        model_id: change.model_id.clone(),
                        field: change.field.as_str().to_string(),
                    });
                }
                let candidate = ProfileCandidate {
                    model_id: change.model_id.clone(),
                    field: change.field,
                    value: change.value.clone(),
                    source: change.source.clone(),
                };
                set_candidate(profile, &candidate, change.lock)?;
            }
            Ok(())
        })
    }

    fn transaction(
        &self,
        base_revision: u64,
        mutate: impl FnOnce(&mut ModelProfileFile) -> Result<(), ModelProfileError>,
    ) -> Result<ModelProfileFile, ModelProfileError> {
        let mut state = self.state.lock();
        if state.revision != base_revision {
            return Err(ModelProfileError::RevisionConflict {
                expected: base_revision,
                actual: state.revision,
            });
        }
        let mut next = state.clone();
        mutate(&mut next)?;
        next.revision = next.revision.saturating_add(1);
        self.persist(&next)?;
        *state = next.clone();
        Ok(next)
    }

    fn persist(&self, next: &ModelProfileFile) -> Result<(), ModelProfileError> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| ModelProfileError::Persist(error.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(next)
            .map_err(|error| ModelProfileError::Persist(error.to_string()))?;
        AtomicFile::new(path, AllowOverwrite)
            .write(|file| file.write_all(&bytes))
            .map_err(|error| ModelProfileError::Persist(error.to_string()))
    }
}

pub fn canonical_model_id(input: &str) -> Result<String, ModelProfileError> {
    let mut value = input.trim().to_ascii_lowercase();
    if value.is_empty() {
        return Err(ModelProfileError::InvalidModelId(input.to_string()));
    }
    // 客户端兼容标记不是独立的上游模型资料：thinking 仅改变请求行为，`[1M]`
    // 仅声明上下文窗口。先剥离这些已知后缀，再执行严格字符校验，避免把任意含有
    // "opus 4.8" 的非法字符串交给宽松别名映射器。
    for suffix in [".thinking", "-thinking"] {
        if let Some(stripped) = value.strip_suffix(suffix) {
            value = stripped.to_string();
            break;
        }
    }
    if let Some(stripped) = value.strip_suffix("[1m]") {
        value = stripped.to_string();
    }
    if !value
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '.' | '_' | '@' | ':'))
    {
        return Err(ModelProfileError::InvalidModelId(input.to_string()));
    }
    let parts: Vec<&str> = value.split('-').collect();
    if parts.len() == 3
        && parts[0] == "claude"
        && matches!(parts[1], "opus" | "sonnet" | "haiku" | "fable" | "mythos")
    {
        if let Some((major, minor)) = parts[2].split_once('.')
            && numeric_version(major, minor)
        {
            return Ok(format!("claude-{}-{major}-{minor}", parts[1]));
        }
    }
    if parts.len() >= 4
        && parts[0] == "claude"
        && matches!(parts[1], "opus" | "sonnet" | "haiku" | "fable" | "mythos")
    {
        if let Some((major, minor)) = parts[2].split_once('.') {
            if numeric_version(major, minor) {
                let mut canonical = vec![parts[0], parts[1], major, minor];
                canonical.extend_from_slice(&parts[3..]);
                return Ok(canonical.join("-"));
            }
        }
        if parts.len() >= 4 && numeric_version(parts[2], parts[3]) {
            return Ok(parts.join("-"));
        }
    }
    if let Some(mapped) = super::converter::map_model(&value) {
        if mapped != value && !value.ends_with("-fast") && !value.contains('@') {
            return canonical_model_id(&mapped);
        }
    }
    Ok(value)
}

pub fn is_trusted_profile_source(source: &str) -> bool {
    source == "manual"
        || source == "kiro:list-available-models"
        || source == "models.dev:anthropic"
        || source == "builtin:verified"
}

fn numeric_version(major: &str, minor: &str) -> bool {
    !major.is_empty()
        && !minor.is_empty()
        && major.chars().all(|ch| ch.is_ascii_digit())
        && minor.chars().all(|ch| ch.is_ascii_digit())
}

fn validate_file(file: &ModelProfileFile) -> Result<(), ModelProfileError> {
    if file.version != PROFILE_FILE_VERSION {
        return Err(ModelProfileError::UnsupportedVersion(file.version));
    }
    for (model_id, profile) in &file.profiles {
        if canonical_model_id(model_id)? != *model_id {
            return Err(ModelProfileError::InvalidModelId(model_id.clone()));
        }
        validate_stored_profile(profile)?;
    }
    Ok(())
}

fn validate_stored_profile(profile: &StoredModelProfile) -> Result<(), ModelProfileError> {
    if let Some(field) = &profile.context_window_tokens {
        validate_integer(ProfileFieldName::ContextWindowTokens, field.value)?;
    }
    if let Some(field) = &profile.max_output_tokens {
        validate_integer(ProfileFieldName::MaxOutputTokens, field.value)?;
    }
    if let Some(field) = &profile.knowledge_cutoff {
        validate_date(ProfileFieldName::KnowledgeCutoff, &field.value)?;
    }
    if let Some(field) = &profile.release_date {
        validate_date(ProfileFieldName::ReleaseDate, &field.value)?;
    }
    Ok(())
}

fn validate_integer(field: ProfileFieldName, value: i64) -> Result<i64, ModelProfileError> {
    if !(1..=MAX_PROFILE_TOKENS).contains(&value) {
        return Err(ModelProfileError::InvalidField {
            field: field.as_str().into(),
            message: format!("必须在 1..={MAX_PROFILE_TOKENS} 范围内"),
        });
    }
    Ok(value)
}

fn validate_date(field: ProfileFieldName, value: &str) -> Result<String, ModelProfileError> {
    let normalized = value.trim();
    let valid = if normalized.len() == 7 {
        NaiveDate::parse_from_str(&format!("{normalized}-01"), "%Y-%m-%d").is_ok()
    } else if normalized.len() == 10 {
        NaiveDate::parse_from_str(normalized, "%Y-%m-%d").is_ok()
    } else {
        false
    };
    if !valid {
        return Err(ModelProfileError::InvalidField {
            field: field.as_str().into(),
            message: "必须是有效的 YYYY-MM 或 YYYY-MM-DD".into(),
        });
    }
    Ok(normalized.to_string())
}

fn apply_manual_integer(
    target: &mut Option<ProfileField<i64>>,
    patch: Option<ManualField<i64>>,
    field: ProfileFieldName,
) -> Result<(), ModelProfileError> {
    let Some(patch) = patch else {
        return Ok(());
    };
    *target = match patch.value {
        Some(value) => Some(ProfileField {
            value: validate_integer(field, value)?,
            source: "manual".into(),
            locked: patch.lock,
            updated_at: Utc::now().to_rfc3339(),
        }),
        None => None,
    };
    Ok(())
}

fn apply_manual_string(
    target: &mut Option<ProfileField<String>>,
    patch: Option<ManualField<String>>,
    field: ProfileFieldName,
) -> Result<(), ModelProfileError> {
    let Some(patch) = patch else {
        return Ok(());
    };
    *target = match patch.value {
        Some(value) => Some(ProfileField {
            value: validate_date(field, &value)?,
            source: "manual".into(),
            locked: patch.lock,
            updated_at: Utc::now().to_rfc3339(),
        }),
        None => None,
    };
    Ok(())
}

fn source_priority(field: ProfileFieldName, source: &str) -> Option<u8> {
    match (field, source) {
        (ProfileFieldName::ContextWindowTokens, "kiro:list-available-models") => Some(3),
        (_, "models.dev:anthropic") => Some(2),
        (_, "builtin:verified") => Some(1),
        _ => None,
    }
}

fn best_candidates(
    candidates: Vec<ProfileCandidate>,
) -> Result<BTreeMap<(String, ProfileFieldName), ProfileCandidate>, ModelProfileError> {
    let mut best = BTreeMap::new();
    for mut candidate in candidates {
        candidate.model_id = canonical_model_id(&candidate.model_id)?;
        validate_candidate(&candidate)?;
        let priority = source_priority(candidate.field, &candidate.source).ok_or_else(|| {
            ModelProfileError::InvalidField {
                field: candidate.field.as_str().into(),
                message: format!("不允许的来源 {}", candidate.source),
            }
        })?;
        let key = (candidate.model_id.clone(), candidate.field);
        let replace = best.get(&key).is_none_or(|current: &ProfileCandidate| {
            let current_priority =
                source_priority(current.field, &current.source).unwrap_or_default();
            priority > current_priority
                || (priority == current_priority
                    && candidate.value.to_string() < current.value.to_string())
        });
        if replace {
            best.insert(key, candidate);
        }
    }
    Ok(best)
}

fn validate_candidate(candidate: &ProfileCandidate) -> Result<(), ModelProfileError> {
    match candidate.field {
        ProfileFieldName::ContextWindowTokens | ProfileFieldName::MaxOutputTokens => {
            let value =
                candidate
                    .value
                    .as_i64()
                    .ok_or_else(|| ModelProfileError::InvalidField {
                        field: candidate.field.as_str().into(),
                        message: "必须是整数".into(),
                    })?;
            validate_integer(candidate.field, value)?;
        }
        ProfileFieldName::KnowledgeCutoff | ProfileFieldName::ReleaseDate => {
            let value =
                candidate
                    .value
                    .as_str()
                    .ok_or_else(|| ModelProfileError::InvalidField {
                        field: candidate.field.as_str().into(),
                        message: "必须是日期字符串".into(),
                    })?;
            validate_date(candidate.field, value)?;
        }
    }
    Ok(())
}

fn profile_has_field(profile: &StoredModelProfile, field: ProfileFieldName) -> bool {
    match field {
        ProfileFieldName::ContextWindowTokens => profile.context_window_tokens.is_some(),
        ProfileFieldName::MaxOutputTokens => profile.max_output_tokens.is_some(),
        ProfileFieldName::KnowledgeCutoff => profile.knowledge_cutoff.is_some(),
        ProfileFieldName::ReleaseDate => profile.release_date.is_some(),
    }
}

fn profile_field_locked(profile: &StoredModelProfile, field: ProfileFieldName) -> bool {
    match field {
        ProfileFieldName::ContextWindowTokens => profile
            .context_window_tokens
            .as_ref()
            .is_some_and(|value| value.locked),
        ProfileFieldName::MaxOutputTokens => profile
            .max_output_tokens
            .as_ref()
            .is_some_and(|value| value.locked),
        ProfileFieldName::KnowledgeCutoff => profile
            .knowledge_cutoff
            .as_ref()
            .is_some_and(|value| value.locked),
        ProfileFieldName::ReleaseDate => profile
            .release_date
            .as_ref()
            .is_some_and(|value| value.locked),
    }
}

fn current_field(
    profile: Option<&StoredModelProfile>,
    field: ProfileFieldName,
) -> (Option<Value>, Option<String>, bool) {
    let Some(profile) = profile else {
        return (None, None, false);
    };
    match field {
        ProfileFieldName::ContextWindowTokens => profile
            .context_window_tokens
            .as_ref()
            .map(|field| {
                (
                    Some(Value::from(field.value)),
                    Some(field.source.clone()),
                    field.locked,
                )
            })
            .unwrap_or((None, None, false)),
        ProfileFieldName::MaxOutputTokens => profile
            .max_output_tokens
            .as_ref()
            .map(|field| {
                (
                    Some(Value::from(field.value)),
                    Some(field.source.clone()),
                    field.locked,
                )
            })
            .unwrap_or((None, None, false)),
        ProfileFieldName::KnowledgeCutoff => profile
            .knowledge_cutoff
            .as_ref()
            .map(|field| {
                (
                    Some(Value::from(field.value.clone())),
                    Some(field.source.clone()),
                    field.locked,
                )
            })
            .unwrap_or((None, None, false)),
        ProfileFieldName::ReleaseDate => profile
            .release_date
            .as_ref()
            .map(|field| {
                (
                    Some(Value::from(field.value.clone())),
                    Some(field.source.clone()),
                    field.locked,
                )
            })
            .unwrap_or((None, None, false)),
    }
}

fn set_candidate(
    profile: &mut StoredModelProfile,
    candidate: &ProfileCandidate,
    locked: bool,
) -> Result<(), ModelProfileError> {
    validate_candidate(candidate)?;
    let updated_at = Utc::now().to_rfc3339();
    match candidate.field {
        ProfileFieldName::ContextWindowTokens => {
            profile.context_window_tokens = Some(ProfileField {
                value: candidate.value.as_i64().unwrap(),
                source: candidate.source.clone(),
                locked,
                updated_at,
            });
        }
        ProfileFieldName::MaxOutputTokens => {
            profile.max_output_tokens = Some(ProfileField {
                value: candidate.value.as_i64().unwrap(),
                source: candidate.source.clone(),
                locked,
                updated_at,
            });
        }
        ProfileFieldName::KnowledgeCutoff => {
            let value = validate_date(
                candidate.field,
                candidate
                    .value
                    .as_str()
                    .expect("validated string candidate"),
            )?;
            profile.knowledge_cutoff = Some(ProfileField {
                value,
                source: candidate.source.clone(),
                locked,
                updated_at,
            });
        }
        ProfileFieldName::ReleaseDate => {
            let value = validate_date(
                candidate.field,
                candidate
                    .value
                    .as_str()
                    .expect("validated string candidate"),
            )?;
            profile.release_date = Some(ProfileField {
                value,
                source: candidate.source.clone(),
                locked,
                updated_at,
            });
        }
    }
    Ok(())
}

fn builtin_profile(model_id: &str) -> StoredModelProfile {
    let context = match model_id {
        "claude-opus-4-6" | "claude-opus-4-7" | "claude-opus-4-8" | "claude-sonnet-4-6"
        | "claude-sonnet-4-8" => Some(1_000_000),
        "claude-opus-4-5" | "claude-sonnet-4-5" | "claude-haiku-4-5" => Some(200_000),
        _ => None,
    };
    StoredModelProfile {
        context_window_tokens: context.map(|value| ProfileField {
            value,
            source: "builtin:verified".into(),
            locked: false,
            updated_at: "2026-07-13T00:00:00Z".into(),
        }),
        ..Default::default()
    }
}

pub fn cutoff_month_year(value: &str) -> Option<String> {
    let normalized = if value.len() == 7 {
        format!("{value}-01")
    } else {
        value.to_string()
    };
    let date = NaiveDate::parse_from_str(&normalized, "%Y-%m-%d").ok()?;
    let month = [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ][date.month0() as usize];
    Some(format!("{month} {}", date.year()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_dir() -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("kiro-model-profile-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn store_round_trips_profiles_and_increments_revision() {
        let dir = test_dir();
        let path = dir.join("model_profiles.json");
        let store = ModelProfileStore::load(&path).unwrap();
        assert_eq!(store.snapshot().revision, 0);

        let updated = store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(1_000_000)),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(updated.revision, 1);

        let reloaded = ModelProfileStore::load(&path).unwrap();
        assert_eq!(reloaded.snapshot().revision, 1);
        assert_eq!(
            reloaded.resolve("claude-opus-4-8").context_window_tokens,
            Some(1_000_000)
        );
    }

    #[test]
    fn corrupt_file_fallback_keeps_persistence_path_for_next_write() {
        let dir = test_dir();
        let path = dir.join("model_profiles.json");
        std::fs::write(&path, b"{not-json").unwrap();

        assert!(matches!(
            ModelProfileStore::load(&path),
            Err(ModelProfileError::Parse(_))
        ));
        let store = ModelProfileStore::new_empty_at(&path);
        store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(1_000_000)),
                ..Default::default()
            })
            .unwrap();

        let reloaded = ModelProfileStore::load(&path).unwrap();
        assert_eq!(reloaded.snapshot().revision, 1);
        assert_eq!(
            reloaded.resolve("claude-opus-4-8").context_window_tokens,
            Some(1_000_000)
        );
    }

    #[test]
    fn stale_revision_cannot_overwrite_concurrent_manual_value() {
        let store = ModelProfileStore::new_in_memory();
        store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(1_000_000)),
                ..Default::default()
            })
            .unwrap();
        let error = store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(200_000)),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(
            error,
            ModelProfileError::RevisionConflict {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[test]
    fn canonical_ids_keep_model_versions_and_variants_distinct() {
        assert_eq!(
            canonical_model_id("claude-opus-4.6").unwrap(),
            "claude-opus-4-6"
        );
        assert_eq!(
            canonical_model_id("claude-opus-4-7").unwrap(),
            "claude-opus-4-7"
        );
        assert_eq!(
            canonical_model_id("claude-opus-4.8-thinking").unwrap(),
            "claude-opus-4-8"
        );
        assert_eq!(
            canonical_model_id("claude-opus-4-8[1M]").unwrap(),
            "claude-opus-4-8"
        );
        assert_eq!(
            canonical_model_id("claude-opus-4-8-fast").unwrap(),
            "claude-opus-4-8-fast"
        );
        assert_eq!(
            canonical_model_id("claude-opus-4-8@default").unwrap(),
            "claude-opus-4-8@default"
        );
    }

    #[test]
    fn sync_fills_only_empty_fields_using_best_candidate() {
        let store = ModelProfileStore::new_in_memory();
        store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                knowledge_cutoff: Some(ManualField::set("2026-01".into())),
                ..Default::default()
            })
            .unwrap();

        let result = store
            .fill_empty(vec![
                ProfileCandidate::integer(
                    "claude-opus-4-8",
                    ProfileFieldName::ContextWindowTokens,
                    200_000,
                    "models.dev:anthropic",
                ),
                ProfileCandidate::integer(
                    "claude-opus-4-8",
                    ProfileFieldName::ContextWindowTokens,
                    1_000_000,
                    "kiro:list-available-models",
                ),
                ProfileCandidate::string(
                    "claude-opus-4-8",
                    ProfileFieldName::KnowledgeCutoff,
                    "2025-12",
                    "models.dev:anthropic",
                ),
            ])
            .unwrap();
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.skipped.len(), 1);
        let profile = store.resolve("claude-opus-4-8");
        assert_eq!(profile.context_window_tokens, Some(1_000_000));
        assert_eq!(profile.knowledge_cutoff.as_deref(), Some("2026-01"));
    }

    #[test]
    fn sync_normalizes_date_values_before_persisting() {
        let store = ModelProfileStore::new_in_memory();
        store
            .fill_empty(vec![ProfileCandidate::string(
                "claude-opus-4-8",
                ProfileFieldName::KnowledgeCutoff,
                " 2026-01 ",
                "models.dev:anthropic",
            )])
            .unwrap();

        let profile = store.resolve("claude-opus-4-8");
        assert_eq!(profile.knowledge_cutoff.as_deref(), Some("2026-01"));
    }

    #[test]
    fn preview_apply_rejects_locked_and_stale_changes() {
        let store = ModelProfileStore::new_in_memory();
        store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(1_000_000)),
                ..Default::default()
            })
            .unwrap();
        let preview = store
            .preview(vec![ProfileCandidate::integer(
                "claude-opus-4-8",
                ProfileFieldName::ContextWindowTokens,
                200_000,
                "models.dev:anthropic",
            )])
            .unwrap();
        assert!(preview.changes[0].locked);
        assert!(matches!(
            store.apply_preview(&preview, &[preview.changes[0].id.clone()]),
            Err(ModelProfileError::LockedField { .. })
        ));

        store
            .patch(PatchProfile {
                base_revision: preview.revision,
                model_id: "claude-opus-4-8".into(),
                max_output_tokens: Some(ManualField::set(128_000)),
                ..Default::default()
            })
            .unwrap();
        assert!(matches!(
            store.apply_preview(&preview, &[]),
            Err(ModelProfileError::RevisionConflict { .. })
        ));
    }

    #[test]
    fn invalid_tokens_and_dates_are_rejected_without_revision_change() {
        let store = ModelProfileStore::new_in_memory();
        let error = store
            .patch(PatchProfile {
                base_revision: 0,
                model_id: "claude-opus-4-8".into(),
                context_window_tokens: Some(ManualField::set(0)),
                knowledge_cutoff: Some(ManualField::set("2026-13".into())),
                ..Default::default()
            })
            .unwrap_err();
        assert!(matches!(error, ModelProfileError::InvalidField { .. }));
        assert_eq!(store.snapshot().revision, 0);
    }
}
