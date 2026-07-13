# 模型能力与身份认证回复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 建立按 canonical model 独立持久化的模型资料注册表，支持 Kiro/models.dev 一键获取与同步，并为上下文窗口和知识截止日期探针提供安全的本地确定性回复。

**Architecture:** 线程安全 `ModelProfileStore` 使用 revision/CAS 和原子文件替换管理字段级来源与锁定状态；Admin 同步层只负责收集候选数据，store 负责合并。Anthropic 请求开始时解析一次不可变资料快照，供本地回答和上下文 usage 路径共同使用。

**Tech Stack:** Rust 1.92、Axum、Tokio/Futures、serde/serde_json、reqwest、atomicwrites、Bun/React/TypeScript、TanStack Query。

---

## 文件结构

- Modify: `Cargo.toml` — 增加跨平台原子覆盖依赖 `atomicwrites`。
- Create: `src/anthropic/model_profile.rs` — 数据模型、canonical ID、revision store、字段级合并和持久化。
- Create: `src/anthropic/model_profile_answer.rs` — 有边界的两类认证问题解析与答案格式化。
- Modify: `src/anthropic/mod.rs` — 导出模型资料模块。
- Modify: `src/anthropic/middleware.rs` — AppState 注入资料 store。
- Modify: `src/anthropic/router.rs` — create_router 接收资料 store。
- Modify: `src/anthropic/handlers.rs` — 资料快照、本地回复、context window 消费。
- Modify: `src/anthropic/stream.rs` — StreamContext 使用请求级 context window 快照。
- Modify: `src/anthropic/websearch_loop.rs` — websearch 使用相同 context window 快照。
- Create: `src/admin/model_profile_sync.rs` — Kiro/models.dev 候选收集、超时、缓存和同步摘要。
- Create: `tests/fixtures/models-dev-small.json` — 多 provider 冲突的固定公网目录夹具。
- Modify: `src/admin/mod.rs` — 导出同步模块和共享类型。
- Modify: `src/admin/middleware.rs` — AdminState 注入资料 store 与同步器。
- Modify: `src/admin/types.rs` — profile、preview、apply、sync DTO。
- Modify: `src/admin/service.rs` — Admin profile 操作与认证回复开关持久化。
- Modify: `src/admin/handlers.rs` — profile CRUD/fetch/sync/preview/apply handler。
- Modify: `src/admin/router.rs` — 注册 profile 路由。
- Modify: `src/model/config.rs` — `modelProfileExactAnswersEnabled`。
- Modify: `src/main.rs` — 加载 `model_profiles.json` 并共享注入。
- Create: `admin-ui/src/api/model-profiles.ts` — Admin API 客户端。
- Create: `admin-ui/src/hooks/use-model-profiles.ts` — React Query hooks。
- Create: `admin-ui/src/lib/model-profiles.ts` — 日期、token、差异与锁定校验。
- Create: `admin-ui/src/lib/model-profiles.test.ts` — 前端纯函数测试。
- Create: `admin-ui/src/components/model-profiles-dialog.tsx` — 模型资料管理弹窗。
- Modify: `admin-ui/src/components/topbar-tools.tsx` — 增加入口。
- Modify: `admin-ui/src/types/api.ts` — TypeScript DTO。

### Task 1: 建立 revision/CAS 模型资料 Store

**Files:**
- Modify: `Cargo.toml`
- Create: `src/anthropic/model_profile.rs`
- Modify: `src/anthropic/mod.rs`
- Test: `src/anthropic/model_profile.rs`

- [ ] **Step 1: 写缺文件、回读和 revision RED 测试**

