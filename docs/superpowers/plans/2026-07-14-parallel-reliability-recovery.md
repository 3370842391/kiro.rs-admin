# RS 并行可靠性恢复与账号区域优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` to implement this plan task-by-task. Every behavior change must follow RED → GREEN, receive spec review first, then code-quality review. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 并行完成 New API 1 秒首响应、API Key 批量导入与 Region/模型路由，并修复生产错误快照中重复出现的 Claude Code 工具转换、工具 Schema 和空请求问题。

**Architecture:** 每个独立问题使用独立 Git worktree 和分支，最多三个实现代理并行，主代理只负责证据归档、规格协调、两阶段审查与集成。所有修复先使用脱敏错误快照生成回归 fixture，再修改最小范围代码；不得猜测工具业务参数、伪造模型能力或把完整 Key/客户正文写入测试和 Git。

**Tech Stack:** Rust 2024、Axum 0.8、Tokio、Serde、Reqwest、SQLite、zstd、React 19、TypeScript 6、Bun Test、Cargo Test、Docker、New API。

---

## 1. 已确认的生产证据

数据来源：`43.225.196.10:18792` 上生产容器 `kiro-rs-admin` 的只读 `error_snapshots.db`。调查只读取错误类型、脱敏结构和工具 Schema，不把完整凭据或客户正文写入本文。

| 问题 | 最近约 6 小时出现次数 | 已确认边界 |
|---|---:|---|
| `tool "read_file" input violates schema: missing required $.path` | 189 | 上游已返回 HTTP 200，但生成的工具入参不满足客户 Schema；现有一次重试仍使用同一请求体，容易重复失败 |
| `Claude Code Read.pages has no Kiro read_file equivalent` | 74 | RS 在请求到达凭据选择和 Kiro 上游前主动 fail-closed，尝试链为 0 |
| `Improperly formed request / REQUEST_BODY_INVALID` | 36 | 已抽样确认一类请求为：system 有内容、唯一 user text 为空、无工具；RS 生成空 `currentMessage.userInputMessage.content` 后被 Kiro 400 |
| `tool "Grep" input violates schema: unexpected property $.case_sensitive` | 33 | RS 把 Kiro `caseSensitive` 反向转换成旧字段 `case_sensitive`，但当前 Claude Code Schema 使用 `-i` 且 `additionalProperties=false` |
| `Upstream returned no assistant content after one retry` | 200 | 已有重试仍耗尽；需区分真实上游空帧、解析丢帧和客户端取消，不能统一猜测修复 |
| `stream_read_error / error decoding response body` | 53 | 可能属于上游/网络，也可能属于解码边界；必须先按 endpoint、凭据和流尾聚类 |

额外样本事实：

- `Read.pages` 是 Claude Code 2.1.30+ 的 PDF 物理页范围，不能转换成文本 `start_line/end_line`。
- 当前 Claude Code `Grep` Schema 接受 `-i`，不接受 `case_sensitive`。
- `read_file` 失败样本中，客户 Schema 明确要求 `path`；不得从用户自然语言中猜路径。
- 当前生产容器版本为 `sha-218ae0`；后续验证必须先部署到隔离 8991，不直接覆盖 8990。

## 2. 并行执行拓扑

### 第一波：三条互不冲突的实现线

| Worktree | 分支 | 负责范围 |
|---|---|---|
| `.worktrees/restore-sse-heartbeat` | `fix/restore-sse-heartbeat` | 立即 `: connected` + 1 秒 `event: ping`，保证 New API 1 秒内收到 `data:` |
| `.worktrees/api-key-region-routing` | `feature/api-key-region-routing` | Rust nickname、API Key 校验、Auth/API Region、Host、模型/用量路由和诊断 |
| `.worktrees/api-key-import-ui` | `feature/api-key-import-ui` | `nickname | ksk_key | apiRegion` 文本解析、预览、批量导入和单条 API Key UI |

### 第二波：工具与请求兼容

第一波任一代理完成并释放并发槽后，依次建立：

