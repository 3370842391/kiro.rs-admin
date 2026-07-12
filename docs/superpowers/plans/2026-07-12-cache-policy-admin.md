# Cache Policy Admin Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (- [ ]) syntax for tracking.

**Goal:** 把 rs 的模拟 prompt cache 默认 TTL 改为 30 分钟，并在管理端提供 5m/30m/1h、启用状态、自动缓存、容量、落盘周期、命中率整形、状态查看和清空缓存的运行时控制。

**Architecture:** Config 保存持久化默认值，CacheMeter 持有可热更新的 CachePolicy 并负责 TTL 解析、LRU、落盘和状态；AdminService 组合 CacheMeter 与 MultiTokenManager 的命中率区间，通过统一 /config/cache-policy API 原子更新。前端把旧命中率弹窗升级为完整缓存策略弹窗，旧 /config/cache-hit-rate API 保留兼容。

**Tech Stack:** Rust 2024、Axum、Tokio、Serde、parking_lot、React 19、TypeScript、TanStack Query、Bun/Vite。

---

## File map

- Modify src/model/config.rs: 新增缓存策略持久化字段、默认值与 serde 测试。
- Modify src/anthropic/cache_metering.rs: 新增 CachePolicy、CacheStats、30m TTL、运行时更新、动态容量、clear 和可靠落盘。
- Modify src/kiro/token_manager.rs: 拆分命中率区间校验、运行时应用和持久化。
- Modify src/admin/types.rs: 新增 cache-policy API 请求/响应类型。
- Modify src/admin/service.rs: 注入 SharedCacheMeter，实现读取、更新、清空及配置持久化。
- Modify src/admin/handlers.rs: 新增三个 cache-policy handler。
- Modify src/admin/router.rs: 注册 cache-policy 路由并保留旧路由。
- Modify src/main.rs: 按 Config 构造 CacheMeter，并将同一实例注入 AdminService。
- Modify admin-ui/src/api/credentials.ts: 新增缓存策略 API 类型和函数。
- Modify admin-ui/src/hooks/use-credentials.ts: 新增 query/mutation hooks。
- Create admin-ui/src/lib/cache-policy.ts: TTL 选项、草稿校验和展示辅助函数。
- Create admin-ui/src/lib/cache-policy.test.ts: Bun 单元测试。
- Create admin-ui/src/components/cache-policy-dialog.tsx: 完整缓存策略 UI。
- Modify admin-ui/src/components/topbar-tools.tsx: 替换旧弹窗和菜单名称。
- Delete admin-ui/src/components/cache-hit-rate-dialog.tsx: 被统一弹窗取代。
- Modify README.md: 记录新配置字段、API 和计费边界。

### Task 1: Persisted cache policy configuration

**Files:**
- Modify: src/model/config.rs

- [ ] **Step 1: Write failing config tests**

在 config.rs 测试模块增加：

~~~rust
#[test]
fn cache_policy_defaults_to_thirty_minutes() {
    let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(config.cache_metering_enabled);
    assert_eq!(config.cache_default_ttl_secs, 1800);
    assert!(config.cache_auto_without_control);
    assert_eq!(config.cache_capacity, 4096);
    assert_eq!(config.cache_flush_interval_secs, 60);
}

#[test]
fn cache_policy_fields_round_trip_in_camel_case() {
    let value = serde_json::json!({
        "cacheMeteringEnabled": false,
        "cacheDefaultTtlSecs": 300,
        "cacheAutoWithoutControl": false,
        "cacheCapacity": 8192,
        "cacheFlushIntervalSecs": 30
    });
    let config: Config = serde_json::from_value(value).unwrap();
    let encoded = serde_json::to_value(config).unwrap();
    assert_eq!(encoded["cacheMeteringEnabled"], false);
    assert_eq!(encoded["cacheDefaultTtlSecs"], 300);
    assert_eq!(encoded["cacheAutoWithoutControl"], false);
    assert_eq!(encoded["cacheCapacity"], 8192);
    assert_eq!(encoded["cacheFlushIntervalSecs"], 30);
}
~~~

- [ ] **Step 2: Run RED**

~~~powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
cargo test cache_policy_ --all-features -j 1 -- --nocapture
~~~