```rust
#[test]
fn store_round_trips_profiles_and_increments_revision() {
    let dir = temp_dir();
    let path = dir.join("model_profiles.json");
    let store = ModelProfileStore::load(&path).unwrap();
    assert_eq!(store.snapshot().revision, 0);

    let updated = store.patch(PatchProfile {
        base_revision: 0,
        model_id: "claude-opus-4-8".into(),
        context_window_tokens: Some(ManualField::set(1_000_000)),
        ..Default::default()
    }).unwrap();
    assert_eq!(updated.revision, 1);

    let reloaded = ModelProfileStore::load(&path).unwrap();
    assert_eq!(reloaded.snapshot().revision, 1);
    assert_eq!(reloaded.resolve("claude-opus-4-8").context_window_tokens, Some(1_000_000));
}

#[test]
fn stale_revision_cannot_overwrite_concurrent_manual_value() {
    let store = ModelProfileStore::new_in_memory();
    store.patch(patch_context(0, "claude-opus-4-8", 1_000_000)).unwrap();
    let error = store.patch(patch_context(0, "claude-opus-4-8", 200_000)).unwrap_err();
    assert!(matches!(error, ModelProfileError::RevisionConflict { expected: 0, actual: 1 }));
}
```

- [ ] **Step 2: 运行 RED 测试**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'; $env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'
cargo test --locked model_profile::tests -- --nocapture
```

Expected: FAIL，因为模块和类型不存在。

- [ ] **Step 3: 增加原子覆盖依赖并定义持久化类型**

`Cargo.toml`：

```toml
atomicwrites = "0.4"
```

核心类型：

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileField<T> {
    pub value: T,
    pub source: String,
    pub locked: bool,
    pub updated_at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredModelProfile {
    pub context_window_tokens: Option<ProfileField<i64>>,
    pub max_output_tokens: Option<ProfileField<i64>>,
    pub knowledge_cutoff: Option<ProfileField<String>>,
    pub release_date: Option<ProfileField<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProfileFile {
    pub version: u32,
    pub revision: u64,
    pub profiles: BTreeMap<String, StoredModelProfile>,
}
```

- [ ] **Step 4: 实现 canonical ID**

`canonical_model_id()` 先 trim/lowercase，再复用 `converter::map_model()` 的已验证 Claude 版本规范化；只去除客户端兼容的 `.thinking`/`-thinking`，不合并 `-fast`、`@default` 或不同 minor 版本。

```rust
pub fn canonical_model_id(input: &str) -> Result<String, ModelProfileError> {
    let mapped = super::converter::map_model(input)
        .ok_or_else(|| ModelProfileError::InvalidModelId(input.to_owned()))?;
    let lower = mapped.to_ascii_lowercase();
    let canonical = if lower.starts_with("claude-") {
        let mut parts = lower.split('.');
        match (parts.next(), parts.next(), parts.next()) {
            (Some(prefix), Some(minor), None) if minor.chars().all(|c| c.is_ascii_digit()) => {
                format!("{prefix}-{minor}")
            }
            _ => lower,
        }
    } else {
        lower
    };
    if canonical.trim().is_empty() { return Err(ModelProfileError::InvalidModelId(input.into())); }
    Ok(canonical)
}
```

增加测试，断言 4.6、4.7、4.8 是三个键，`claude-opus-4.8-thinking` 与基础 4.8 是同一资料，`claude-opus-4-8-fast` 不合并。

- [ ] **Step 5: 实现单写事务和原子落盘**

store 使用 `parking_lot::Mutex<ModelProfileFile>`。每个写操作在锁内检查 revision、clone、验证、通过 `AtomicFile::new(path, AllowOverwrite).write(...)` 落盘，成功后再替换内存并 revision+1；落盘错误时内存不变。

- [ ] **Step 6: 实现字段校验和删除语义**

token 必须 `1..=10_000_000`；日期只接受严格 `YYYY-MM`/`YYYY-MM-DD` 且月份/日期有效。字段清空删除 persisted field；删除模型只删除该 profile，不创建 tombstone。

- [ ] **Step 7: 运行 store 测试**

Run: `cargo test --locked model_profile::tests -- --nocapture`

Expected: 缺文件、重启回读、revision conflict、落盘失败内存不变、日期/token 校验和 canonical ID 测试全部 PASS。

- [ ] **Step 8: 提交 Task 1**

```powershell
git add -- Cargo.toml Cargo.lock src/anthropic/model_profile.rs src/anthropic/mod.rs
git commit -m "feat(model): 增加并发安全的模型资料存储"
```

