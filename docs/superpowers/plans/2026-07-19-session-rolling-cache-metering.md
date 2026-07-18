# 会话滚动缓存计量 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为 RS 模拟 Prompt Cache 增加可运行时开关的最近前缀滚动窗口、批量 LRU 和管理端观测，使连续对话稳定产生 cache_read 而不被超长历史反复挤成 cache_creation。

**Architecture:** `CachePolicy` 增加滚动开关和窗口大小；`extract_segments` 仍计算完整 token 链，但 lookup/record 只接收最近 N 个候选，滚动模式使用 v2 哈希命名空间，关闭开关时恢复 v1 全历史行为。CacheMeter 在满载时先清过期项再批量淘汰到 95%，运行期计数通过现有 Admin API 暴露；React 管理端提供独立开关、窗口输入和风险提示。

**Tech Stack:** Rust 2024、Axum、Serde、parking_lot、React 19、TypeScript 6、Bun、Vite、Docker/BuildKit

---

## 文件职责

- `src/anthropic/cache_metering.rs`：滚动候选选择、v1/v2 哈希、lookup/record、批量淘汰和运行期计数。
- `src/model/config.rs`：持久化配置字段、默认值和 Serde 兼容。
- `src/main.rs`：启动时把 Config 映射到 CachePolicy。
- `src/admin/types.rs`：Admin API 的缓存策略请求/响应契约。
- `src/admin/service.rs`：运行时更新、持久化和状态响应。
- `admin-ui/src/api/credentials.ts`：前端 API 类型。
- `admin-ui/src/lib/cache-policy.ts`：草稿校验。
- `admin-ui/src/lib/cache-policy.test.ts`：前端校验测试。
- `admin-ui/src/components/cache-policy-dialog.tsx`：开关、窗口参数、状态与警告。
- `config.example.json`：可部署配置示例。
- `docs/superpowers/specs/2026-07-19-session-rolling-cache-metering-design.md`：已批准设计及开关补充。

### Task 1: 锁定配置和 Admin API 契约

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/anthropic/cache_metering.rs`
- Modify: `src/main.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`

- [ ] **Step 1: Write failing Rust contract tests**

在 `src/model/config.rs` 现有测试模块加入：

```rust
#[test]
fn cache_rolling_policy_defaults_are_enabled_and_bounded() {
    let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
    assert!(config.cache_rolling_prefix_enabled);
    assert_eq!(config.cache_rolling_prefix_limit, 8);
}

#[test]
fn cache_rolling_policy_round_trips_camel_case() {
    let config: Config = serde_json::from_value(serde_json::json!({
        "cacheRollingPrefixEnabled": false,
        "cacheRollingPrefixLimit": 16
    }))
    .unwrap();
    assert!(!config.cache_rolling_prefix_enabled);
    assert_eq!(config.cache_rolling_prefix_limit, 16);
    let encoded = serde_json::to_value(config).unwrap();
    assert_eq!(encoded["cacheRollingPrefixEnabled"], false);
    assert_eq!(encoded["cacheRollingPrefixLimit"], 16);
}
```

在 `src/admin/types.rs` 现有测试模块扩展 `cache_policy_patch_is_partial_and_camel_case`：

```rust
let patch: SetCachePolicyRequest = serde_json::from_value(serde_json::json!({
    "rollingPrefixEnabled": false,
    "rollingPrefixLimit": 12
}))
.unwrap();
assert_eq!(patch.rolling_prefix_enabled, Some(false));
assert_eq!(patch.rolling_prefix_limit, Some(12));
```

在 `src/anthropic/cache_metering.rs` 测试模块加入：

```rust
#[test]
fn cache_policy_rejects_rolling_limit_outside_two_to_sixty_four() {
    let invalid_low = CachePolicy {
        rolling_prefix_limit: 1,
        ..CachePolicy::default()
    };
    assert!(invalid_low.validate().is_err());

    let invalid_high = CachePolicy {
        rolling_prefix_limit: 65,
        ..CachePolicy::default()
    };
    assert!(invalid_high.validate().is_err());
}
```

- [ ] **Step 2: Run RED tests**

Run:

```powershell
cargo test -j 1 cache_rolling_policy
```

Expected: FAIL to compile because the rolling fields do not exist.

- [ ] **Step 3: Add persisted and runtime policy fields**

在 `Config` 增加：

```rust
#[serde(default = "default_cache_rolling_prefix_enabled")]
pub cache_rolling_prefix_enabled: bool,

