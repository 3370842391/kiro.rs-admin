# 工具参数兼容与错误现场治理实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 修复确定性 `read_file` 参数别名错误，保留安全的上游错误详情，并合并短窗口内重复错误快照。

**Architecture:** 在现有工具 Schema 校验前加入最小字段别名归一化；复用 `AttemptObservation` 的有界错误信息进入终止错误与 trace；在 `SharedErrorSnapshotStore` 的事务边界内按请求指纹和错误元数据做短窗口去重。三者通过现有错误和快照接口连接，不改变普通响应路径。

**Tech Stack:** Rust、serde_json、SQLite、Tokio、现有 Rust/Bun 测试体系。

---

### Task 1: `read_file.file_path` 别名归一化

**Files:**
- Modify: `src/anthropic/tool_schema.rs`
- Test: `src/anthropic/tool_schema.rs` 内现有单元测试模块

- [ ] **Step 1: 写失败测试**

新增测试，构造 required `path` 且 additionalProperties=false 的 object schema，输入 `{"file_path":"/tmp/a"}`，断言 `validate_and_repair` 返回 `Repaired`、输入变为 `{"path":"/tmp/a"}`；同时覆盖已有 `path` 或非字符串 `file_path` 时返回 Invalid。

- [ ] **Step 2: 运行测试确认失败**

运行：`cargo test tool_schema --lib -- --nocapture`

预期：新增别名正例失败，显示当前缺少 `$.path`。

- [ ] **Step 3: 实现最小别名层**

在 object 校验入口读取 `required` 和 `properties`，仅在 `path` 为必填、输入无 `path`、输入有字符串 `file_path` 且 schema 未声明相反语义时移动字段；把修复路径加入既有 repairs 列表后继续正常 Schema 校验。

- [ ] **Step 4: 运行测试确认通过**

运行：`cargo test tool_schema --lib -- --nocapture`

预期：新增测试与原有 tool schema 测试全部通过。

### Task 2: 保留上游 Error/Exception 详情

**Files:**
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/tool_attempt.rs`（仅在现有终止错误管道需要时）
- Test: `src/anthropic/stream.rs` 现有协议错误测试

- [ ] **Step 1: 写失败测试**

新增 Error 和 Exception 两个测试，向 `StreamContext` 注入对应事件，调用收尾方法，断言错误 SSE 的稳定类型仍保持不变，同时 message 包含错误码和有界原因。

- [ ] **Step 2: 运行测试确认失败**

运行：`cargo test upstream_error --lib -- --nocapture`

预期：当前实现只记录错误码或只生成通用错误，新增断言失败。

- [ ] **Step 3: 实现安全详情传递**

让 `Event::Error`/`Event::Exception` 分支把有界 message 存入现有 `terminal_protocol_error` 或 `AttemptObservation`，继续使用 `public_error` 的敏感信息边界；禁止把请求体、工具输入或认证字段拼入对外消息。

- [ ] **Step 4: 运行测试确认通过**

运行：`cargo test upstream_error --lib -- --nocapture` 和 `cargo test stream --lib -- --nocapture`。

预期：错误详情测试和现有流协议测试全部通过。

### Task 3: 重复错误快照短窗口合并

**Files:**
- Modify: `src/anthropic/error_snapshot.rs`
- Modify: `src/admin/error_snapshot_db.rs`
- Modify: `src/admin/types.rs`（若列表响应需要重复次数字段）
- Test: `src/admin/error_snapshot_db.rs` 和 `src/anthropic/error_snapshot.rs` 现有测试模块

- [ ] **Step 1: 写失败测试**

新增四个测试：相同指纹/错误/模式在 60 秒内只保留一条并累加次数；超过窗口创建新记录；错误类型或响应模式不同不合并；合并存储失败时仍返回原始插入错误且不影响请求处理。

- [ ] **Step 2: 运行测试确认失败**

运行：`cargo test error_snapshot --lib -- --nocapture`

预期：当前每次都新增记录，重复次数断言失败。

- [ ] **Step 3: 实现事务内合并**

在现有快照写入事务中按指纹、错误类型、响应模式和时间窗口查询可合并记录；命中时更新重复计数和 `updated_at`，跳过 payload 写入；未命中沿用原子插入流程。旧数据库记录默认重复次数为 1，迁移保持向后兼容。

- [ ] **Step 4: 运行测试确认通过**

运行：`cargo test error_snapshot --lib -- --nocapture`。

预期：四个新增测试和既有快照治理测试全部通过。

### Task 4: 综合验证与交付

**Files:**
- Test: `scripts/repetition-guard.contract.test.ts`（只验证不改动）

- [ ] **Step 1: 运行 Rust 全量测试**

运行：`cargo test`

预期：全部通过，仅允许已有 warning。

- [ ] **Step 2: 运行管理端与协议合约测试**

运行：`bun test`、`bun test scripts/repetition-guard.contract.test.ts`、`cargo fmt --check`、`cargo metadata --locked --no-deps`。

- [ ] **Step 3: 检查差异并提交**

运行：`git diff --check` 和 `git status --short`，只暂存本计划涉及文件，提交：`fix(protocol): 兼容工具参数并合并错误快照`。

- [ ] **Step 4: 合并到本地 master**

确认测试通过后，将功能分支以非快进方式合并到本地 `master`，保留主工作区已有批量登录改动，不推送远程。