### Task 2: 实现字段级候选合并、预览和 apply

**Files:**
- Modify: `src/anthropic/model_profile.rs`
- Test: `src/anthropic/model_profile.rs`

- [ ] **Step 1: 写只补空值和来源优先级 RED 测试**

```rust
#[test]
fn sync_fills_only_empty_persisted_fields_using_best_candidate() {
    let store = ModelProfileStore::new_in_memory();
    store.patch(manual_cutoff(0, "claude-opus-4-8", "2026-01")).unwrap();
    let result = store.fill_empty(vec![
        candidate_context("claude-opus-4-8", 200_000, "models.dev:anthropic"),
        candidate_context("claude-opus-4-8", 1_000_000, "kiro:list-available-models"),
        candidate_cutoff("claude-opus-4-8", "2025-12", "models.dev:anthropic"),
    ]).unwrap();
    assert_eq!(result.applied.len(), 1);
    let profile = store.resolve("claude-opus-4-8");
    assert_eq!(profile.context_window_tokens, Some(1_000_000));
    assert_eq!(profile.knowledge_cutoff.as_deref(), Some("2026-01"));
}
```

- [ ] **Step 2: 写 preview/apply CAS RED 测试**

```rust
#[test]
fn preview_apply_rejects_stale_revision_and_locked_field() {
    let store = seeded_store_with_locked_context();
    let preview = store.preview(vec![candidate_context("claude-opus-4-8", 200_000, "models.dev:anthropic")]);
    assert!(preview.changes[0].locked);
    assert!(matches!(store.apply_preview(&preview, &[preview.changes[0].id.clone()]), Err(ModelProfileError::LockedField { .. })));
    store.patch(manual_output(preview.revision, "claude-opus-4-8", 128_000)).unwrap();
    assert!(matches!(store.apply_preview(&preview, &[]), Err(ModelProfileError::RevisionConflict { .. })));
}
```

- [ ] **Step 3: 实现候选类型和稳定来源排序**

```rust
pub enum ProfileFieldName { ContextWindowTokens, MaxOutputTokens, KnowledgeCutoff, ReleaseDate }

pub struct ProfileCandidate {
    pub model_id: String,
    pub field: ProfileFieldName,
    pub value: serde_json::Value,
    pub source: String,
}
```

上下文候选排序：Kiro > models.dev:anthropic > builtin:verified；其他字段：models.dev:anthropic > builtin:verified。任何 persisted 字段都阻止普通 fill-empty 覆盖。

- [ ] **Step 4: 实现 preview 和 apply**

store preview 返回 revision、稳定 change ID、现值、候选值、来源和锁状态；`apply_preview()` 只接受该 preview 内的 change ID，锁定或 revision 变化返回 conflict。apply 使用 Task 1 的同一写事务；API 层的 5 分钟 previewId 由 Task 3 的同步器缓存负责。

- [ ] **Step 5: 运行合并测试并提交**

Run: `cargo test --locked model_profile::tests -- --nocapture`

Expected: 普通同步不覆盖、强制 apply 只改未锁定字段、revision/锁冲突稳定。

```powershell
git add -- src/anthropic/model_profile.rs
git commit -m "feat(model): 增加资料同步预览与原子应用"
```

### Task 3: 实现 Kiro 与 models.dev 同步器

**Files:**
- Create: `src/admin/model_profile_sync.rs`
- Modify: `src/admin/mod.rs`
- Test: `src/admin/model_profile_sync.rs`

- [ ] **Step 1: 写 models.dev provider 选择 RED 测试**

```rust
#[test]
fn models_dev_parser_selects_only_exact_anthropic_entry() {
    let catalog = ModelsDevCatalog::parse(include_str!("../../tests/fixtures/models-dev-small.json")).unwrap();
    let item = catalog.get("claude-opus-4-8").unwrap();
    assert_eq!(item.context, Some(1_000_000));
    assert_eq!(item.output, Some(128_000));
    assert_eq!(item.knowledge.as_deref(), Some("2026-01"));
    assert_eq!(item.source, "models.dev:anthropic");
}
```