Expected: 编译失败，Config 尚无五个缓存策略字段。

- [ ] **Step 3: Add fields and defaults**

在命中率字段之前加入：

~~~rust
#[serde(default = "default_cache_metering_enabled")]
pub cache_metering_enabled: bool,
#[serde(default = "default_cache_default_ttl_secs")]
pub cache_default_ttl_secs: u64,
#[serde(default = "default_cache_auto_without_control")]
pub cache_auto_without_control: bool,
#[serde(default = "default_cache_capacity")]
pub cache_capacity: usize,
#[serde(default = "default_cache_flush_interval_secs")]
pub cache_flush_interval_secs: u64,
~~~

加入：

~~~rust
fn default_cache_metering_enabled() -> bool { true }
fn default_cache_default_ttl_secs() -> u64 { 30 * 60 }
fn default_cache_auto_without_control() -> bool { true }
fn default_cache_capacity() -> usize { 4096 }
fn default_cache_flush_interval_secs() -> u64 { 60 }
~~~

Config::default() 填入同一组值，不修改已有 cache_hit_rate_min_pct/max_pct。

- [ ] **Step 4: Run GREEN and commit**

~~~powershell
cargo test cache_policy_ --all-features -j 1 --quiet
cargo test model::config --all-features -j 1 --quiet
git add -- src/model/config.rs
git diff --cached --check
git commit -m "feat(cache): 增加缓存策略配置字段"
~~~

### Task 2: Runtime CachePolicy, TTL semantics, stats and clear

**Files:**
- Modify: src/anthropic/cache_metering.rs

- [ ] **Step 1: Write TTL precedence RED tests**

增加 request_with_ttl 和 request_with_system_ttls 测试辅助函数，并写入：

~~~rust
#[test]
fn cache_policy_supports_five_thirty_and_sixty_minutes() {
    assert_eq!(parse_explicit_ttl("5m"), Some(300));
    assert_eq!(parse_explicit_ttl("30m"), Some(1800));
    assert_eq!(parse_explicit_ttl("1h"), Some(3600));
    assert_eq!(parse_explicit_ttl("garbage"), None);
}

#[test]
fn explicit_five_minutes_overrides_default_thirty_minutes() {
    assert_eq!(
        effective_ttl(&request_with_ttl(Some("5m")), CachePolicy::default()),
        300
    );
}

#[test]
fn missing_ttl_uses_default_thirty_minutes() {
    assert_eq!(
        effective_ttl(&request_with_ttl(None), CachePolicy::default()),
        1800
    );
}

#[test]
fn multiple_explicit_ttls_use_largest_explicit_value() {
    let req = request_with_system_ttls(&["5m", "1h"]);
    assert_eq!(effective_ttl(&req, CachePolicy::default()), 3600);
}
~~~

- [ ] **Step 2: Run TTL RED**

~~~powershell
cargo test cache_policy_supports --all-features -j 1 -- --nocapture
cargo test explicit_five --all-features -j 1 -- --nocapture
cargo test missing_ttl --all-features -j 1 -- --nocapture
cargo test multiple_explicit --all-features -j 1 -- --nocapture
~~~

Expected: 缺少 CachePolicy、parse_explicit_ttl 和 effective_ttl。

- [ ] **Step 3: Implement CachePolicy and TTL resolution**

~~~rust
pub const ALLOWED_TTL_SECS: [u64; 3] = [300, 1800, 3600];

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachePolicy {
    pub enabled: bool,
    pub default_ttl_secs: u64,
    pub auto_without_cache_control: bool,
    pub capacity: usize,
    pub flush_interval_secs: u64,
}

impl Default for CachePolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            default_ttl_secs: 1800,
            auto_without_cache_control: true,
            capacity: 4096,
            flush_interval_secs: 60,
        }
    }
}

impl CachePolicy {
    pub fn validate(self) -> anyhow::Result<Self> {
        if !ALLOWED_TTL_SECS.contains(&self.default_ttl_secs) {
            anyhow::bail!("cacheDefaultTtlSecs 只能是 300、1800 或 3600");
        }
        if !(256..=65_536).contains(&self.capacity) {
            anyhow::bail!("cacheCapacity 必须在 256..=65536 内");
        }
        if !(10..=600).contains(&self.flush_interval_secs) {
            anyhow::bail!("cacheFlushIntervalSecs 必须在 10..=600 内");
        }
        Ok(self)
    }
}

