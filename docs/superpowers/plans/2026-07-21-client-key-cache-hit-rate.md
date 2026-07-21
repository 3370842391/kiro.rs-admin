# 客户端 Key 级缓存命中率覆盖 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 为每个客户端 API Key 增加可选的缓存命中率最小/最大值覆盖，并贯穿鉴权、缓存 usage 计量和管理端创建/编辑流程。

**Architecture:** `ClientKey` 持久化一个可选 `CacheHitRateBounds`；鉴权时把它复制到不可变的 `KeyContext`/usage hook 快照。缓存计量统一使用“Key 覆盖优先，否则 provider 全局配置”的解析函数，旧数据自然继承全局。

**Tech Stack:** Rust、Axum、Serde、SQLite/JSON 持久化、React 19、TypeScript、Bun test。

---

### Task 1: 后端缓存策略值对象与 Key 持久化

**Files:**
- Modify: `src/admin/client_keys.rs`
- Modify: `src/admin/types.rs`
- Test: `src/admin/client_keys.rs` tests

- [ ] **Step 1: Write failing tests**

覆盖以下行为：旧 JSON 缺失字段时为 `None`；自定义值能 round-trip；`0/0` 和单侧边界合法；越界或非零 `min>max` 被拒绝；更新持久化失败时恢复旧值。

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test admin::client_keys`

Expected: 新增测试因缺少缓存策略字段、构造参数或更新接口而失败。

- [ ] **Step 3: Implement minimal model and manager APIs**

增加可复制、可序列化的 `CacheHitRateBounds { min_pct, max_pct }`，给 `ClientKey` 和 `AuthorizedClientKey` 增加可选字段；扩展创建和 `update_meta`，并在 manager 内复用全局同等校验规则。

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test admin::client_keys`

Expected: 全部客户端 Key 测试通过。

- [ ] **Step 5: Commit**

```powershell
git add src/admin/client_keys.rs src/admin/types.rs
git commit -m "feat(key): 增加客户端缓存命中率策略"
```

### Task 2: API 创建/更新/列表与鉴权快照

**Files:**
- Modify: `src/admin/handlers.rs`
- Modify: `src/anthropic/middleware.rs`
- Modify: `src/anthropic/handlers.rs`
- Test: `src/admin/handlers.rs`, `src/anthropic/middleware.rs`

- [ ] **Step 1: Write failing API and snapshot tests**

验证创建能接收自定义值、更新支持 `inherit/custom` patch、非法值返回 400、列表返回覆盖值，且 `KeyContext` 保存鉴权瞬间的覆盖快照。

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test admin::handlers anthropic::middleware`

Expected: 新 payload 字段无法反序列化或响应缺少策略字段。

- [ ] **Step 3: Implement API and middleware propagation**

增加 camelCase 请求/响应类型；创建缺省为继承；更新缺省保持不变、显式 `inherit` 清除；在 `auth_middleware` 把授权 Key 的策略复制到 `KeyContext`；列表和创建响应返回当前策略。

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test admin::handlers anthropic::middleware`

Expected: API 与鉴权测试全部通过。

- [ ] **Step 5: Commit**

```powershell
git add src/admin/handlers.rs src/anthropic/middleware.rs src/anthropic/handlers.rs
git commit -m "feat(key): 贯通缓存策略鉴权快照"
```

### Task 3: 所有响应路径使用 Key 覆盖值

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/middleware.rs`
- Test: `src/anthropic/cache_metering.rs`, `src/anthropic/handlers.rs`

- [ ] **Step 1: Write failing precedence tests**

验证 Key 覆盖值优先于 provider 全局值；无覆盖时仍使用全局值；`0/0` 会关闭整形；流式、非流式和本地精确回复使用同一结果。

- [ ] **Step 2: Run focused tests and verify RED**

Run: `cargo test anthropic::cache_metering anthropic::handlers`

Expected: 当前代码始终读取 provider 全局值，覆盖优先级测试失败。

- [ ] **Step 3: Implement one resolver and replace all call sites**

新增一个接收 `Option<CacheHitRateBounds>` 与 provider 全局值的解析函数，统一替换所有 `provider.cache_hit_rate_bounds()` usage 计量调用，包括本地 exact、严格 JSON、流式和非流式路径。

- [ ] **Step 4: Run focused tests and verify GREEN**

Run: `cargo test anthropic::cache_metering anthropic::handlers`

Expected: 覆盖优先级和既有缓存测试全部通过。

- [ ] **Step 5: Commit**

```powershell
git add src/anthropic/cache_metering.rs src/anthropic/handlers.rs src/anthropic/middleware.rs
git commit -m "fix(cache): 按客户端 Key 应用命中率覆盖"
```

### Task 4: 管理端创建/编辑/列表 UI

**Files:**
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/api/client-keys.ts`
- Modify: `admin-ui/src/components/client-keys-page.tsx`
- Test: `admin-ui/src/lib/client-key-cache-hit-rate.test.ts`
- Test: `admin-ui/src/components/client-key-cache-hit-rate-ui.contract.test.ts`

- [ ] **Step 1: Write failing UI/payload tests**

测试继承/自定义切换、`0..100` 校验、`min>max` 拒绝、自定义 payload 和清除时的 `inherit` payload；合约测试确认创建与编辑对话框都有两个输入框和说明。

- [ ] **Step 2: Run tests and verify RED**

Run: `bun test src/lib/client-key-cache-hit-rate.test.ts src/components/client-key-cache-hit-rate-ui.contract.test.ts`

Expected: 新模块或控件尚不存在，测试失败。

- [ ] **Step 3: Implement UI**

增加“继承全局/自定义”选择、最小/最大百分比输入、即时错误提示；列表显示“继承全局”或 `min%–max%`；编辑回填当前策略；提交 API 使用 `custom` 或 `inherit`。

- [ ] **Step 4: Run tests and build**

Run: `bun test` and `bun run build`

Expected: 所有前端测试通过，生产构建成功。

- [ ] **Step 5: Commit**

```powershell
git add admin-ui/src/types/api.ts admin-ui/src/api/client-keys.ts admin-ui/src/components/client-keys-page.tsx admin-ui/src/lib/client-key-cache-hit-rate.test.ts admin-ui/src/components/client-key-cache-hit-rate-ui.contract.test.ts
git commit -m "feat(admin-ui): 支持按 Key 配置缓存范围"
```

### Task 5: 全量验证与本地合并

**Files:**
- Verify only; no additional source files expected.

- [ ] **Step 1: Build embedded frontend**

Run: `bun run build` from `admin-ui`.

- [ ] **Step 2: Run Rust suite**

Run: `cargo test` from repository root.

- [ ] **Step 3: Inspect task-only diff**

Run: `git diff --check` and `git status --short`; ensure only this feature branch files are staged and the main worktree batch-login changes remain untouched.

- [ ] **Step 4: Merge locally**

From the main worktree, fast-forward or merge `feature/client-key-cache-bounds` into `master`, then rerun the focused cache/key tests on the merged result.

- [ ] **Step 5: Report**

Report commit hashes, tests, and the exact admin behavior. Do not push to GitHub or deploy unless separately requested.