fixture 同时包含 `anthropic`、`azure` 和 `aihubmix` 的不同值，确保不会取第一条第三方记录。

- [ ] **Step 2: 写 Kiro 候选转换 RED 测试**

```rust
#[test]
fn kiro_models_only_supply_discovery_and_context() {
    let candidates = candidates_from_kiro(response_with("claude-opus-4-8", 1_000_000));
    assert!(candidates.iter().any(|c| c.field == ProfileFieldName::ContextWindowTokens));
    assert!(!candidates.iter().any(|c| c.field == ProfileFieldName::KnowledgeCutoff));
}
```

- [ ] **Step 3: 实现公网目录解析与 30 分钟缓存**

`ModelsDevClient` 使用 10 秒 reqwest timeout；缓存 `Arc<Mutex<Option<(Instant, ModelsDevCatalog)>>>`。普通同步命中 30 分钟缓存，`force=true` 绕过。JSON 中 provider=`anthropic` 的 exact canonical key 出现重复时返回 warning 而非猜测。

同时增加 `PreviewCache`：

```rust
struct PreviewCache {
    entries: Mutex<HashMap<String, (Instant, ProfilePreview)>>,
    ttl: Duration,
}

impl PreviewCache {
    fn insert(&self, preview: ProfilePreview) -> String;
    fn take_valid(&self, preview_id: &str) -> Result<ProfilePreview, PreviewCacheError>;
}
```

ID 使用 `preview_` 加 UUID；TTL 固定 5 分钟。`take_valid` 对成功读取立即消费，未知或过期返回 Gone，不能重复 apply。

- [ ] **Step 4: 实现健康凭据并发扫描**

从 `token_manager.snapshot()` 选择 `disabled == false` 且当前可调度的凭据 ID，使用：

```rust
stream::iter(ids)
    .map(|id| query_kiro_with_timeout(id))
    .buffer_unordered(4)
    .collect::<Vec<_>>()
    .await
```

每个 Kiro 查询 15 秒超时；单个失败进入 warnings，不清空已有资料。所有来源失败且没有候选时返回 typed `AllSourcesFailed`。

- [ ] **Step 5: 实现 fetch-one 与 sync-all**

`fetch_one(model_id, credential_id, force_public)` 只保留目标 canonical model；`sync_all(force_public)` 取健康凭据模型并集。两者只返回候选和逐来源摘要，不直接写 store。

`preview()` 把 store 生成的 ProfilePreview 放入 PreviewCache 并返回 previewId；`apply(previewId, baseRevision, selected changes)` 先 `take_valid`，逐项核对 model/field/value/source，再调用 store `apply_preview()`。

- [ ] **Step 6: 运行同步器测试**

Run: `cargo test --locked model_profile_sync -- --nocapture`

Expected: provider 选择、缓存、超时、并发上限、部分失败 warning、全部失败错误全部 PASS。

- [ ] **Step 7: 提交 Task 3**

```powershell
git add -- src/admin/model_profile_sync.rs src/admin/mod.rs tests/fixtures/models-dev-small.json
git commit -m "feat(model): 聚合Kiro与公开模型资料"
```

### Task 4: 注入共享 Store 并提供 Admin API

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/main.rs`
- Modify: `src/anthropic/middleware.rs`
- Modify: `src/anthropic/router.rs`
- Modify: `src/admin/middleware.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Test: `src/admin/handlers.rs`

- [ ] **Step 1: 写 API revision 与锁冲突 RED 测试**

覆盖：GET revision、PATCH 成功、过期 PATCH 409、fetch 只补空值、sync 部分成功 200+warnings、apply 锁冲突 409、未知/过期/已消费 previewId 410、DELETE 后 builtin resolved 仍显示。

- [ ] **Step 2: 增加配置开关**

```rust
#[serde(default = "default_true")]
pub model_profile_exact_answers_enabled: bool,
```

默认 `true`，Config 缺字段时兼容旧文件。

- [ ] **Step 3: 在 main 加载并共享注入**