pub fn parse_explicit_ttl(value: &str) -> Option<u64> {
    if value.eq_ignore_ascii_case("5m") { return Some(300); }
    if value.eq_ignore_ascii_case("30m") { return Some(1800); }
    if value.eq_ignore_ascii_case("1h") { return Some(3600); }
    None
}
~~~

实现 request_has_cache_control、explicit_ttls、effective_ttl。存在受支持显式 TTL 时取最大显式值；否则使用 policy.default_ttl_secs。删除旧 detect_max_ttl 中“默认 30m 与显式 5m 取 max”的错误可能。

- [ ] **Step 4: Write enabled and auto-cache RED tests**

~~~rust
#[test]
fn disabled_policy_does_not_read_or_write_cache() {
    let cache = CacheMeter::with_policy(None, CachePolicy {
        enabled: false,
        ..CachePolicy::default()
    });
    let req = build_request_with_system_breakpoint();
    let first = compute_cache_usage(&cache, &req, 1);
    let second = compute_cache_usage(&cache, &req, 1);
    assert_eq!(first.cache_covered_est, 0);
    assert_eq!(second.cache_read, 0);
    assert_eq!(cache.stats().active_entries, 0);
}

#[test]
fn auto_without_control_can_be_disabled() {
    let cache = CacheMeter::with_policy(None, CachePolicy {
        auto_without_cache_control: false,
        ..CachePolicy::default()
    });
    let req = req_with_messages(vec![
        msg_with_cc("user", "first", false),
        msg_with_cc("assistant", "second", false),
        msg_with_cc("user", "third", false),
    ]);
    assert_eq!(compute_cache_usage(&cache, &req, 1).cache_covered_est, 0);
}
~~~

- [ ] **Step 5: Run usage RED**

~~~powershell
cargo test disabled_policy --all-features -j 1 -- --nocapture
cargo test auto_without_control --all-features -j 1 -- --nocapture
~~~

Expected: 缺少 with_policy/stats，或当前实现仍写缓存。

- [ ] **Step 6: Add runtime policy fields**

~~~rust
pub struct CacheMeter {
    inner: Mutex<Inner>,
    policy: parking_lot::RwLock<CachePolicy>,
    policy_changed: tokio::sync::Notify,
    persist_path: Option<PathBuf>,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<u64, CacheEntry>,
    dirty: bool,
    generation: u64,
    last_flush_at: Option<i64>,
}
~~~

保留兼容构造器并新增运行时 API：

~~~rust
pub fn new(path: Option<PathBuf>) -> Self {
    Self::with_policy(path, CachePolicy::default())
}

pub fn with_policy(path: Option<PathBuf>, policy: CachePolicy) -> Self;
pub fn policy(&self) -> CachePolicy;
pub fn update_policy(&self, policy: CachePolicy) -> anyhow::Result<CachePolicy>;
~~~

compute_cache_usage 首先读取 policy：disabled 直接返回零覆盖；auto 关闭且无 cache_control 时返回零覆盖；record 使用 effective TTL 和运行时 capacity。

- [ ] **Step 7: Write stats, capacity and clear RED tests**

~~~rust
#[test]
fn lowering_capacity_evicts_lru_immediately() {
    let cache = CacheMeter::new(None);
    let hashes: Vec<u64> = (0..257).collect();
    let tokens = vec![10; hashes.len()];
    cache.record(&hashes, &tokens, 1800);
    cache.update_policy(CachePolicy {
        capacity: 256,
        ..CachePolicy::default()
    }).unwrap();
    assert_eq!(cache.stats().active_entries, 256);
}

#[test]
fn clear_removes_memory_and_persisted_entries() {
    let path = std::env::temp_dir()
        .join(format!("kiro-cache-clear-{}.json", uuid::Uuid::new_v4()));
    let cache = CacheMeter::new(Some(path.clone()));
    cache.record(&[7], &[42], 1800);
    assert_eq!(cache.clear().unwrap(), 1);
    assert_eq!(cache.stats().active_entries, 0);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
    let _ = std::fs::remove_file(path);
}
~~~