| Worktree | 分支 | 负责范围 |
|---|---|---|
| `.worktrees/claude-code-tool-compat` | `fix/claude-code-tool-compat` | `Read.pages` 历史兼容、`Grep -i` 和八个内置工具 Schema 漂移审计 |
| `.worktrees/tool-schema-retry` | `fix/tool-schema-retry` | `read_file` 缺 path 的可观测性、重试请求加固和失败边界 |
| `.worktrees/empty-request-compat` | `fix/empty-request-compat` | 空 user → 空 Kiro currentMessage 的本地检测与可配置兼容 |

### 审查顺序

每条分支必须执行：实现代理自审 → 规格审查代理 → 代码质量审查代理 → 修复 Important/Critical → 复审。不能用实现代理自审替代独立审查。

## 3. Task 0：建立脱敏 fixture 与基线

**Files:**

- Create: `tests/fixtures/tool_compat/read_pages_history.json`
- Create: `tests/fixtures/tool_compat/grep_case_sensitive.json`
- Create: `tests/fixtures/tool_compat/read_file_missing_path.json`
- Create: `tests/fixtures/request_shapes/system_only_empty_user.json`
- Modify: `docs/2026-07-14-completed-work-summary.md`

- [ ] 从错误快照只提取结构：角色、block 类型、工具名、入参 key、Schema properties/required、Kiro current/history 长度；删除客户正文、邮箱、Key、Authorization、工具参数值和真实 trace ID。
- [ ] fixture 使用固定假 ID、假路径和假文本，保留触发错误所需的最小结构。
- [ ] 扫描 fixture，禁止出现 `ksk_`/`csk_` 长 Key、Bearer、Cookie、真实邮箱和服务器管理 Key。
- [ ] 记录基线：Rust `885/885`，Admin UI `57/57`、148 assertions、生产构建 2579 modules。

Run:

```powershell
rg -n "ksk_[A-Za-z0-9]{20,}|csk_[A-Za-z0-9]{20,}|Bearer [A-Za-z0-9_-]{20,}|@[A-Za-z0-9.-]+" tests/fixtures
cargo test -j 2 --bin kiro-rs --locked --no-default-features
Set-Location admin-ui
bun test
bun run build
```

Expected: 密钥扫描无命中；现有测试全部通过。

## 4. Task 1：恢复 New API 可见的 1 秒首响应

**Existing spec:** `docs/superpowers/specs/2026-07-11-early-stream-handshake-design.md`

**Existing plan:** `docs/superpowers/plans/2026-07-11-new-api-visible-heartbeat.md`

**Files:**

- Modify: `src/anthropic/handlers.rs`
- Verify: `src/openai/handlers.rs`

- [ ] RED：把静默等待测试改为首项立即等于 `: connected\n\n`，约 1 秒后等于 `event: ping\ndata: {"type":"ping"}\n\n`；当前代码必须失败。
- [ ] GREEN：恢复 `PendingCallEvent::Comment`、立即 connected、1 秒 interval 和标准 Anthropic ping data event。
- [ ] 上游成功后首个正式协议事件仍为 `message_start`；上游失败只发脱敏 `event:error`，不伪造 `message_start`。
- [ ] 客户断开时 provider future 必须随 body stream drop 取消，不启动脱离请求生命周期的后台任务。
- [ ] OpenAI Chat/Responses 转换器忽略 comment 和 ping，不把它们转成正文。
- [ ] `first_token_ms` 继续只记录真实内容；New API 的首响应口径允许把 1 秒 ping 记为首字。

Run:

```powershell
cargo test anthropic::handlers::tests::pending_call_stream_emits_connected_then_ping -- --exact
cargo test openai::handlers::tests::openai_stream_parsers_ignore_anthropic_handshake_and_ping -- --exact
cargo test -j 2 --bin kiro-rs --locked --no-default-features
```

Acceptance:

- `earlyStreamHandshake=true` 时第一个 body 字节立即产生。
- 上游超过 1 秒未完成时，New API 在 1 秒左右收到至少一个 `data:` 行。
- 本任务明确优先客户感知速度，不再要求通过“message_start 前无事件”的严格检测。

客户影响：启用后上游错误发生在 HTTP 200 已提交之后，只能通过 SSE error 返回；普通非流式请求不变。

## 5. Task 2：API Key、nickname 与 Region/Host 后端