```rust
let model_profiles = Arc::new(
    ModelProfileStore::load(cache_dir.join("model_profiles.json"))
        .unwrap_or_else(|error| {
            tracing::warn!(%error, "模型资料加载失败，使用空持久化资料");
            ModelProfileStore::new_in_memory()
        }),
);
model_profiles.set_exact_answers_enabled(config.model_profile_exact_answers_enabled);
```

同一个 Arc 注入 Anthropic `AppState`、AdminState 和 `ModelProfileSyncService`。

- [ ] **Step 4: 定义并注册全部 API**

```text
GET    /api/admin/model-profiles
PATCH  /api/admin/model-profiles/:modelId
DELETE /api/admin/model-profiles/:modelId
POST   /api/admin/model-profiles/:modelId/fetch
POST   /api/admin/model-profiles/sync
POST   /api/admin/model-profiles/preview
POST   /api/admin/model-profiles/apply
PUT    /api/admin/model-profiles/settings
```

PATCH/DELETE 必须带 `baseRevision`，apply 必须带 `previewId + baseRevision`。revision/lock conflict 返回 409；未知、过期或已消费 previewId 返回 410；字段校验 400；部分来源失败 200+warnings；全部失败 502；落盘失败 500。

- [ ] **Step 5: 实现 settings 持久化顺序**

先加载最新 config、写入 `modelProfileExactAnswersEnabled` 并保存；保存成功后才更新 store 的 AtomicBool。失败时运行时不变。

- [ ] **Step 6: 运行 Admin API 测试**

Run: `cargo test --locked model_profiles_api -- --nocapture`

Expected: 所有状态码、revision 和锁定契约 PASS。

- [ ] **Step 7: 提交 Task 4**

```powershell
git add -- src/model/config.rs src/main.rs src/anthropic/middleware.rs src/anthropic/router.rs src/admin/middleware.rs src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs
git commit -m "feat(admin): 增加模型资料同步与管理API"
```

### Task 5: 实现有边界的本地认证回复

**Files:**
- Create: `src/anthropic/model_profile_answer.rs`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/handlers.rs:928-1160`
- Modify: `src/anthropic/handlers.rs:1590-1660`
- Modify: `src/anthropic/handlers.rs:3110-3180`
- Test: `src/anthropic/model_profile_answer.rs`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写两个目标探针 RED 测试**

```rust
#[test]
fn answers_context_window_as_single_integer() {
    let req = one_text_request("claude-opus-4-8", "What is your maximum context window size in tokens? Reply with just a single integer (no commas, no units, no explanation), e.g. 200000.");
    assert_eq!(exact_model_profile_answer(&req, &profile_4_8()), Some("1000000".into()));
}

#[test]
fn answers_cutoff_as_english_month_and_year() {
    let req = one_text_request("claude-opus-4-8", "What is your knowledge cutoff date? Reply with just the month and year, e.g. 'March 2024'. No additional explanation.");
    assert_eq!(exact_model_profile_answer(&req, &profile_4_8()), Some("January 2026".into()));
}
```

- [ ] **Step 2: 写 fail-closed RED 测试矩阵**

逐一断言返回 None：system 非空、messages>1、两个 text blocks、超过 512 bytes、tools、tool_choice、thinking、image、PDF、output_config、普通开放式上下文问题、附加第三项任务、未知模型、只有 heuristic 来源、开关关闭。

- [ ] **Step 3: 实现固定模板族解析器**

```rust
enum ProfileProbe { ContextWindow, KnowledgeCutoff }