#[serde(default = "default_cache_rolling_prefix_limit")]
pub cache_rolling_prefix_limit: usize,
```

并定义：

```rust
fn default_cache_rolling_prefix_enabled() -> bool {
    true
}

fn default_cache_rolling_prefix_limit() -> usize {
    8
}
```

在 `CachePolicy` 增加同名运行时字段：

```rust
pub rolling_prefix_enabled: bool,
pub rolling_prefix_limit: usize,
```

`CachePolicy::default()` 使用 `true` 和 `8`；`validate()` 增加：

```rust
if !(2..=64).contains(&self.rolling_prefix_limit) {
    anyhow::bail!("cacheRollingPrefixLimit 必须在 2..=64 内");
}
```

在 `src/main.rs` 构造策略时映射两个字段。更新所有测试内的 `CachePolicy { ... }` 字面量；能用结构更新语法的统一使用 `..CachePolicy::default()`，避免新增字段时再次破坏测试。

- [ ] **Step 4: Extend Admin request/response and persistence**

`CachePolicyResponse` 增加：

```rust
pub rolling_prefix_enabled: bool,
pub rolling_prefix_limit: usize,
```

`SetCachePolicyRequest` 增加两个可选字段：

```rust
#[serde(default)]
pub rolling_prefix_enabled: Option<bool>,
#[serde(default)]
pub rolling_prefix_limit: Option<usize>,
```

`AdminService::set_cache_policy` 使用 `unwrap_or(current...)` 形成完整策略，并持久化到 `Config`。`build_cache_policy_response` 从 `CachePolicy` 和 `CacheStats` 填充新增字段。

- [ ] **Step 5: Run GREEN contract tests**

Run:

```powershell
cargo test -j 1 cache_rolling_policy
cargo test -j 1 cache_policy_patch_is_partial_and_camel_case
cargo test -j 1 cache_policy_response_calculates_capacity_usage
```

Expected: all selected tests PASS.

- [ ] **Step 6: Commit the contract slice**

```powershell
git add -- src/model/config.rs src/anthropic/cache_metering.rs src/main.rs src/admin/types.rs src/admin/service.rs
git diff --cached --check
git commit -m "feat(cache): 增加滚动缓存策略契约"
```

### Task 2: 实现最近前缀窗口和运行时回退开关

**Files:**
- Modify: `src/anthropic/cache_metering.rs`

- [ ] **Step 1: Write failing rolling-window tests**

在同文件测试模块增加一个纯选择函数测试和一个跨轮命中测试：

```rust
#[test]
fn rolling_candidates_keep_only_the_deepest_eight_segments() {
    let segments: Vec<Segment> = (1..=2_223)
        .map(|i| Segment {
            hash: i as u64,
            cumulative_tokens: i as u32,
            ttl_secs: 1_800,
        })
        .collect();
    let selected = select_cache_candidates(&segments, true, 8);
    assert_eq!(selected.len(), 8);
    assert_eq!(selected.first().unwrap().hash, 2_216);
    assert_eq!(selected.last().unwrap().hash, 2_223);
}