- [ ] **Step 8: Run stats RED**

~~~powershell
cargo test lowering_capacity --all-features -j 1 -- --nocapture
cargo test clear_removes --all-features -j 1 -- --nocapture
~~~

Expected: 缺少 CacheStats/clear，容量仍使用静态常量。

- [ ] **Step 9: Implement stats and reliable persistence**

~~~rust
#[derive(Debug, Clone, Copy, Serialize)]
pub struct CacheStats {
    pub active_entries: usize,
    pub capacity: usize,
    pub dirty: bool,
    pub last_flush_at: Option<i64>,
    pub persist_enabled: bool,
}
~~~

record、evict_expired、clear 修改 entries 时递增 generation。flush_to_disk 返回 anyhow::Result；取得 snapshot+generation，写成功后仅当 generation 未变化才清 dirty，失败保持 dirty。clear 清内存并立即 flush。容量检查读取 policy.capacity。

- [ ] **Step 10: Make background interval runtime-aware**

~~~rust
pub fn spawn_background(self: Arc<Self>) {
    tokio::spawn(async move {
        loop {
            let delay = std::time::Duration::from_secs(
                self.policy().flush_interval_secs
            );
            tokio::select! {
                _ = tokio::time::sleep(delay) => {
                    self.evict_expired();
                    if let Err(error) = self.flush_to_disk() {
                        tracing::warn!(%error, "CacheMeter 后台落盘失败");
                    }
                }
                _ = self.policy_changed.notified() => continue,
            }
        }
    });
}
~~~

update_policy 成功后 notify_one。TTL 修改不重写旧 entry expiry；下一次 record 才按新 TTL 续期。

- [ ] **Step 11: Run GREEN and commit**

~~~powershell
cargo test anthropic::cache_metering::tests --all-features -j 1 --quiet
cargo test anthropic::usage::tests --all-features -j 1 --quiet
git add -- src/anthropic/cache_metering.rs
git diff --cached --check
git commit -m "feat(cache): 支持运行时缓存策略"
~~~

### Task 3: Unified Admin cache-policy API and runtime wiring

**Files:**
- Modify: src/kiro/token_manager.rs
- Modify: src/admin/types.rs
- Modify: src/admin/service.rs
- Modify: src/admin/handlers.rs
- Modify: src/admin/router.rs
- Modify: src/main.rs

- [ ] **Step 1: Write Admin contract RED tests**

在 admin/types.rs 增加：

~~~rust
#[test]
fn cache_policy_patch_is_partial_and_camel_case() {
    let patch: SetCachePolicyRequest = serde_json::from_value(
        serde_json::json!({
            "defaultTtlSecs": 300,
            "capacity": 8192
        })
    ).unwrap();
    assert_eq!(patch.default_ttl_secs, Some(300));
    assert_eq!(patch.capacity, Some(8192));
    assert_eq!(patch.enabled, None);
}
~~~

在 token_manager.rs 增加：

~~~rust
#[test]
fn cache_hit_rate_validation_is_reusable_without_mutation() {
    assert!(MultiTokenManager::validate_cache_hit_rate_bounds(0, 95).is_ok());
    assert!(MultiTokenManager::validate_cache_hit_rate_bounds(99, 90).is_err());
}
~~~

- [ ] **Step 2: Run RED**

~~~powershell
cargo test cache_policy_patch --all-features -j 1 -- --nocapture
cargo test cache_hit_rate_validation --all-features -j 1 -- --nocapture
~~~

Expected: 缺少 API 类型和可复用验证函数。

- [ ] **Step 3: Add Admin types**

