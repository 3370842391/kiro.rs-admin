# 默认最好模式 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 新增默认最好模式，使用 Kiro Runtime → Legacy Kiro IDE → Legacy CodeWhisperer → Legacy Amazon Q 的安全四端点降级，并用会话粘性、首字节 EWMA 和实时 in-flight 负载改善账号调度。

**Architecture:** 配置层增加 `EndpointMode`（best/manual），Provider 根据模式解析默认端点和降级链；TokenManager 维护短 TTL 的会话亲和与凭据首字节 EWMA，Provider/handler 在流式首个上游 chunk 到达时更新指标。Admin API 与现有 endpoint chains 对话框提供模式切换，旧配置继续按 manual 语义运行。

**Tech Stack:** Rust、Axum、Serde、Tokio、parking_lot、React/TypeScript、TanStack Query、Bun。

---

### Task 1: 配置模式与固定最佳端点链

**Files:**
- Modify: `src/model/config.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/kiro/token_manager.rs`
- Modify: `src/main.rs`
- Test: `src/model/config.rs`, `src/kiro/provider.rs`

- [ ] **Step 1: Write failing tests**

  在配置测试中加入：缺少 `endpointMode` 时得到 `best`；`manual` 可 round-trip；非法字符串返回 Serde 错误。在 provider 测试中加入：最佳模式默认 endpoint 为 `runtime`，固定链为 `ide, codewhisperer, amazonq`。

- [ ] **Step 2: Run tests to verify failure**

  Run: `cargo test -j 1 endpoint_mode best_endpoint_chain`

  Expected: FAIL，因为 `EndpointMode` 和最佳模式解析尚不存在。

- [ ] **Step 3: Implement minimal configuration and endpoint resolution**

  增加 `EndpointMode { Best, Manual }`，字段使用 `#[serde(default)] pub endpoint_mode: EndpointMode`；默认值为 `Best`。在 `KiroProvider` 增加 `endpoint_mode()`、`effective_endpoint_name()` 和 `fallback_chain_for()`：最佳模式只对未显式 endpoint 的 IDE 凭据返回 `runtime` 和三项固定降级，manual 保持原配置覆盖。`main.rs` 启动校验允许最佳模式的 runtime 注册，并打印模式和链。

- [ ] **Step 4: Run focused tests**

  Run: `cargo test -j 1 endpoint_mode best_endpoint_chain`

  Expected: PASS。

- [ ] **Step 5: Commit**

  ```powershell
  git add src/model/config.rs src/kiro/provider.rs src/kiro/token_manager.rs src/main.rs
  git commit -m "feat(route): add default best endpoint mode"
  ```

### Task 2: 会话粘性与实时首字节调度