**Canonical plan:** `docs/superpowers/plans/2026-07-14-api-key-batch-region-model-routing.md`

本总计划不复制其 560 行细节，以该计划的 Task 4、6、7、8、9、10 为准，增加以下集成约束：

- [ ] API Key 的 Auth Region 固定 `us-east-1`；API Region 只接受 `us-east-1` / `eu-central-1`。
- [ ] API Key 模型、用量、偏好和生成请求全部使用显式 API Region。
- [ ] EU CodeWhisperer/REST host 使用 `q.eu-central-1.amazonaws.com`，不得构造 `codewhisperer.eu-central-1.amazonaws.com`。
- [ ] API Key 使用当前 Kiro 版本；OAuth/SSO 的旧版本兼容路径不被顺带改写。
- [ ] 模型响应显示实际 Region、Host、版本，不伪造 Claude/GPT 模型名单。
- [ ] `nickname` 与真实 `email` 分离；刷新邮箱不能覆盖 nickname。
- [ ] 完整 Key 不进入日志、错误、trace、fixture、测试输出和 Git diff。

客户影响：新账号的实际区域选择和可见模型可能改变；普通已正确配置账号、对话协议、SSE、缓存计费不受影响。

## 6. Task 3：API Key 文本导入与管理端 UI

**Files:**

- Create: `admin-ui/src/lib/api-key-import.ts`
- Create: `admin-ui/src/lib/api-key-import.test.ts`
- Modify: `admin-ui/src/components/batch-import-dialog.tsx`
- Modify: `admin-ui/src/components/add-credential-dialog.tsx`
- Modify: `admin-ui/src/components/credential-card.tsx`
- Modify: `admin-ui/src/components/available-models-dialog.tsx`
- Modify: `admin-ui/src/types/api.ts`

- [ ] 支持 `nickname | ksk_key`，使用批次 API Region。
- [ ] 支持 `nickname | ksk_key | apiRegion`，第三列覆盖批次 Region。
- [ ] 空行和 `#` 注释忽略；非法列数、空 nickname、非 `ksk_`、重复 Key、非法 Region 逐行报错。
- [ ] 错误、预览和 toast 只显示 Key 掩码，不保留原始整行。
- [ ] 文本模式复用现有批量 SSE 导入、统一代理、RPM、分组、验活和回滚逻辑；JSON/KAM 模式不回归。
- [ ] 单条 API Key 模式显示只读 Auth Region，强制选择 API Region，增加 nickname。
- [ ] 卡片显示优先级为 `nickname > email > #id`，并展示 Auth/API Region。

Run:

```powershell
Set-Location admin-ui
bun test src/lib/api-key-import.test.ts
bun test
bun run build
```

## 7. Task 4：Claude Code 内置工具 Schema 漂移修复

**Files:**

- Modify: `src/anthropic/converter.rs`
- Test: `src/anthropic/converter.rs`

### 4A. `Read.pages` 历史兼容

- [ ] RED：现有 `cc_outbound_read_pages_errors` 改为“合法 pages 历史不得让下一轮 400”，确认当前代码失败。
- [ ] 不把 PDF 页码映射为文本行号。
- [ ] 最小热修只处理已经由客户端执行完成的历史：保留 `path`，将合法且有界的 pages 记入 `explanation`，不再返回 `UnsupportedToolMapping`。
- [ ] pages 为 null/缺失时行为不变；非字符串、超长或异常 pages 不产生无界提示注入，也不能毒死后续会话。
- [ ] 完整两轮 fixture：assistant `Read{file_path,pages}` + user tool_result + 新 user，转换后 ID 配对且 provider 调用次数为 1。
- [ ] Raw 模式继续原样保留 pages。

第二阶段可选：只有 8991 证明 Kiro 接受扩展内置 Schema 后，才双向增加 optional `pages`，让未来工具调用也支持 PDF 页范围。未验证前不直接进生产。

### 4B. `Grep.case_sensitive`