~~~rust
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePolicyResponse {
    pub enabled: bool,
    pub default_ttl_secs: u64,
    pub allowed_ttl_secs: [u64; 3],
    pub auto_without_cache_control: bool,
    pub capacity: usize,
    pub flush_interval_secs: u64,
    pub min_pct: u32,
    pub max_pct: u32,
    pub active_entries: usize,
    pub usage_pct: f64,
    pub dirty: bool,
    pub last_flush_at: Option<String>,
    pub persist_enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetCachePolicyRequest {
    #[serde(default)] pub enabled: Option<bool>,
    #[serde(default)] pub default_ttl_secs: Option<u64>,
    #[serde(default)] pub auto_without_cache_control: Option<bool>,
    #[serde(default)] pub capacity: Option<usize>,
    #[serde(default)] pub flush_interval_secs: Option<u64>,
    #[serde(default)] pub min_pct: Option<u32>,
    #[serde(default)] pub max_pct: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearCacheResponse {
    pub cleared_entries: usize,
}
~~~

- [ ] **Step 4: Refactor hit-rate validation/application**

在 MultiTokenManager 增加：

~~~rust
pub fn validate_cache_hit_rate_bounds(
    min_pct: u32,
    max_pct: u32,
) -> anyhow::Result<()>;

pub fn apply_cache_hit_rate_bounds_runtime(
    &self,
    min_pct: u32,
    max_pct: u32,
) {
    self.cache_hit_rate_min_pct.store(min_pct, Ordering::Relaxed);
    self.cache_hit_rate_max_pct.store(max_pct, Ordering::Relaxed);
}
~~~

现有 set_cache_hit_rate_bounds 改成验证 → 持久化 → runtime apply。

- [ ] **Step 5: Write service response RED test**

~~~rust
#[test]
fn cache_policy_response_calculates_capacity_usage() {
    let response = build_cache_policy_response(
        CachePolicy::default(),
        CacheStats {
            active_entries: 1024,
            capacity: 4096,
            dirty: false,
            last_flush_at: None,
            persist_enabled: true,
        },
        (0, 95),
    );
    assert_eq!(response.default_ttl_secs, 1800);
    assert_eq!(response.usage_pct, 25.0);
    assert_eq!(response.max_pct, 95);
}
~~~

- [ ] **Step 6: Run service RED**

~~~powershell
cargo test cache_policy_response_calculates --all-features -j 1 -- --nocapture
~~~

Expected: 缺少 response helper 和 CacheMeter 注入。

- [ ] **Step 7: Inject SharedCacheMeter and implement service methods**

AdminService 增加 cache_meter: Option<SharedCacheMeter>，构造器填 None，并增加：

~~~rust
pub fn with_cache_meter(mut self, cache_meter: SharedCacheMeter) -> Self {
    self.cache_meter = Some(cache_meter);
    self
}

pub fn get_cache_policy(
    &self
) -> Result<CachePolicyResponse, AdminServiceError>;

pub fn set_cache_policy(
    &self,
    req: SetCachePolicyRequest
) -> Result<CachePolicyResponse, AdminServiceError>;

pub fn clear_cache(
    &self
) -> Result<ClearCacheResponse, AdminServiceError>;
~~~

set_cache_policy 顺序：

1. 从运行时 policy/bounds 合并 patch。
2. CachePolicy::validate 和 hit-rate validate。
3. Config::load(config_path) 写入全部缓存字段并 save。
4. 持久化成功后 cache_meter.update_policy。
5. token_manager.apply_cache_hit_rate_bounds_runtime。
6. 返回最新 response。

config path 不存在时沿用现有接口语义：记录 warning，允许仅运行时更新；path 存在但写失败时返回 500 且不改运行时。

- [ ] **Step 8: Add handlers and routes**

新增 get_cache_policy、set_cache_policy、clear_cache_policy_entries，并注册：

~~~rust
.route(
    "/config/cache-policy",
    get(get_cache_policy).put(set_cache_policy),
)
.route(
    "/config/cache-policy/clear",
    post(clear_cache_policy_entries),
)
~~~

保留 /config/cache-hit-rate。

- [ ] **Step 9: Wire startup policy and AdminService**

main.rs 构造：

~~~rust
let cache_policy = CachePolicy {
    enabled: config.cache_metering_enabled,
    default_ttl_secs: config.cache_default_ttl_secs,
    auto_without_cache_control: config.cache_auto_without_control,
    capacity: config.cache_capacity,
    flush_interval_secs: config.cache_flush_interval_secs,
}.validate().unwrap_or_else(|error| {
    tracing::warn!(%error, "缓存策略配置无效，回退默认值");
    CachePolicy::default()
});

let cache_meter = Arc::new(CacheMeter::with_policy(
    Some(cache_dir.join("cache_metering.json")),
    cache_policy,
));
~~~

AdminService builder 增加：

~~~rust
.with_cache_meter(cache_meter.clone())
~~~

- [ ] **Step 10: Run GREEN and commit**

~~~powershell
cargo test cache_policy_ --all-features -j 1 --quiet
cargo test cache_hit_rate --all-features -j 1 --quiet
cargo test admin:: --all-features -j 1 --quiet
git add -- src/kiro/token_manager.rs src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs src/main.rs
git diff --cached --check
git commit -m "feat(admin): 增加缓存策略管理接口"
~~~

### Task 4: Frontend cache-policy contracts and validation

**Files:**
- Create: admin-ui/src/lib/cache-policy.ts
- Create: admin-ui/src/lib/cache-policy.test.ts
- Modify: admin-ui/src/api/credentials.ts
- Modify: admin-ui/src/hooks/use-credentials.ts

- [ ] **Step 1: Write Bun RED tests**

~~~ts
import { describe, expect, test } from 'bun:test'
import { formatTtl, validateCachePolicyDraft } from './cache-policy'

describe('cache policy', () => {
  test('formats supported TTLs', () => {
    expect(formatTtl(300)).toBe('5 分钟')
    expect(formatTtl(1800)).toBe('30 分钟')
    expect(formatTtl(3600)).toBe('1 小时')
  })

  test('validates capacity, interval and bounds', () => {
    expect(validateCachePolicyDraft({
      capacity: 4096,
      flushIntervalSecs: 60,
      minPct: 0,
      maxPct: 95,
    })).toBeNull()
    expect(validateCachePolicyDraft({
      capacity: 100,
      flushIntervalSecs: 60,
      minPct: 0,
      maxPct: 95,
    })).toContain('256')
    expect(validateCachePolicyDraft({
      capacity: 4096,
      flushIntervalSecs: 60,
      minPct: 99,
      maxPct: 90,
    })).toContain('下界')
  })
})
~~~

- [ ] **Step 2: Run RED**

~~~powershell
cd admin-ui
bun test src/lib/cache-policy.test.ts
~~~

Expected: 缺少 cache-policy.ts。

- [ ] **Step 3: Implement pure helpers**

~~~ts
export const CACHE_TTL_OPTIONS = [300, 1800, 3600] as const

export function formatTtl(seconds: number): string {
  if (seconds === 300) return '5 分钟'
  if (seconds === 1800) return '30 分钟'
  if (seconds === 3600) return '1 小时'
  return String(seconds) + ' 秒'
}

export function validateCachePolicyDraft(value: {
  capacity: number
  flushIntervalSecs: number
  minPct: number
  maxPct: number
}): string | null {
  if (value.capacity < 256 || value.capacity > 65536) {
    return '最大条目必须在 256–65536 内'
  }
  if (value.flushIntervalSecs < 10 || value.flushIntervalSecs > 600) {
    return '落盘周期必须在 10–600 秒内'
  }
  if (value.minPct < 0 || value.minPct > 100 ||
      value.maxPct < 0 || value.maxPct > 100) {
    return '命中率必须在 0–100% 内'
  }
  if (value.minPct > 0 && value.maxPct > 0 &&
      value.minPct > value.maxPct) {
    return '命中率下界不能大于上界'
  }
  return null
}
~~~

- [ ] **Step 4: Add API contracts**

~~~ts
export interface CachePolicyConfig {
  enabled: boolean
  defaultTtlSecs: 300 | 1800 | 3600
  allowedTtlSecs: number[]
  autoWithoutCacheControl: boolean
  capacity: number
  flushIntervalSecs: number
  minPct: number
  maxPct: number
  activeEntries: number
  usagePct: number
  dirty: boolean
  lastFlushAt?: string | null
  persistEnabled: boolean
}

export type SetCachePolicyRequest = Partial<Pick<
  CachePolicyConfig,
  'enabled' | 'defaultTtlSecs' | 'autoWithoutCacheControl' |
  'capacity' | 'flushIntervalSecs' | 'minPct' | 'maxPct'
>>

export async function getCachePolicy(): Promise<CachePolicyConfig> {
  return (await api.get<CachePolicyConfig>('/config/cache-policy')).data
}

export async function setCachePolicy(
  req: SetCachePolicyRequest
): Promise<CachePolicyConfig> {
  return (await api.put<CachePolicyConfig>('/config/cache-policy', req)).data
}

export async function clearCachePolicyEntries():
Promise<{ clearedEntries: number }> {
  return (await api.post<{ clearedEntries: number }>(
    '/config/cache-policy/clear'
  )).data
}
~~~

旧 getCacheHitRate/setCacheHitRate 继续导出。

- [ ] **Step 5: Add hooks**

~~~ts
export function useCachePolicy() {
  return useQuery({
    queryKey: ['cachePolicy'],
    queryFn: getCachePolicy,
  })
}

export function useSetCachePolicy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCachePolicy,
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ['cachePolicy'] }),
  })
}