**Files:**
- Modify: `src/kiro/token_manager.rs`
- Modify: `src/kiro/provider.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `admin-ui/src/types/api.ts`
- Test: `src/kiro/token_manager.rs`, `src/kiro/provider.rs`

- [ ] **Step 1: Write failing tests**

  加入测试：同一 affinity key 在 TTL 内优先得到相同账号；粘性账号达到过载阈值时选择其他健康账号；两个账号负载相同且一个首字节 EWMA 更慢时选择更快账号；无首字节样本时不降权；最佳模式的会话 key 不跨 group 共享。

- [ ] **Step 2: Run tests to verify failure**

  Run: `cargo test -j 1 session_affinity first_byte_ewma`

  Expected: FAIL，因为 TokenManager 尚无 affinity 和 EWMA 状态。

- [ ] **Step 3: Implement runtime state and provider hook**

  在 TokenManager 增加进程内 `session_affinity` map、5 分钟 TTL 和 `first_byte_ewma_ms` 字段；保留现有无 affinity 的 API，并新增带 affinity 的 acquire 方法。使用 in-flight 作为硬事实，粘性仅在账号健康且负载未超过阈值时生效。增加 `record_first_byte_latency(id, ms)`，alpha=0.3，忽略 0/异常值。Provider 从请求体安全提取 conversation ID，并在最佳模式调用带 affinity 的 acquire；handler 在首次收到上游 body chunk 后调用 provider 记录对应账号的首字节延迟。Admin snapshot 增加可选 `firstByteEwmaMs`。

- [ ] **Step 4: Run focused tests**

  Run: `cargo test -j 1 session_affinity first_byte_ewma`

  Expected: PASS。

- [ ] **Step 5: Commit**

  ```powershell
  git add src/kiro/token_manager.rs src/kiro/provider.rs src/anthropic/handlers.rs src/admin/handlers.rs admin-ui/src/types/api.ts
  git commit -m "feat(dispatch): add affinity and first-byte aware scheduling"
  ```

### Task 3: 最佳模式 Admin API

**Files:**
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `src/admin/router.rs`
- Test: `src/admin/service.rs`, `src/admin/handlers.rs`

- [ ] **Step 1: Write failing tests**

  测试 GET 返回 `best`、中文标签和四端点链；PUT `manual` 后持久化配置，重新加载仍为 manual；非法 mode 返回 400 且当前值不变。

- [ ] **Step 2: Run tests to verify failure**

  Run: `cargo test -j 1 endpoint_mode_admin`

  Expected: FAIL，因为路由和服务方法不存在。

- [ ] **Step 3: Implement API and persistence**

  增加 `EndpointModeResponse`、`SetEndpointModeRequest`，在 AdminService 中校验 `best/manual`，更新 TokenManager 运行态并写入 config.json；在 router 注册 `/config/endpoint-mode` GET/PUT。响应包含 `mode`、`label`、`primaryEndpoint`、`fallbackEndpoints`、`adaptiveScheduling`。

- [ ] **Step 4: Run focused tests**

  Run: `cargo test -j 1 endpoint_mode_admin`

  Expected: PASS。

- [ ] **Step 5: Commit**

  ```powershell
  git add src/admin/types.rs src/admin/service.rs src/admin/handlers.rs src/admin/router.rs
  git commit -m "feat(admin): expose endpoint mode switch"
  ```

### Task 4: 管理端模式选择 UI

**Files:**
- Modify: `admin-ui/src/api/credentials.ts`
- Modify: `admin-ui/src/hooks/use-credentials.ts`
- Modify: `admin-ui/src/components/endpoint-chains-dialog.tsx`
- Test: `admin-ui/src/components/endpoint-mode-ui.contract.test.ts`

- [ ] **Step 1: Write failing UI contract test**

  断言 endpoint chains 对话框包含“默认最好模式”、`Kiro Runtime`、`Legacy Kiro IDE`、`Legacy CodeWhisperer`、`Legacy Amazon Q`，并保留“手动端点链”入口。

- [ ] **Step 2: Run test to verify failure**

  Run: `cd admin-ui; bun test src/components/endpoint-mode-ui.contract.test.ts`

  Expected: FAIL，因为 API hook 和模式选择控件不存在。

- [ ] **Step 3: Implement API hooks and UI**

  增加 endpoint mode API 类型、query/mutation hook；在现有 endpoint chains 对话框顶部加入模式选择。选择 best 时展示只读四端点链和实时调度说明；选择 manual 时显示现有可编辑链和参数。保存模式后刷新 endpoint chains 与 credentials 查询，错误使用现有 toast 处理。

- [ ] **Step 4: Run focused UI test and build**

  Run: `cd admin-ui; bun test src/components/endpoint-mode-ui.contract.test.ts; bun run build`

  Expected: PASS，生产构建成功。

- [ ] **Step 5: Commit**

  ```powershell
  git add admin-ui/src/api/credentials.ts admin-ui/src/hooks/use-credentials.ts admin-ui/src/components/endpoint-chains-dialog.tsx admin-ui/src/components/endpoint-mode-ui.contract.test.ts
  git commit -m "feat(admin-ui): add best endpoint mode selector"
  ```

### Task 5: 文档、回归验证与交付

**Files:**
- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] **Step 1: Document behavior and customer impact**

  说明最佳模式默认启用、四端点顺序、串行安全重试、会话粘性 TTL、首字节调度；明确不会并发重复执行请求，不改变工具调用、SSE 顺序、计费和缓存字段。说明切换 manual 可恢复旧行为。

- [ ] **Step 2: Run complete verification**

  Run: `cargo fmt --all -- --check; cargo test -j 1; cd admin-ui; bun test; bun run build; git diff --check`

  Expected: 所有测试通过、构建成功、无 diff 检查错误。

- [ ] **Step 3: Commit documentation**

  ```powershell
  git add README.md CHANGELOG.md docs/superpowers/specs/2026-07-22-endpoint-best-mode-design.md docs/superpowers/plans/2026-07-22-endpoint-best-mode.md
  git commit -m "docs(route): document default best endpoint mode"
  ```