- [ ] RED：Kiro `grep_search{caseSensitive:false}` 还原为 Claude Code `Grep` 后，当前 Schema 校验必须复现 `unexpected property $.case_sensitive`。
- [ ] 按当前 Claude Code Schema 映射：`caseSensitive=false` → `"-i": true`；`caseSensitive=true` → 省略 `-i` 或输出 Schema 允许的 false，不再生成 `case_sensitive`。
- [ ] 保留 `query→pattern`、`includePattern→glob`；禁止输出客户 Schema 未声明字段。

### 4C. 八个内置工具契约审计

- [ ] 对 Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch 建表比较当前 Claude Code Schema、Kiro Schema和双向映射。
- [ ] 每个映射执行“客户端合法输入 → Kiro → 客户端”并用原始客户 Schema 再验证。
- [ ] 只修有 fixture 证明的字段漂移，不删除客户业务参数来凑 Schema。

客户影响：修复后 PDF Read 历史不再永久阻断会话；Grep 不再因 RS 自己生成非法字段失败。普通文本 Read、Write/Edit/Bash 和首字链路不变。

## 8. Task 5：`read_file` 缺少 path 的诊断与一次有效重试

**Files:**

- Modify: `src/anthropic/tool_schema.rs`
- Modify: `src/anthropic/tool_attempt.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/error_snapshot.rs`
- Test: corresponding Rust test modules

### 5A. 先补证据

- [ ] 错误快照为非法工具调用保存脱敏摘要：工具名、input keys、值类型、violations、attempt；禁止保存路径值、命令正文和完整 JSON。
- [ ] 修复 invalid UTF-8 stream tail 只剩 hash、无法判断工具入参的问题；保留有界原始尾部或解析后的安全事件摘要。
- [ ] 用 `read_file_missing_path.json` 精确复现 input `{}` 或缺 path 的实际形状，再进入修复。

### 5B. 让第二次请求与第一次不同

当前 `run_with_single_retry` 在工具 Schema 失败后基本复用原请求，因此模型容易重复同一错误。改为：

- [ ] 只有首轮未交付正文/工具且错误为 `MissingRequired` 时才重试。
- [ ] 重试体只修改失败工具的结构化 description，追加有界说明：上一轮遗漏 required 字段，必须严格满足 inputSchema；允许列出 Schema 已公开的缺失字段名。
- [ ] 不从 prompt 猜 path，不填空字符串、零值或伪造文件路径。
- [ ] 第二轮仍非法时保持明确 `upstream_tool_schema_error`，不能把非法工具交给客户端执行。
- [ ] 记录两次尝试的脱敏 input keys，证明第二次请求确实使用了加固后的工具描述。

客户影响：只在上游首次生成非法工具且尚未交付语义内容时增加一次重试和少量 retry-only Token；合法工具、普通对话和缓存计费口径不变。

## 9. Task 6：空 user 导致 `REQUEST_BODY_INVALID`

**Files:**

- Modify: `src/anthropic/converter.rs`
- Modify: `src/model/config.rs`
- Modify: `config.example.json`
- Test: `src/anthropic/converter.rs`

- [ ] RED：fixture 必须复现“system 有内容、唯一 user text 为空 → Kiro history 有 system，但 currentMessage content 为空”。
- [ ] 在 provider 调用前增加 Kiro 请求不变量：current user content 为空且没有图片、文档、tool_result 或其他可执行 block 时，不允许把空 currentMessage 发给上游。
- [ ] 新增 `emptyUserMessageCompat`，缺省 `false`；关闭时返回清晰本地 `invalid_request_error`，不再消耗凭据并产生模糊上游 400。
- [ ] 开启时，仅对“单轮、无工具、无文档/图片、system 非空、user 仅为空 text”的精确形状使用最小 `Continue.` 作为当前消息；不得复制 system、不得影响多轮/tool_result 请求。
- [ ] 8991 与生产配置可显式开启；trace 记录 `empty_user_compat_applied=true`，便于评估是否应长期保留。

客户影响：关闭时错误更快、更清晰但仍为 400；开启时原本失败的 system-only 调用会产生模型响应，并增加一个最小输入 Token。正常非空对话不受影响。

## 10. Task 7：剩余错误快照分流，而不是盲目改代码

**Files:**

- Create: `docs/2026-07-14-error-snapshot-census.md`
- Modify: diagnostics/tests only after root cause is proven