export function useClearCachePolicyEntries() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: clearCachePolicyEntries,
    onSuccess: () =>
      queryClient.invalidateQueries({ queryKey: ['cachePolicy'] }),
  })
}
~~~

- [ ] **Step 6: Run GREEN and commit**

~~~powershell
bun test src/lib/cache-policy.test.ts
bun run build
cd ..
git add -- admin-ui/src/lib/cache-policy.ts admin-ui/src/lib/cache-policy.test.ts admin-ui/src/api/credentials.ts admin-ui/src/hooks/use-credentials.ts
git diff --cached --check
git commit -m "feat(ui): 增加缓存策略前端契约"
~~~

### Task 5: Cache policy dialog and topbar integration

**Files:**
- Create: admin-ui/src/components/cache-policy-dialog.tsx
- Modify: admin-ui/src/components/topbar-tools.tsx
- Delete: admin-ui/src/components/cache-hit-rate-dialog.tsx

- [ ] **Step 1: Build the approved dialog**

CachePolicyDialog 状态必须包含：

~~~ts
const [enabled, setEnabled] = useState(true)
const [defaultTtlSecs, setDefaultTtlSecs] =
  useState<300 | 1800 | 3600>(1800)
const [autoWithoutCacheControl, setAutoWithoutCacheControl] =
  useState(true)