fn classify_profile_probe(req: &MessagesRequest) -> Option<ProfileProbe>;
fn format_cutoff_month(value: &str) -> Option<String>;
```

先执行结构护栏，再对 512 bytes 内的单文本进行 ASCII lowercase/Unicode trim。英文只接受设计规格列出的 subject clause + response clause 组合；中文只接受两条固定模板。不得在整段历史中做 `contains()` 关键词拼接。

- [ ] **Step 4: 只使用可信来源生成答案**

context 输出十进制整数；cutoff 对 `YYYY-MM-DD` 忽略日并转换为英文月份。字段来源必须是 manual、Kiro、models.dev:anthropic 或 builtin:verified；缺失/非法返回 None。

- [ ] **Step 5: 集成标准本地 response builder**

在 exact system/user/ping 之前调用 `try_local_model_profile_response()`，流式使用 `build_local_text_stream_events()`，非流式使用 `build_local_text_message()`；hook outcome 标为 success，usage 只计算当前客户端可见输入，不调用 provider。

- [ ] **Step 6: 运行本地回答测试**

Run:

```powershell
cargo test --locked model_profile_answer -- --nocapture
cargo test --locked local_model_profile -- --nocapture
```

Expected: 目标探针流式/非流式 PASS，所有安全护栏不触发。

- [ ] **Step 7: 提交 Task 5**

```powershell
git add -- src/anthropic/model_profile_answer.rs src/anthropic/mod.rs src/anthropic/handlers.rs
git commit -m "feat(identity): 增加模型资料确定性回复"
```

### Task 6: 让 context usage 使用请求级资料快照

**Files:**
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/websearch_loop.rs`
- Test: `src/anthropic/stream.rs`
- Test: `src/anthropic/handlers.rs`

- [ ] **Step 1: 写运行时资料覆盖 RED 测试**

```rust
#[test]
fn stream_context_uses_request_profile_window_for_context_usage() {
    let mut ctx = StreamContext::new_with_thinking("custom-model", 10, false, HashMap::new(), HashSet::new());
    ctx.set_context_window_size(1_000_000);
    ctx.process_kiro_event(&Event::ContextUsage(ContextUsageEvent { context_usage_percentage: 50.0 }));
    assert_eq!(ctx.upstream_input_tokens(), Some(500_000));
}
```

- [ ] **Step 2: 在 StreamContext 增加快照字段**

```rust
context_window_size: i32,

pub fn set_context_window_size(&mut self, value: i32) {
    self.context_window_size = value.max(1);
}
```

构造器仍用 `get_context_window_size(model)` 作为兼容默认；BufferedStreamContext 提供委托 setter。

- [ ] **Step 3: 在请求开始解析一次 profile**

模型先经过 `state.model_mappings.resolve()`，再 canonicalize，然后 `model_profiles.resolve()`。把 `ResolvedModelProfile` 保存在局部变量，非流、实时流、CC 缓冲流均传入同一个 `context_window_tokens`。

- [ ] **Step 4: 修改三个消费路径**

非流式 ContextUsage、StreamContext 和 websearch loop 都优先使用 snapshot context；资料缺失时使用现有 `get_context_window_size()`。请求处理中管理端更新不得改变当前请求的计算值。

- [ ] **Step 5: 运行 context 与 websearch 测试**

Run:

```powershell
cargo test --locked context_window -- --nocapture
cargo test --locked websearch -- --nocapture
```

Expected: 三条路径一致，内置兜底回归 PASS。

- [ ] **Step 6: 提交 Task 6**

```powershell
git add -- src/anthropic/stream.rs src/anthropic/handlers.rs src/anthropic/websearch_loop.rs
git commit -m "feat(model): 统一请求级上下文窗口资料"
```

### Task 7: 实现模型资料管理端

**Files:**
- Create: `admin-ui/src/api/model-profiles.ts`
- Create: `admin-ui/src/hooks/use-model-profiles.ts`
- Create: `admin-ui/src/lib/model-profiles.ts`
- Create: `admin-ui/src/lib/model-profiles.test.ts`
- Create: `admin-ui/src/components/model-profiles-dialog.tsx`
- Modify: `admin-ui/src/components/topbar-tools.tsx`
- Modify: `admin-ui/src/types/api.ts`

- [ ] **Step 1: 写前端校验和 revision RED 测试**

```ts
test('validates token and cutoff fields', () => {
  expect(validateProfileDraft({ contextWindowTokens: 0, maxOutputTokens: 128000, knowledgeCutoff: '2026-13', releaseDate: '2026-05-28' })).not.toBeNull()
  expect(validateProfileDraft({ contextWindowTokens: 1000000, maxOutputTokens: 128000, knowledgeCutoff: '2026-01', releaseDate: '2026-05-28' })).toBeNull()
})

test('buildApplyRequest carries preview revision and selected exact values', () => {
  const request = buildApplyRequest(previewFixture, ['claude-opus-4-8:contextWindowTokens'])
  expect(request.previewId).toBe(previewFixture.previewId)
  expect(request.baseRevision).toBe(previewFixture.revision)
  expect(request.changes).toEqual([previewFixture.changes[0]])
})
```