#[test]
fn disabling_rolling_candidates_restores_full_history() {
    let segments: Vec<Segment> = (1..=128)
        .map(|i| Segment {
            hash: i as u64,
            cumulative_tokens: i as u32,
            ttl_secs: 1_800,
        })
        .collect();
    assert_eq!(select_cache_candidates(&segments, false, 8).len(), 128);
}
```

扩展现有跨轮 MessagesRequest fixture：第一次调用 `compute_cache_usage` 后追加 user/assistant，再次调用，断言：

```rust
assert!(warm.cache_read > 0);
assert!(warm.cache_read < warm.cache_covered_est);
assert_eq!(cache.stats().active_entries, 10); // 首轮 8，下一轮仅新增 2
```

如果 fixture 的历史不足 8 段，改为构造 20 条交替 user/assistant 文本，确保窗口确实发生滑动。

- [ ] **Step 2: Run RED tests**

```powershell
cargo test -j 1 rolling_candidates
cargo test -j 1 rolling_window_warm_turn
```

Expected: FAIL because selection helper and rolling behavior do not exist.

- [ ] **Step 3: Implement candidate selection**

在 `Segment` 后增加：

```rust
fn select_cache_candidates(
    segments: &[Segment],
    rolling_enabled: bool,
    rolling_limit: usize,
) -> &[Segment] {
    if !rolling_enabled || segments.len() <= rolling_limit {
        return segments;
    }
    &segments[segments.len() - rolling_limit..]
}
```

`compute_cache_usage` 中先保留完整 `segments`，随后：

```rust
let candidates = select_cache_candidates(
    &segments,
    policy.rolling_prefix_enabled,
    policy.rolling_prefix_limit,
);
let hashes: Vec<u64> = candidates.iter().map(|segment| segment.hash).collect();
let cum_tokens: Vec<u32> = candidates
    .iter()
    .map(|segment| segment.cumulative_tokens)
    .collect();
```

`covered` 仍取完整链最深前缀：

```rust
let covered = segments.last().unwrap().cumulative_tokens;
```

因为候选切片一定包含完整链最后一段，所以最深命中与 creation 差值仍可使用候选累计 token。

- [ ] **Step 4: Add independent v2 hash namespace**

在 `extract_segments` 写入 isolation seed 后增加：

```rust
if policy.rolling_prefix_enabled {
    hasher.update(b"|cache-meter:v2|");
}
```

关闭开关时不加入版本字符串，从而恢复当前 v1 hash 行为。补测试证明同一请求在开关两侧首段 hash 不同，且关闭开关两次计算结果稳定。

- [ ] **Step 5: Replace linear DEBUG dumps with a constant-size summary**

删除逐段 `dump.join(", ")`，改为：

```rust
tracing::debug!(
    all_segments = segments.len(),
    candidates = candidates.len(),
    hits = results.iter().filter(|result| result.hit).count(),
    misses = results.iter().filter(|result| !result.hit).count(),
    deepest_hit = deepest_hit.map(|index| index as i64).unwrap_or(-1),
    cache_read,
    cache_creation = covered.saturating_sub(cache_read),
    ttl_secs = candidates.first().map(|segment| segment.ttl_secs).unwrap_or(0),
    rolling = policy.rolling_prefix_enabled,
    "CacheMeter summary"
);
```

不得输出 prompt、session id、工具参数或全量 hash。

- [ ] **Step 6: Run rolling tests and existing cache tests**

```powershell
cargo test -j 1 rolling_candidates
cargo test -j 1 anthropic::cache_metering
```

Expected: selected rolling tests and the complete cache_metering module PASS.

- [ ] **Step 7: Commit rolling algorithm**

```powershell
git add -- src/anthropic/cache_metering.rs
git diff --cached --check
git commit -m "feat(cache): 限制最近前缀滚动窗口"
```

### Task 3: 实现批量淘汰和运行期观测

**Files:**
- Modify: `src/anthropic/cache_metering.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`

- [ ] **Step 1: Write failing eviction and counter tests**

新增测试：

```rust
#[test]
fn record_over_capacity_evicts_in_a_five_percent_batch() {
    let cache = CacheMeter::with_policy(
        None,
        CachePolicy {
            capacity: 256,
            ..CachePolicy::default()
        },
    );
    let hashes: Vec<u64> = (0..257).collect();
    let tokens: Vec<u32> = (0..257).map(|value| value as u32 + 1).collect();
    cache.record(&hashes, &tokens, 1_800);
    let stats = cache.stats();
    assert_eq!(stats.active_entries, 243); // floor(256 * 95 / 100)
    assert_eq!(stats.evictions, 14);
}