const [capacity, setCapacity] = useState(4096)
const [flushIntervalSecs, setFlushIntervalSecs] = useState(60)
const [minPct, setMinPct] = useState(0)
const [maxPct, setMaxPct] = useState(0)
~~~

使用 useCachePolicy、useSetCachePolicy、useClearCachePolicyEntries。保存前调用 validateCachePolicyDraft；TTL 只渲染 CACHE_TTL_OPTIONS 三个选项。

页面分区：

1. 模拟缓存计量开关。
2. 默认 TTL 三选一，30 分钟标记“默认”。
3. 无 cache_control 自动缓存开关。
4. 最大条目与落盘周期数字输入。
5. 命中率 min/max，明确冷启动不抬高。
6. 状态卡：active/capacity、usage%、dirty、lastFlushAt、persistEnabled。
7. 费用提示：只改变 rs 计量，不降低 Kiro 成本。
8. 清空缓存二次确认。

确认文字固定为：

~~~text
清空后，后续请求会重新产生 cache_creation；不会删除用量历史。确定继续？
~~~

- [ ] **Step 2: Replace topbar references**

导入 CachePolicyDialog；状态/callback 改为 cachePolicyOpen、setCachePolicyOpen、openCachePolicy。紧凑和完整菜单文字统一为：

~~~text
缓存策略（TTL / 容量 / 命中率）
~~~

渲染：

~~~tsx
<CachePolicyDialog
  open={cachePolicyOpen}
  onOpenChange={setCachePolicyOpen}
/>
~~~

- [ ] **Step 3: Remove old component and verify**

使用 apply_patch 删除 cache-hit-rate-dialog.tsx，然后运行：

~~~powershell
rg -n "CacheHitRateDialog|openCacheHitRate|cacheHitRateOpen" admin-ui/src
cd admin-ui
bun test src/lib/cache-policy.test.ts
bun run build
~~~

Expected: rg 无旧引用，测试和构建通过。

- [ ] **Step 4: Commit**