Run: `bun test src/lib/model-profiles.test.ts`

Expected: FAIL，因为模块不存在。

- [ ] **Step 2: 定义 TypeScript DTO 与 API**

类型严格镜像后端 camelCase 字段，所有写请求包含 `baseRevision`；409 错误在 hook 中转换为“资料已被其他操作更新，请刷新后重试”。

- [ ] **Step 3: 实现 React Query hooks**

query key 使用 `['model-profiles']`。PATCH、DELETE、fetch、sync、apply、settings 成功后 invalidate；preview 不写缓存。

- [ ] **Step 4: 实现资料表和编辑弹窗**

展示模型 ID、四个字段、来源、锁、更新时间；手填保存默认锁定。清空字段明确提示会回退到 builtin resolved，不表示隐藏模型。

- [ ] **Step 5: 实现一键获取、同步和强制覆盖预览**

获取/同步默认直接只补空值并展示 applied/skipped/warnings；强制覆盖先展示现值与候选值，只有未锁定字段可勾选，apply 原样携带 previewId/revision/value/source。410 时关闭旧预览并提示重新获取。

- [ ] **Step 6: 增加认证回复开关**

弹窗顶部提供“启用模型资料认证回复”，关闭时仍允许资料编辑和同步，并显示“探针将继续走上游”。

- [ ] **Step 7: 运行前端测试和构建**

Run:

```powershell
cd admin-ui
bun test src/lib/model-profiles.test.ts
bun run build
```

Expected: 测试 PASS，TypeScript/Vite 构建成功。

- [ ] **Step 8: 提交 Task 7**

```powershell
git add -- admin-ui/src/api/model-profiles.ts admin-ui/src/hooks/use-model-profiles.ts admin-ui/src/lib/model-profiles.ts admin-ui/src/lib/model-profiles.test.ts admin-ui/src/components/model-profiles-dialog.tsx admin-ui/src/components/topbar-tools.tsx admin-ui/src/types/api.ts
git commit -m "feat(admin): 增加模型资料管理界面"
```

### Task 8: 完成模型资料集成验收

**Files:**
- Modify: `src/bin/anthropic_probe.rs`
- Test: `src/anthropic/model_profile.rs`
- Test: `src/anthropic/model_profile_answer.rs`
- Test: `src/admin/model_profile_sync.rs`

- [ ] **Step 1: 为本地探针增加两个身份 case**

新增 `context-window-profile` 和 `knowledge-cutoff-profile`，分别验证流式、非流式严格输出和 usage；探针不打印 Admin Key 或模型资料文件全文。

- [ ] **Step 2: 运行目标 Rust 回归**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'; $env:CARGO_INCREMENTAL='0'; $env:RUSTFLAGS='-C debuginfo=0'
cargo test --locked model_profile -- --nocapture
cargo test --locked context_window -- --nocapture
cargo test --locked exact_output -- --nocapture
cargo test --locked tool_choice -- --nocapture
```

Expected: 全部 PASS。

- [ ] **Step 3: 运行完整静态验证**

Run:

```powershell
cargo fmt --all -- --check
cargo check --locked
cd admin-ui; bun test; bun run build; cd ..
git diff --check
```

Expected: 全部退出 0。

- [ ] **Step 4: 在 8991 验证获取、同步和身份探针**

对 4.6、4.7、4.8 分别保存不同资料，重启测试容器确认回读；运行两个 Ztest 类探针，确认只返回对应资料且上游请求计数不增加。再运行正常多轮、tools、thinking、PDF 和 websearch，确认不会触发本地资料回答。

- [ ] **Step 5: 提交探针**

```powershell
git add -- src/bin/anthropic_probe.rs
git commit -m "test(probe): 覆盖模型资料身份探针"
```