#[test]
fn lookup_updates_runtime_hit_and_miss_counters() {
    let cache = CacheMeter::new(None);
    cache.record(&[11], &[100], 1_800);
    cache.lookup(&[11, 12], &[100, 200]);
    let stats = cache.stats();
    assert_eq!(stats.segment_lookups, 2);
    assert_eq!(stats.segment_hits, 1);
    assert_eq!(stats.segment_misses, 1);
}
```

- [ ] **Step 2: Run RED tests**

```powershell
cargo test -j 1 record_over_capacity_evicts_in_a_five_percent_batch
cargo test -j 1 lookup_updates_runtime_hit_and_miss_counters
```

Expected: FAIL because counters and batch target do not exist.

- [ ] **Step 3: Add counters and two distinct eviction paths**

`Inner` 增加非持久化计数：

```rust
segment_lookups: u64,
segment_hits: u64,
segment_misses: u64,
evictions: u64,
expired_entries_removed: u64,
```

使用 `saturating_add` 更新。保留严格容量函数供启动和管理员调小容量：

```rust
fn evict_to_capacity_locked(inner: &mut Inner, capacity: usize) -> usize;
```

增加 record 专用函数：

```rust
fn evict_batch_if_needed_locked(inner: &mut Inner, capacity: usize, now: i64) -> usize;
```

行为必须是：

1. `len <= capacity` 立即返回 0。
2. 先删除 `expires_at <= now` 条目并累计 `expired_entries_removed`。
3. 若仍超容量，目标为 `(capacity * 95 / 100).max(1)`。
4. 按 `last_hit_at` 选择最旧条目删除到目标。
5. 实际删除数累计到 `evictions`。

`CacheStats` 和 `stats()` 返回所有计数；持久化 JSON 仍只序列化 entries，不序列化计数。

将 `CacheEntry.last_hit_at` 的新写入值改为 Unix 毫秒；`expires_at` 继续使用 Unix 秒。新增失败测试 `lru_touch_timestamp_uses_millisecond_resolution`，证明高并发 LRU 不再以整秒产生大量并列值。旧 JSON 中的秒级 `last_hit_at` 无需迁移，会在新条目出现后自然成为更旧候选。

同时给 `CachePolicyResponse` 增加并由 `build_cache_policy_response` 填充：

```rust
pub segment_lookups: u64,
pub segment_hits: u64,
pub segment_misses: u64,
pub evictions: u64,
pub expired_entries_removed: u64,
```

- [ ] **Step 4: Run GREEN tests and service response test**

```powershell
cargo test -j 1 record_over_capacity_evicts_in_a_five_percent_batch
cargo test -j 1 lookup_updates_runtime_hit_and_miss_counters
cargo test -j 1 cache_policy_response_calculates_capacity_usage
```

Expected: all PASS.

- [ ] **Step 5: Commit eviction and observability**

```powershell
git add -- src/anthropic/cache_metering.rs src/admin/types.rs src/admin/service.rs
git diff --cached --check
git commit -m "perf(cache): 批量淘汰并增加运行观测"
```

### Task 4: 增加管理端开关和参数输入

**Files:**
- Modify: `admin-ui/src/api/credentials.ts`
- Modify: `admin-ui/src/lib/cache-policy.ts`
- Modify: `admin-ui/src/lib/cache-policy.test.ts`
- Modify: `admin-ui/src/components/cache-policy-dialog.tsx`

- [ ] **Step 1: Write failing frontend validation tests**

给 `validateCachePolicyDraft` 的输入增加 `rollingPrefixLimit`，新增：

```ts
test('validates rolling prefix limit', () => {
  const valid = {
    capacity: 65_536,
    flushIntervalSecs: 60,
    rollingPrefixLimit: 8,
    minPct: 0,
    maxPct: 95,
  }
  expect(validateCachePolicyDraft(valid)).toBeNull()
  expect(validateCachePolicyDraft({ ...valid, rollingPrefixLimit: 1 })).toContain('2–64')
  expect(validateCachePolicyDraft({ ...valid, rollingPrefixLimit: 65 })).toContain('2–64')
})
```

更新同文件现有 fixture，全部补上 `rollingPrefixLimit: 8`。

- [ ] **Step 2: Run RED frontend test**

```powershell
cd admin-ui
bun test src/lib/cache-policy.test.ts
```

Expected: FAIL because the validator does not accept or validate the field.

- [ ] **Step 3: Extend frontend API types and validator**

`CachePolicyConfig` 增加：

```ts
rollingPrefixEnabled: boolean
rollingPrefixLimit: number
segmentLookups: number
segmentHits: number
segmentMisses: number
evictions: number
expiredEntriesRemoved: number
```

`SetCachePolicyRequest` 的 Pick 加入 `rollingPrefixEnabled` 和 `rollingPrefixLimit`。校验器加入：

```ts
if (
  !Number.isInteger(value.rollingPrefixLimit) ||
  value.rollingPrefixLimit < 2 ||
  value.rollingPrefixLimit > 64
) {
  return '滚动前缀数必须是 2–64 内的整数'
}
```

- [ ] **Step 4: Add UI switch, input and warning**

`CachePolicyDialog` 增加状态：

```ts
const [rollingPrefixEnabled, setRollingPrefixEnabled] = useState(true)
const [rollingPrefixLimit, setRollingPrefixLimit] = useState(8)
```

加载响应和保存请求时包含两字段。在“无 cache_control 自动缓存”之后加入：

```tsx
<SettingSwitch
  id="cache-rolling-prefix-enabled"
  label="最近前缀滚动缓存"
  description="每次只登记最近的可复用前缀，避免超长对话占满缓存；关闭后恢复旧算法。"
  checked={rollingPrefixEnabled}
  onCheckedChange={setRollingPrefixEnabled}
  disabled={busy || !enabled}
