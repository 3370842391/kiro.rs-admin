# 上游工具与上下文超限恢复 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让确定性的上下文超限快速失败，并安全恢复值不变的工具参数表示差异。

**Architecture:** 在 `provider.rs` 的备用端点响应边界统一分类请求错误，防止错误进入后续端点和账号重试。工具修复继续集中在 `tool_schema.rs` 的事务式候选副本中，新增受 Schema 约束的字段别名与 JSON 数组字符串修复。

**Tech Stack:** Rust、Tokio、Reqwest、Serde JSON、现有 Cargo 单元测试。

---

### Task 1: 备用端点请求错误短路

**Files:**
- Modify: `src/kiro/provider.rs`
- Test: `src/kiro/provider.rs`

- [x] **Step 1: Write the failing tests**

新增分类测试，要求 HTTP 400 和端点识别出的客户端校验错误返回“停止”，HTTP 429 与普通 5xx 返回“继续”。

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test fallback_stops_on_request_wide_errors_but_keeps_transient_failover`
Expected: FAIL，因为备用端点分类函数尚不存在或仍把 400 当成瞬态错误。

- [x] **Step 3: Write minimal implementation**

增加一个只负责响应分类的小函数，并在备用端点非 2xx 分支中先记录 `BAD_REQUEST`，随后立即返回包含原始状态和响应体的错误；其他状态保持现有降级链。

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test fallback_stops_on_request_wide_errors_but_keeps_transient_failover`
Expected: PASS。

### Task 2: 安全工具字段别名

**Files:**
- Modify: `src/anthropic/tool_schema.rs`
- Test: `src/anthropic/tool_schema.rs`

- [x] **Step 1: Write the failing test**

覆盖 `path/file_path/filePath`、`oldStr/old_string/oldString`、`newStr/new_string/newString` 的必填目标字段修复，并断言冲突、已声明源字段和类型不匹配仍保持失败关闭。

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test repairs_bidirectional_tool_field_aliases`
Expected: FAIL，因为这些双向别名尚未完整注册。

- [x] **Step 3: Write minimal implementation**

只扩展 `SAFE_REQUIRED_PROPERTY_ALIASES`。继续复用现有条件：目标必填、目标缺失、源字段未声明、源值与目标类型匹配。

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test repairs_bidirectional_tool_field_aliases`
Expected: PASS。

### Task 3: JSON 数组字符串修复

**Files:**
- Modify: `src/anthropic/tool_schema.rs`
- Test: `src/anthropic/tool_schema.rs`

- [x] **Step 1: Write the failing tests**

必填数组收到 `"[{\"content\":\"x\"}]"` 时应解析并继续校验；普通字符串、超限字符串和不满足 `items` 的数组字符串必须保持无效且不修改原输入。

- [x] **Step 2: Run test to verify it fails**

Run: `cargo test json_encoded`
Expected: FAIL，当前会报告 `expected array`。

- [x] **Step 3: Write minimal implementation**

在类型校验前，仅对必填且声明包含 `array` 类型的字符串尝试 `serde_json::from_str::<Value>`；仅接受数组，并由后续原始 Schema 校验决定事务是否提交。

- [x] **Step 4: Run test to verify it passes**

Run: `cargo test json_encoded`
Expected: PASS。

### Task 4: 回归验证与提交

**Files:**
- Verify: `src/kiro/provider.rs`
- Verify: `src/anthropic/tool_schema.rs`

- [x] **Step 1: Run focused tests**

Run: `cargo test kiro::provider::tests && cargo test anthropic::tool_schema::tests`
Expected: PASS，零失败。

- [x] **Step 2: Run full library tests and build**

Run: `cargo test && cargo build`
Expected: PASS，退出码 0。

- [x] **Step 3: Review and commit**

只暂存本计划涉及的文档、`provider.rs` 和 `tool_schema.rs`，执行 `git diff --cached --check` 后创建中文本地提交，不推送远端。