- [ ] 将 `transient` 按 final_status/recovered 分开，避免把已恢复重试统计成客户失败。
- [ ] `auth_failed / invalid bearer` 归为账号健康与自动禁用，不改对话转换。
- [ ] `MODEL_NOT_AVAILABLE` 与 API Key/Region 任务联动，按 credential+resolvedRegion+host 验证。
- [ ] 对 200 次 `no assistant content` 抽样至少 20 个快照，区分上游空帧、解析丢帧、重试同账号和客户端取消。
- [ ] 对 53 次 `stream_read_error` 按 endpoint、proxy、credential、duration 聚类；没有 RS 解码证据时不得伪称已修复。
- [ ] 只把有稳定 fixture 和可重复根因的问题升级为新实现任务。

## 11. Task 8：修复当前 master 已知的两个集成阻塞项

**Files:**

- Modify: `src/kiro/token_manager.rs`
- Modify: `src/admin/service.rs`
- Modify: `admin-ui/src/lib/rpm-operations.ts`
- Test: corresponding Rust/TypeScript tests

- [ ] 批量 RPM 持久化失败后，同值重试必须再次尝试写盘，不能因 `updated=0` 返回假成功；采用 dirty 状态或失败回滚，并写失败→重试测试。
- [ ] `sourceChannel` 增加严格字符/字节上限，前后端一致；防止 10,000 账号逐个克隆超长字符串导致内存放大/OOM。

这两项与截图无关，但已经进入本地 master；合并本计划的其他分支前必须解决，避免把已知 Important 带入新版本。

## 12. Task 9：集成、8991 验收与发布门禁

- [ ] 每条分支先 rebase/merge 最新本地主集成分支，解决冲突后重新跑本分支测试。
- [ ] 先合并 SSE；再合并 API 后端和 UI；再合并工具兼容、Schema 重试和空请求；最后合并 RPM 安全修复。
- [ ] 合并后扫描完整 Key、Authorization、Cookie、真实邮箱和临时快照文件。
- [ ] 完整验证：

```powershell
cargo fmt -- --check
cargo test -j 2 --bin kiro-rs --locked --no-default-features
cargo check --all-targets --locked --no-default-features
Set-Location admin-ui
bun test
bun run build
Set-Location ..
git diff --check
git status --short
```

- [ ] 部署隔离公网 8991，保持 8990 不动。
- [ ] New API 首响应测试至少 30 次：上游慢于 1 秒时，首个 `data:` p95 ≤ 1.3 秒；事件顺序为 connected → ping → message_start。
- [ ] `Read.pages` 历史续轮至少 20 次，0 次本地 `request_conversion_error`。
- [ ] Grep `-i`、Read 普通文本、Write/Edit/Bash 各完成真实工具往返。
- [ ] `read_file` 缺 path fixture 证明第二次请求已加固；仍非法时客户端收到明确错误且工具未执行。
- [ ] system-only 空 user 在开关关闭/开启两种模式分别符合设计。
- [ ] API Key 美国区/EU 区各验证模型、用量、非流式、流式和实际 Host。
- [ ] 对比部署前后错误快照 30 分钟窗口，分别统计四类图片错误，不用总失败数掩盖分项回归。

## 13. 完成定义

只有同时满足以下条件才算完成：

1. New API 在启用 early handshake 时约 1 秒内收到可见 `data:` ping。
2. API Key 文本批量导入、nickname、Auth/API Region 和 EU Host 路由完整可用。
3. `Read.pages` 历史不再永久毒死会话，且没有错误地当作文本行号。
4. Grep 不再收到 RS 自己生成的 `case_sensitive` 非法字段。
5. 非法 `read_file` 不会执行；重试具有真实差异，并留下可定位的脱敏证据。
6. 空 currentMessage 不再发送给 Kiro；兼容行为必须受配置控制。
7. RPM 写盘重试和超长 sourceChannel 两个 Important 已关闭。
8. Rust、前端测试、全目标 check、格式和生产构建全部通过。
9. 8991 实网验收通过后才允许讨论升级生产 8990。
10. 没有完整 Key、Authorization、客户正文或服务器管理凭据进入 Git 历史。