/>
```

参数区加入：

```tsx
<NumberField
  id="cache-rolling-prefix-limit"
  label="每请求滚动前缀数"
  description="范围 2–64，推荐 8；仅在滚动缓存开启时生效。"
  min={2}
  max={64}
  value={rollingPrefixLimit}
  onChange={setRollingPrefixLimit}
  disabled={busy || !enabled || !rollingPrefixEnabled}
/>
```

关闭时展示带 `role="alert"` 的琥珀色警告，内容固定为：

```text
已恢复旧的全历史前缀算法。超长对话可能一次写入数千条缓存记录，建议只用于临时对比或紧急回退。
```

运行状态增加段命中率和淘汰数；命中率分母为 0 时显示 `尚无数据`，不得产生 NaN。

- [ ] **Step 5: Run frontend tests and build**

```powershell
bun test src/lib/cache-policy.test.ts
bun run build
```

Expected: tests PASS and Vite production build exits 0.

- [ ] **Step 6: Commit Admin UI**

```powershell
git add -- admin-ui/src/api/credentials.ts admin-ui/src/lib/cache-policy.ts admin-ui/src/lib/cache-policy.test.ts admin-ui/src/components/cache-policy-dialog.tsx
git diff --cached --check
git commit -m "feat(admin): 增加滚动缓存运行开关"
```

### Task 5: 更新配置示例并完成范围回归

**Files:**
- Modify: `config.example.json`
- Modify: `docs/superpowers/specs/2026-07-19-session-rolling-cache-metering-design.md`

- [ ] **Step 1: Add deployable example values**

在 `config.example.json` 的缓存字段附近加入：

```json
"cacheRollingPrefixEnabled": true,
"cacheRollingPrefixLimit": 8,
"cacheCapacity": 65536
```

保留合法 JSON，若文件已有 `cacheCapacity` 则替换值而不是产生重复键。

- [ ] **Step 2: Self-review switch wording in the approved spec**

确认设计文档明确包含：默认开启、关闭恢复 v1、无需重启、与另外两个缓存开关独立、切换时首轮允许冷创建、关闭时 UI 警告。逐节检查每个参数都有确定默认值、范围和回退语义。

- [ ] **Step 3: Run static checks**

```powershell
git diff --check
```

Expected: `git diff --check` exits 0.

- [ ] **Step 4: Commit docs and example**

```powershell
git add -- config.example.json
git add -f -- docs/superpowers/specs/2026-07-19-session-rolling-cache-metering-design.md docs/superpowers/plans/2026-07-19-session-rolling-cache-metering.md
git diff --cached --check
git commit -m "docs(cache): 补充滚动缓存配置与实施计划"
```

### Task 6: 全量验证和隔离 8991 验收

**Files:**
- No source changes expected

- [ ] **Step 1: Run formatting and focused local verification**

```powershell
cargo fmt --all -- --check
cargo test -j 1 anthropic::cache_metering
cargo test -j 1 cache_policy
cd admin-ui
bun test src/lib/cache-policy.test.ts
bun run build
```

Expected: formatting, focused Rust tests, Bun tests and build all exit 0。若 Windows 本机再次出现 `memory allocation failed`，记录为环境限制，不把它报告为测试通过，转入 Step 2 的 Linux 构建验证。

- [ ] **Step 2: Run the Linux server build/test gate**

将当前提交作为隔离测试 ref 提供给 `/opt/kiro-rs-test/scripts/test-deploy.sh`，只构建 `kiro-rs-test`，不得操作 `kiro-rs-admin`。构建前先执行：

```bash
cargo test -j 2 anthropic::cache_metering
cargo test -j 2 cache_policy
```

Expected: all Rust tests PASS，随后生成 `kiro-rs-test:<commit>` 镜像。

- [ ] **Step 3: Verify Admin runtime switching on 8991**

在 `https://rs-test.43-225-196-10.sslip.io/admin`：