~~~powershell
cd ..
git add -- admin-ui/src/components/cache-policy-dialog.tsx admin-ui/src/components/cache-hit-rate-dialog.tsx admin-ui/src/components/topbar-tools.tsx
git diff --cached --check
git commit -m "feat(ui): 增加缓存策略管理弹窗"
~~~

### Task 6: Documentation, full verification and local integration

**Files:**
- Modify: README.md

- [ ] **Step 1: Document fields and billing semantics**

配置表增加：

~~~markdown
| cacheMeteringEnabled | true | 是否启用 rs 模拟 prompt cache 计量 |
| cacheDefaultTtlSecs | 1800 | 未显式指定 TTL 时的默认窗口；仅允许 300/1800/3600 |
| cacheAutoWithoutControl | true | 无 cache_control 时是否自动模拟稳定前缀缓存 |
| cacheCapacity | 4096 | 最大前缀缓存条目数，256–65536 |
| cacheFlushIntervalSecs | 60 | 清理及落盘周期，10–600 秒 |
~~~

缓存章节明确：显式 TTL 优先；30m 是 rs 扩展；这是计量模拟，不减少 Kiro 上游成本；冷启动 read 为 0。

- [ ] **Step 2: Run focused backend verification**

~~~powershell
$env:CARGO_TARGET_DIR='D:\kiro2api\kiro-rs2\kiro.rs-admin\target'
$env:RUSTFLAGS='-C debuginfo=0'
cargo test anthropic::cache_metering::tests --all-features -j 1 --quiet
cargo test cache_policy_ --all-features -j 1 --quiet
cargo test cache_hit_rate --all-features -j 1 --quiet
cargo test admin:: --all-features -j 1 --quiet
~~~

- [ ] **Step 3: Run full verification**

~~~powershell
cargo test --all-features -j 1 --quiet
$env:CARGO_INCREMENTAL='0'
cargo check --all-features -j 1 --quiet
Remove-Item Env:CARGO_INCREMENTAL -ErrorAction SilentlyContinue
rustfmt --edition 2024 --check src/model/config.rs src/anthropic/cache_metering.rs src/kiro/token_manager.rs src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs src/main.rs
cd admin-ui
bun test src/lib/cache-policy.test.ts
bun run build
cd ..
git diff --check
~~~

如果 Windows cargo check 因 rustc 内存分配失败，只对 check 设置 CARGO_INCREMENTAL=0 重跑；不得把环境崩溃误报为测试失败或通过。

- [ ] **Step 4: Secret and scope review**

~~~powershell
git status --short
git diff --stat
git diff | Select-String -Pattern 'csk_|sk-kiro-|ANTHROPIC_AUTH_TOKEN|githubToken|profileArn'
~~~

Expected: 无密钥，只包含计划内文件。

- [ ] **Step 5: Commit documentation**

~~~powershell
git add -- README.md
git diff --cached --check
git commit -m "docs(cache): 说明缓存策略与费用边界"
~~~

- [ ] **Step 6: Local Admin API acceptance**

用本地管理员 Key 放入 shell 变量但不输出，验证：

~~~text
GET  /api/admin/config/cache-policy
PUT  /api/admin/config/cache-policy {"defaultTtlSecs":300}
PUT  /api/admin/config/cache-policy {"defaultTtlSecs":1800}
PUT  /api/admin/config/cache-policy {"defaultTtlSecs":3600}
POST /api/admin/config/cache-policy/clear
~~~

断言：三种 TTL 无需重启立即生效；非法 600 返回 400；clear 返回 clearedEntries；旧 cache-hit-rate API 仍工作。

- [ ] **Step 7: Cache accounting acceptance**

构造同 session 的两轮请求并断言：

1. 冷请求 cacheRead=0，creation>0。
2. warm 请求 cacheRead>0。
3. 客户端显式 5m 的有效 TTL 为 300，不被默认 1800 覆盖。
4. 禁用后 creation/read 均为 0。
5. 重新启用不自动清空；clear 后 activeEntries=0。
6. input+creation+read 等于客户端可见输入总量。

- [ ] **Step 8: Hand off branch**

确认 git status --short 为空，记录最终提交 hash。不要推送或部署，除非用户明确授权；完成后使用 superpowers:finishing-a-development-branch 提供本地合并、PR、保留分支选项。