1. 保存 `rollingPrefixEnabled=true, rollingPrefixLimit=8, capacity=65536`。
2. 重新 GET `/api/admin/config/cache-policy`，确认立即返回同样值。
3. 关闭滚动开关，确认无需重启且容器 restart count 不变。
4. 重新开启，确认状态恢复且历史 usage 没有被清空。

- [ ] **Step 4: Replay long conversations and inspect summary logs**

回放 2,200、1,100、1,290 消息三种长会话。滚动开启时验证：

```text
candidates <= 8
DEBUG 单行大小不随 all_segments 线性增长
warm 轮次 deepest_hit 存在
cache_creation 主要对应新增消息
```

关闭滚动时只执行一次受控对比，确认 candidates 恢复全历史后立即重新开启，避免测试容器缓存被长期污染。

- [ ] **Step 5: Observe one 30-minute cache window**

以测试配置修改时间为边界，按 `request_start = ts - durationMs` 聚合 usage，并记录：calls、creation、read、creation-only、read-positive、entry 数、evictions、segment hit rate。验收目标：

```text
连续长会话 warm cache_read 占比通常达到 80%–95%
单请求候选不超过 8
容量不再以约 18 秒速度整表周转
无对话中断、工具错误或 SSE 回归
```

- [ ] **Step 6: Final scope and secret review**

```powershell
git status --short --branch
git diff master --stat
git log --oneline master..HEAD
git diff master --check
```

检查差异不包含 `csk_`、服务器凭据、`config.json`、数据库、WAL、日志或 `scripts/11.txt`。

- [ ] **Step 7: Enter branch finishing workflow**

使用 `superpowers:finishing-a-development-branch`。只有所有门禁通过后才允许合并回本地 master；不自动 push，不修改生产 8990。
