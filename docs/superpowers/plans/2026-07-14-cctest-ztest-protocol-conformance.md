# CCTest/Ztest 协议兼容优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在不硬编码检测 nonce、不伪造任意客户参数、不破坏现有缓存与普通对话的前提下，修复 CCTest/Ztest 暴露的工具 Schema/值、结构化输出、流式事件、签名、WebSearch、文本型 PDF 与协议指纹问题。

**Architecture:** 先在隔离 8991 增加可关闭的协议取证与本地回放，把检测请求、Kiro 原始事件和客户端响应固化为脱敏 fixture；随后引入独立的“响应契约层”，在任何内容交付客户端前统一验证工具 Schema、结构化 JSON、SSE 状态机和 thinking 签名。对 Schema 明确给出的 `const`/单值 `enum` 允许确定性修复，其余错误只允许在尚未输出语义内容时安全重试一次。

**Tech Stack:** Rust 2024、Axum、Serde/serde_json、Tokio、现有 Kiro event-stream 解析器、`pdf-extract`、Docker BuildKit、`anthropic_probe`、CCTest API。

---

## 0. 已确认基线

### 0.1 当前代码与环境

- 工作分支：`fix/ztest-d3-d7`
- 当前提交：`c53ff61`（`fix(tool): 避免Schema内容注入工具描述`）
- 公开测试实例：`https://rs-test.43-225-196-10.sslip.io`
- 当前测试镜像：`kiro-rs-test:c53ff61b4183`
- 生产 8990 不在本计划操作范围内。

### 0.2 CCTest 证据

- 报告：`https://cctest.ai/zh/result/2d73893a-773e-43d4-8bcc-e834ac44553c`
- 公开结果 API：`https://cctest.ai/api/check/2d73893a-773e-43d4-8bcc-e834ac44553c`
- 官方 API 文档：`https://cctest.ai/zh/docs`

| 维度 | 得分 | 状态 | 本计划处理 |
|---|---:|---|---|
| `tag_check` | 10/10 | 通过 | 回归保护 |
| `stream_structure` | 0/10 | 失败 | Task 4 |
| `non_stream` | 5/5 | 通过 | 回归保护 |
| `websearch` | 0/10 | 失败 | Task 6 |
| `signature_proto` | 0/10 | 失败 | Task 5 |
| `output_config` | 0/10 | 失败 | Task 3 |
| `server_tool` | 0/10 | 失败 | Task 2 |
| `token_inject` | 10/10 | 通过 | 禁止回归 |
| `knowledge` | 5/5 | 通过 | 禁止回归 |
| `doc_recognition` | 0/5 | 失败 | Task 7 |
| `image_recognition` | 5/5 | 通过 | 禁止回归 |
| `fingerprint` | 0/10 | 失败 | Task 8 |

Token 审计当前正常：总成本倍率 `0.8x`、缓存命中率 `90%`、`anomalies=[]`。本轮不得修改缓存命中整形、计费拆分或 Token 注入逻辑。

### 0.3 工具调用额外证据

现有检测显示：

- 调用：通过；
- 工具名：通过；
- JSON 语法：通过；
- Schema：失败；
- 值：失败。

当前 `validate_tool_choice_content()` 仅检查工具是否存在、名称和并行数量，不校验 `input` 是否满足客户端原始 `input_schema`。因此“合法 JSON”仍可能是业务上不可执行的错误参数。

### 0.4 正式端工具续轮 400 证据（P0）

正式 8990 当前镜像 `ghcr.io/3370842391/kiro-rs:sha-218ae0` 在 2026-07-13 16:58:15-16:58:26 UTC 内记录了 12 次同类失败：全部使用入口 Key 1、模型 `claude-sonnet-4-6`、IDE endpoint，分布在凭据 37/40/42/44/46，耗时仅 132-200 ms，统一返回：

```json
{"message":"Invalid tool use format.","reason":"REQUEST_BODY_INVALID"}
```

不同凭据收到完全相同的请求级 400，证明它不是单账号、限流或网络问题。正式容器使用 `RUST_LOG=debug`，但每次请求打印完整 JSON、HTTP/2 帧和 Authorization，50 MiB × 3 的 Docker 日志在数分钟内轮转，原始失败请求已经丢失；后续取证不得继续依赖全量 DEBUG。

已在正式端完成最小对照：

| 请求变体 | 结果 |
|---|---|
| 普通工具 Schema | HTTP 200 |
| property `const` | HTTP 200 |
| 单值 `enum` | HTTP 200 |
| `const` + 单值 `enum` | HTTP 200 |
| 合法 tool_use/tool_result 续轮（第二轮带 tools） | HTTP 200 |
| 合法 tool_use/tool_result 续轮（第二轮不带 tools） | HTTP 200 |
| 历史 ID `functions.get_weather:1` | HTTP 400，明确提示 ID 必须匹配 `^[a-zA-Z0-9_-]+$` |
| 历史 ID `tool/get_weather/1` | HTTP 400，精确复现 `Invalid tool use format / REQUEST_BODY_INVALID` |

GitHub 同类问题也确认 Claude Agent SDK/session resume 会产生带 `.`、`:` 等字符的历史 `tool_use.id`。当前 `convert_assistant_message()` 与 `ToolResult` 原样透传客户端 ID，缺少成对合法化。由于 00:58 的原始请求体已被轮转，实施时仍须先用 Task 1 的脱敏取证捕获一次真实失败；但“非法历史 ID 可稳定制造同一 400”已经是必须独立修复的客户兼容缺陷。

## 1. 方案选择

### 方案 A：响应契约层 + 有界修复/重试（推荐）

在转换阶段保存客户端原始契约，在响应交付前验证；只修复 Schema 中完全确定的固定值，其余错误安全重试一次。优点是同时服务真实客户和检测站，避免把 detector prompt 写进系统提示词。代价是 required tool、structured output 请求需要缓冲到完整响应后再交付，首个可见事件会稍晚。

### 方案 B：按检测 prompt 硬编码回答

开发最快，但会误伤普通用户、被检测站换 nonce 后立即失效，并继续增加隐藏 Token。拒绝采用。

### 方案 C：完整模拟 Anthropic 官方服务

包括伪造 thinking 签名、WebSearch encrypted content、事件 ID 与渠道指纹。短期改动巨大，且无法真正生成 Anthropic 的密码学签名。只在方案 A 完成后，对仍无法通过的单项做受限兼容，不作为首轮方案。

## 2. 文件职责规划

### 新增

- `src/anthropic/protocol_capture.rs`：仅测试环境开启的脱敏协议取证，记录请求形状、上游事件和出站协议，不记录 API Key。
- `src/anthropic/tool_history.rs`：验证历史工具 ID、为非法 ID 建立请求内稳定映射，并同步改写对应 tool_result ID。
- `src/anthropic/tool_schema.rs`：工具参数 Schema 验证、固定值修复与错误分类。
- `src/anthropic/structured_output.rs`：`output_config.format` 解析后的 JSON 提取、Schema 验证和一次恢复。
- `src/anthropic/thinking_signature.rs`：原生签名优先、合成签名生成与回传校验。
- `src/anthropic/testdata/cctest/`：从 8991 抓取并脱敏的 CCTest 请求/响应 fixture。
- `scripts/cctest-check.ps1`：使用环境变量启动 CCTest、轮询结果并输出维度差异；不保存任何 Key。

### 修改

- `src/anthropic/mod.rs`：注册上述模块。
- `src/anthropic/types.rs`：完整解析 `output_config.format`，继续兼容现有 `effort`。
- `src/anthropic/converter.rs`：在 `ConversionResult` 中保留工具契约与结构化输出契约。
- `src/anthropic/handlers.rs`：非流/缓冲流统一执行契约验证与安全重试。
- `src/anthropic/stream.rs`：工具完成前验证、严格 SSE 状态机、thinking 签名输出。
- `src/anthropic/websearch.rs`：按 fixture 修正原生 WebSearch 块、usage 与事件顺序。
- `src/anthropic/document.rs`：扩大文本型 PDF 的安全识别范围，仍不支持扫描件 OCR。
- `src/bin/anthropic_probe.rs`：增加 CCTest 等价黑盒探针和原始 SSE 字节校验。
- `Cargo.toml` / `Cargo.lock`：仅 Task 5 需要加入 `hmac`，其他任务不新增依赖。

---

### Task 1: 建立 CCTest 取证与本地回放基线

**Files:**
- Create: `src/anthropic/protocol_capture.rs`
- Create: `src/anthropic/testdata/cctest/README.md`
- Create: `scripts/cctest-check.ps1`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 写失败测试，证明现有探针忽略了 SSE 前导注释和 ping**

在 `src/bin/anthropic_probe.rs` 增加原始字节测试：

```rust
#[test]
fn strict_wire_probe_rejects_events_before_message_start() {
    let wire = concat!(
        ": connected\n\n",
        "event: ping\ndata: {\"type\":\"ping\"}\n\n",
        "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
    );
    assert!(classify_strict_anthropic_wire(wire).is_err());
}
```

- [ ] **Step 2: 运行测试并确认 RED**

Run:

```bash
cargo test --bin anthropic_probe strict_wire_probe_rejects_events_before_message_start -- --nocapture
```

Expected: FAIL，因为当前探针只解析 `data:` JSON，忽略 wire 上的前导事件顺序。

- [ ] **Step 3: 实现可关闭的协议取证器**

`protocol_capture.rs` 定义：

```rust
pub(crate) struct ProtocolCapture {
    root: std::path::PathBuf,
    request_id: String,
}

impl ProtocolCapture {
    pub(crate) fn from_env(request_id: &str) -> Option<Self>;
    pub(crate) fn inbound(&self, request: &MessagesRequest);
    pub(crate) fn outbound_kiro(&self, body: &str);
    pub(crate) fn upstream_event(&self, event: &crate::kiro::model::events::Event);
    pub(crate) fn outbound_sse(&self, event: &super::stream::SseEvent);
}
```

启用条件必须是 `KIRO_RS_PROTOCOL_CAPTURE_DIR` 非空；默认完全关闭。单个文件最大 2 MiB，字段名保留，PDF/image base64、文档正文、工具字符串值替换为长度与 SHA-256，HTTP Authorization/x-api-key 永不进入文件。

- [ ] **Step 4: 为取证脱敏写测试**

断言：

```rust
assert!(!captured.contains("csk_"));
assert!(!captured.contains("secret customer document"));
assert!(captured.contains("\"sha256\""));
assert!(captured.contains("\"input_schema\""));
```

- [ ] **Step 5: 增加 CCTest 启动/轮询脚本**

`scripts/cctest-check.ps1` 只读取环境变量：

```powershell
$headers = @{ Authorization = "Bearer $env:CCTEST_API_KEY" }
$body = @{
  url = $env:CCTEST_TARGET_URL
  apiKey = $env:CCTEST_TARGET_API_KEY
  model = $env:CCTEST_MODEL
  checkTokenUsage = $true
  concurrency = 1
} | ConvertTo-Json
```

POST `/api/v1/check` 后每 2 秒轮询 `/api/v1/check/:taskId`，最终只打印 `taskId`、`total`、`verdictKey` 和 `scores`，不得回显两个 Key。

- [ ] **Step 6: 在 8991 运行一次取证并固化 fixture**

将抓到的数据脱敏后保存为：

```text
src/anthropic/testdata/cctest/tool_request.json
src/anthropic/testdata/cctest/tool_upstream_events.jsonl
src/anthropic/testdata/cctest/tool_response.json
src/anthropic/testdata/cctest/stream_response.sse
src/anthropic/testdata/cctest/output_config_request.json
src/anthropic/testdata/cctest/websearch_request.json
src/anthropic/testdata/cctest/document_request.json
```

Fixture 中不得出现 API Key、完整 PDF/base64、客户正文或凭据 ID。

- [ ] **Step 7: 验证并提交**

Run:

```bash
cargo test --bin anthropic_probe --quiet
cargo test protocol_capture --quiet
git diff --check
```

Commit:

```bash
git add -- src/anthropic/protocol_capture.rs src/anthropic/testdata/cctest src/anthropic/mod.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs scripts/cctest-check.ps1
git commit -m "test(protocol): 增加CCTest协议取证与回放"
```

---

### Task 2: 修复工具调用 Schema 与值校验

**Files:**
- Create: `src/anthropic/tool_history.rs`
- Create: `src/anthropic/tool_schema.rs`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/converter.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/stream.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 为非法历史工具 ID 写失败测试**

在 `tool_history.rs` 固化正式端复现：

```rust
#[test]
fn remaps_invalid_tool_use_id_and_matching_result_as_one_pair() {
    let mut history = history_with_tool_pair(
        "functions.AskUserQuestion:1",
        "AskUserQuestion",
    );
    let mut current_results = Vec::new();

    let outcome = normalize_tool_history_ids(&mut history, &mut current_results).unwrap();
    let (tool_use_id, tool_result_id) = first_history_pair_ids(&history);

    assert_eq!(tool_use_id, tool_result_id);
    assert!(is_upstream_safe_tool_id(tool_use_id));
    assert!(outcome.changed());
}
```

再覆盖 `tool/get_weather/1`、空 ID、超长 ID、已经合法的 `tooluse_abc-123`、两个非法 ID 清洗后可能同形、重复 tool_use ID 和孤立 tool_result。重复 ID 必须 fail-closed，不得猜测 tool_result 属于哪个调用。

- [ ] **Step 2: 运行 ID 测试并确认 RED**

Run:

```bash
cargo test anthropic::tool_history::tests -- --nocapture
```

Expected: FAIL，因为当前 `convert_assistant_message()` 和 `process_message_content()` 原样透传客户端 ID。

- [ ] **Step 3: 实现请求内成对 ID 映射**

接口固定为：

```rust
pub(crate) fn is_upstream_safe_tool_id(id: &str) -> bool;

pub(crate) fn normalize_tool_history_ids(
    history: &mut [crate::kiro::model::requests::conversation::Message],
    current_results: &mut [crate::kiro::model::requests::tool::ToolResult],
) -> Result<ToolIdNormalization, ToolHistoryError>;
```

规则：

1. 非空、长度不超过 64 且每个字符均为 ASCII 字母/数字/`_`/`-` 时原样保留；
2. 其他 ID 映射为 `tooluse_` + 原 ID 的 SHA-256 前 40 位 hex，避免简单字符替换造成 `a:b`/`a.b` 碰撞；
3. 同一原 ID 在 assistant tool_use、历史 user tool_result、当前 tool_result 中必须使用同一映射；
4. 映射只存在于本次发往 Kiro 的请求，不改写客户输入文件、不改写已经发给客户端的历史响应；
5. 重复 tool_use ID、哈希碰撞或无法唯一配对时返回本地 `invalid_tool_history`，不得调用上游；
6. 在 `build_history()` 完成后、现有 `validate_tool_pairing()` 之前执行，使后续配对只看到安全 ID。

- [ ] **Step 4: 用正式端等价黑盒确认 ID 修复**

在 `anthropic_probe` 增加三消息续轮：user → assistant tool_use(`tool/get_weather/1`) → user tool_result(同 ID)。

Run:

```bash
cargo run --bin anthropic_probe -- --only invalid-tool-history-id
```

Expected before fix: HTTP 400 `REQUEST_BODY_INVALID`；Expected after fix: HTTP 200，最终正文正常结束且没有重复工具执行。

- [ ] **Step 5: 用真实 fixture 写 Schema 失败测试**

覆盖 CCTest 的五项判定：调用、名称、JSON、Schema、值。至少加入：

```rust
#[test]
fn cctest_tool_fixture_fails_schema_before_fix() {
    let contract = fixture_tool_contract();
    let mut input = fixture_upstream_tool_input();
    let result = validate_and_repair(&contract.schema, &mut input);
    assert_eq!(result, ToolInputOutcome::Valid);
    assert_eq!(input, contract.expected_input);
}
```

另加普通客户安全边界：非固定 required 字段缺失时不得生成猜测值。

- [ ] **Step 6: 运行 Schema 测试并确认 RED**

Run:

```bash
cargo test anthropic::tool_schema::tests -- --nocapture
```

Expected: FAIL，因为当前只验证工具名称，不验证参数契约。

- [ ] **Step 7: 实现独立工具契约类型**

```rust
#[derive(Debug, Clone)]
pub(crate) struct ToolContract {
    pub client_name: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolInputViolation {
    MissingRequired(String),
    TypeMismatch { path: String, expected: String },
    ConstMismatch { path: String },
    EnumMismatch { path: String },
    AdditionalProperty(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ToolInputOutcome {
    Valid,
    Repaired { paths: Vec<String> },
    Invalid { violations: Vec<ToolInputViolation> },
}
```

验证范围与现有规范化 Schema 一致：`type`、`properties`、`required`、`const`、`enum`、`items`、`additionalProperties`。不要支持已在 `normalize_json_schema()` 中剥离的顶层组合关键字。

- [ ] **Step 8: 实现严格的确定性修复规则**

只允许：

1. required 属性含 `const`：缺失或错误时写入该 `const`；
2. required 属性含单值 `enum`：缺失或错误时写入唯一值；
3. 递归对象/数组中的同类固定值修复。

禁止：从用户 prompt 抽取 nonce、把任意字符串转数字、为普通 required 字段填空串/零、删除模型生成的业务字段来“凑”检测。

- [ ] **Step 9: 在转换结果中保留原始工具契约**

给 `ConversionResult` 增加：

```rust
pub tool_contracts: std::collections::HashMap<String, ToolContract>,
```

Map key 使用还原后的客户端工具名；Schema 使用规范化后的真实结构，不把字段名和值复制到自然语言 description。

- [ ] **Step 10: 在工具事件交付前统一验证**

非流式在 `normalize_non_stream_content_blocks()` 之后、`validate_tool_choice_content()` 之前验证；流式在 `ToolJsonAccumulator` 完成 JSON 解析后、`CompletedToolUse` 发出前验证。

若仍存在非固定字段错误：

- 尚未发出 text/thinking/tool：原始请求安全重试一次；
- 已发出任何语义内容：发送 `upstream_tool_schema_error`，不得交付错误工具调用；
- 不得重试第二次，避免循环和重复工具执行。

- [ ] **Step 11: 扩展黑盒探针**

探针必须断言：

```text
tool_use.name == get_weather
input is object
input satisfies input_schema
input.city == expected city
input.unit == expected enum/const
input.nonce == expected fixture value
stop_reason == tool_use
```

- [ ] **Step 12: 运行测试并提交**

Run:

```bash
cargo test anthropic::tool_schema --quiet
cargo test anthropic::tool_history --quiet
cargo test anthropic::converter::tests --quiet
cargo test anthropic::stream::tests --quiet
cargo test anthropic::handlers::tests --quiet
cargo test --bin anthropic_probe --quiet
```

Commit:

```bash
git add -- src/anthropic/tool_history.rs src/anthropic/tool_schema.rs src/anthropic/mod.rs src/anthropic/converter.rs src/anthropic/handlers.rs src/anthropic/stream.rs src/bin/anthropic_probe.rs
git commit -m "fix(tool): 修复工具续轮ID与Schema契约"
```

**客户影响：** 合法工具 ID 完全不变；非法 ID 只在发往 Kiro 的请求副本中成对映射，因此此前直接 400 的 Claude Agent SDK/session resume 对话可以继续，正常对话不增加 Token、重试或首包延迟。普通 `tool_choice=auto` 且参数合法时无变化；required tool 在完整 Schema 校验前会缓冲，可能增加少量首包延迟；只有上游生成非法参数时才产生一次额外调用。不会执行错误工具，也不会从客户文本猜值。

---

### Task 3: 完整支持 `output_config.format` 结构化输出

**Files:**
- Create: `src/anthropic/structured_output.rs`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/types.rs`
- Modify: `src/anthropic/converter.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/exact_output.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 写反序列化失败测试**

目标请求：

```json
{
  "output_config": {
    "effort": "high",
    "format": {
      "type": "json_schema",
      "schema": {
        "type": "object",
        "properties": {"answer": {"type": "integer"}},
        "required": ["answer"],
        "additionalProperties": false
      }
    }
  }
}
```

断言解析后 `effort` 和 `format.schema` 同时存在。当前 `OutputConfig` 只有 `effort`，测试应先失败。

- [ ] **Step 2: 扩展类型但不向 Kiro 下发未知字段**

```rust
#[derive(Debug, Deserialize, Clone)]
pub struct OutputConfig {
    #[serde(default = "default_effort")]
    pub effort: String,
    #[serde(default)]
    pub format: Option<OutputFormat>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct OutputFormat {
    #[serde(rename = "type")]
    pub format_type: String,
    pub schema: serde_json::Value,
}
```

`AdditionalModelRequestFields` 仍只发送 Kiro 支持的 `effort`；`format` 由本地响应契约层处理。

- [ ] **Step 3: 实现结构化输出验证器**

复用 Task 2 的 Schema 基础验证，不允许 tool-only 字段。接口：

```rust
pub(crate) fn validate_output_json(
    text: &str,
    format: &OutputFormat,
) -> Result<serde_json::Value, StructuredOutputError>;
```

要求：恰好一个完整 JSON 值、无 Markdown fence、无前后解释、满足 Schema。

- [ ] **Step 4: 接入现有严格 JSON 一次恢复**

非流式和缓冲流：复用 `handlers.rs` 现有的 `recover_strict_json_attempts_with_validator()` 与 `exact_output.rs` 的 `append_strict_json_retry_instruction()`；把验证闭包从“只检查单个完整 JSON 值”扩展为 `validate_output_json()` 的 Schema 验证。第二次仍失败时返回 `upstream_structured_output_error`。

实时流在检测到 `output_config.format` 时自动切换为缓冲流，验证成功后再按标准 `message_start → text block → message_delta → message_stop` 发送。

- [ ] **Step 5: 写 RED/GREEN 回归**

覆盖：合法对象、缺 required、错误类型、多余字段、Markdown fence、两个 JSON 值、UTF-8 字符串、stream/non-stream 一致。

- [ ] **Step 6: 运行并提交**

Run:

```bash
cargo test anthropic::structured_output --quiet
cargo test strict_json --quiet
cargo test --bin anthropic_probe --quiet
```

Commit:

```bash
git add -- src/anthropic/structured_output.rs src/anthropic/mod.rs src/anthropic/types.rs src/anthropic/converter.rs src/anthropic/handlers.rs src/anthropic/exact_output.rs src/bin/anthropic_probe.rs
git commit -m "feat(output): 支持JSON Schema结构化输出"
```

**客户影响：** 仅带 `output_config.format` 的请求改为完整缓冲，首字延迟上升但不会再收到半截/非法 JSON；普通文本和仅 `effort` 请求保持原路径。

---

### Task 4: 收紧原始 SSE 流结构

**Files:**
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/stream.rs`
- Modify: `src/bin/anthropic_probe.rs`
- Test fixture: `src/anthropic/testdata/cctest/stream_response.sse`

- [ ] **Step 1: 写严格 wire 状态机**

状态机只接受：

```text
message_start
(content_block_start → content_block_delta* → content_block_stop)*
message_delta
message_stop
```

`ping` 只允许出现在 `message_start` 之后；块 index 必须唯一且 start/stop 配对；terminal 只能出现一次。

- [ ] **Step 2: 用当前 8991 fixture 确认 RED**

Expected failure 至少包含一种真实差异，例如：`event before message_start`、重复 terminal、usage 不一致或块顺序错误。必须以 fixture 为准，不先假定检测站一定反对 SSE comment。

- [ ] **Step 3: 移除标准 Anthropic 路径的前导 ping**

将 `EARLY_CONNECTED_SSE` / `EARLY_PING_SSE` 改为：

- 标准 `/v1/messages` 默认不在 `message_start` 前发送 `event: ping`；
- 若确需保活，只在已经发出 `message_start` 后发送 ping；
- 测试专用兼容开关必须默认关闭，不能让普通客户依赖非标准前导事件。

- [ ] **Step 4: 统一 usage 与 terminal**

断言 `message_start.message.usage.input_tokens` 与最终拆分口径一致；`message_delta.usage.output_tokens` 只含最终输出；每条成功流必须且只能以 `message_stop` 结束，错误流不得在成功 terminal 后追加 `error`。

- [ ] **Step 5: 运行并提交**

Run:

```bash
cargo test anthropic::stream::tests --quiet
cargo test early_stream --quiet
cargo test --bin anthropic_probe --quiet
```

Commit:

```bash
git add -- src/anthropic/handlers.rs src/anthropic/stream.rs src/bin/anthropic_probe.rs src/anthropic/testdata/cctest/stream_response.sse
git commit -m "fix(stream): 对齐Anthropic原始SSE事件顺序"
```

**客户影响：** 慢上游在 `message_start` 前不再收到 ping，连接首字可能稍晚；一旦开始响应，事件顺序更接近官方 SDK 预期，减少客户端解析分歧。

---

### Task 5: 修复 thinking 签名协议

**Files:**
- Create: `src/anthropic/thinking_signature.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/anthropic/mod.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/converter.rs`

- [ ] **Step 1: 写失败测试，禁止固定占位签名**

```rust
#[test]
fn two_thinking_blocks_do_not_share_a_fixed_placeholder() {
    let a = issue_signature("request-a", "thinking-a");
    let b = issue_signature("request-b", "thinking-b");
    assert_ne!(a, b);
    assert_ne!(a, "kiro-rs-thinking-signature");
}
```

- [ ] **Step 2: 原生签名优先**

若 Kiro event 带原生 signature，必须原样透传，流式 `signature_delta` 与非流 `thinking.signature` 使用同一个值。

- [ ] **Step 3: 为无原生签名生成请求级 opaque replay token**

加入 `hmac = "0.12"`，并使用标准库 `OnceLock<[u8; 16]>` 保存进程级 secret；首次调用时从仓库已经启用 `v4`/`fast-rng` feature 的 `uuid::Uuid::new_v4()` 取得 16 字节随机源，因此不再新增 `rand`/`getrandom` 依赖：

```text
krs1.<base64url(request_nonce)>.<base64url(HMAC-SHA256(thinking_hash || nonce))>
```

禁止固定常量；不得声称这是 Anthropic 官方密码学签名。下一轮客户端回传时仅验证格式/HMAC，converter 仍只把 thinking 正文发给 Kiro。

- [ ] **Step 4: 测试流/非流一致与回传**

覆盖 signature_delta、非流字段、同一块一致、多块不同、篡改拒绝、旧固定 placeholder 兼容读取但不再生成。

- [ ] **Step 5: 运行并提交**

Run:

```bash
cargo test thinking_signature --quiet
cargo test signature_delta --quiet
cargo test thinking --quiet
```

Commit:

```bash
git add -- Cargo.toml Cargo.lock src/anthropic/thinking_signature.rs src/anthropic/mod.rs src/anthropic/handlers.rs src/anthropic/stream.rs src/anthropic/converter.rs
git commit -m "fix(thinking): 使用请求级可回放签名"
```

**已知上限：** 如果 CCTest 验证的是 Anthropic 私钥产生的真实密码学签名，Kiro 上游无法提供时该 10 分不能诚实补齐。本任务只修复固定占位符和跨轮回放协议，不伪造官方私钥签名。

---

### Task 6: 对齐原生 WebSearch 协议块

**Files:**
- Modify: `src/anthropic/websearch.rs`
- Modify: `src/anthropic/websearch_loop.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`
- Test fixture: `src/anthropic/testdata/cctest/websearch_request.json`

- [ ] **Step 1: 按真实 fixture 写事件序列失败测试**

测试必须比较 block 类型、index、ID 关联、result 内容、usage 和 stop_reason，不只判断“包含 web_search 字样”。

- [ ] **Step 2: 去除非官方的搜索决策前言**

当前 `generate_websearch_events()` 在 server tool 前合成 `I'll search for ...` 文本块。若 fixture/官方基线没有该块，则删除；第一块直接为 `server_tool_use`。

- [ ] **Step 3: 修正 result 与 opaque 内容**

当前 `encrypted_content` 直接放明文 snippet。改为服务端 opaque blob，摘要文本单独用于最终回答；不得把 snippet 冒充官方密文。若 CCTest 要求官方可验证密文且无上游原始值，将该差异记录为能力上限，不硬编码固定密文。

- [ ] **Step 4: 对齐 usage 和 terminal**

确保：

```json
{
  "usage": {
    "output_tokens": 0,
    "server_tool_use": {"web_search_requests": 1}
  }
}
```

字段位置、事件顺序和最终 text/citation 以 fixture 为准；纯 native WebSearch 与 mixed-tools loop 分别测试。

- [ ] **Step 5: 运行并提交**

Run:

```bash
cargo test anthropic::websearch::tests --quiet
cargo test anthropic::websearch_loop::tests --quiet
cargo test --bin anthropic_probe websearch -- --nocapture
```

Commit:

```bash
git add -- src/anthropic/websearch.rs src/anthropic/websearch_loop.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs src/anthropic/testdata/cctest/websearch_request.json
git commit -m "fix(websearch): 对齐服务端搜索事件协议"
```

**客户影响：** 原生 WebSearch 的中间块会变化，但最终答案与搜索结果仍保留；普通客户工具不受影响。

---

### Task 7: 扩大文本型 PDF 的安全识别范围

**Files:**
- Modify: `src/anthropic/document.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/bin/anthropic_probe.rs`
- Test fixture: `src/anthropic/testdata/cctest/document_request.json`
- Test fixture: `src/anthropic/testdata/cctest/document.pdf.sha256.json`

- [ ] **Step 1: 用 CCTest PDF fixture 复现失败**

只保存 PDF 的 SHA-256、长度、页数和脱敏提取文本；若测试必须嵌入文件，确认文件不含客户数据且小于 100 KiB。

- [ ] **Step 2: 区分解析失败与问答匹配失败**

新增诊断断言：

```rust
let expansion = expand_pdf_documents(&mut request).await?;
assert!(expansion.extracted_text().contains(EXPECTED_TOKEN));
assert_eq!(expansion.deterministic_answer(&request), Some(EXPECTED_TOKEN.into()));
```

先确定是 `pdf_extract` 没拿到文本，还是 `detect_unique_identifier()` 因措辞/格式过窄没有短路。

- [ ] **Step 3: 扩大确定性识别但保持 fail-closed**

支持“只返回文档中唯一的精确标识/代码/短字符串”的中英文等价措辞；候选必须在当前轮文本型文档中唯一出现，长度 4-128，不能是普通自然语言句子。多个候选或模糊问题继续交给模型，不本地猜答案。

- [ ] **Step 4: 保留文档元数据与顺序**

扩展时保留 `title`/`context` 到 `[Document ...]` 头部；多文档仍按消息块顺序插入。扫描件继续返回明确“不支持 OCR”，本轮不引入 OCR 依赖。

- [ ] **Step 5: 运行并提交**

Run:

```bash
cargo test anthropic::document::tests --quiet
cargo test pdf_probe --bin anthropic_probe -- --nocapture
```

Commit:

```bash
git add -- src/anthropic/document.rs src/anthropic/handlers.rs src/bin/anthropic_probe.rs src/anthropic/testdata/cctest/document_request.json src/anthropic/testdata/cctest/document.pdf.sha256.json
git commit -m "fix(document): 扩大文本PDF确定性识别"
```

**客户影响：** 文本型 PDF 的标识提取更稳定；扫描 PDF 仍不支持。普通开放式 PDF 问答仍走模型，不被本地规则替代。

---

### Task 8: 收口协议指纹与渠道分类

**Files:**
- Modify: `src/anthropic/stream.rs`
- Modify: `src/anthropic/handlers.rs`
- Modify: `src/anthropic/types.rs`
- Modify: `src/bin/anthropic_probe.rs`

- [ ] **Step 1: 在 Task 2-7 完成后重新跑 CCTest**

若 `fingerprint` 已随流结构/签名/WebSearch 修复变为 10，则本任务只添加回归 fixture，不继续改协议。若仍为 0，再从取证中列出唯一差异。

- [ ] **Step 2: 增加严格协议矩阵**

覆盖：

- HTTP `content-type`、SSE 编码与错误 envelope；
- `message.id`、`tool_use.id` 非空且请求内唯一；
- 响应 `model` 与请求模型映射一致；
- `stop_reason` 与内容类型一致；
- stream/non-stream usage 口径一致；
- 未请求 thinking 时不出现 thinking/signature；
- 未请求 tools 时不出现 tool blocks。

- [ ] **Step 3: 只修真实差异**

禁止为“看起来像官方”而随机改变全部 ID 或 header。每个修改都必须由 CCTest fixture 或官方 Anthropic 行为基线支持，并有一个修改前失败、修改后通过的测试。

- [ ] **Step 4: 运行并提交**

Run:

```bash
cargo test protocol --quiet
cargo test --bin anthropic_probe --quiet
```

Commit:

```bash
git add -- src/anthropic/stream.rs src/anthropic/handlers.rs src/anthropic/types.rs src/bin/anthropic_probe.rs
git commit -m "fix(protocol): 收口Anthropic响应指纹"
```

---

### Task 9: 全量回归、一次部署与外部验收

**Files:**
- Modify: `docs/superpowers/plans/2026-07-14-cctest-ztest-protocol-conformance.md`（勾选执行结果）

- [ ] **Step 1: 服务器非部署测试**

先通过 BuildKit 缓存运行：

```bash
CARGO_PROFILE_TEST_DEBUG=0 \
CARGO_PROFILE_TEST_LTO=false \
CARGO_PROFILE_TEST_CODEGEN_UNITS=256 \
CARGO_INCREMENTAL=0 \
cargo test --quiet --locked --no-default-features -j 1
```

Expected: probe tests 与主程序 tests 均 0 failed。

- [ ] **Step 2: 运行本地黑盒矩阵**

必须通过：

```text
strict SSE wire
non-stream structure
required tool schema/value
tool result continuation
structured output stream/non-stream
thinking signature replay
native WebSearch
mixed WebSearch + client tools
text PDF identifier
image recognition
knowledge/profile probes
Token/cache accounting
```

- [ ] **Step 3: 只部署一次到 8991**

```bash
cd /opt/kiro-rs-test
./scripts/test-deploy.sh <verified-commit>
```

确认 `kiro-rs-admin` 生产容器和 8990 镜像未变化。

- [ ] **Step 4: CCTest 外部验收**

使用 `scripts/cctest-check.ps1`，`checkTokenUsage=true`、`concurrency=1`。目标：

| 阶段 | 最低目标 | 说明 |
|---|---:|---|
| Task 2-4 后 | 65 | 工具 + 输出 + 流结构各 +10 |
| Task 6-7 后 | 80 | WebSearch +10、文档 +5 |
| Task 5/8 可通过时 | 90-100 | 签名/协议各 +10；真实密码学签名可能是上游硬限制 |

Token 审计必须继续满足：`overallRatio <= 1.2`、`cacheHitRate >= 60`、`anomalies=[]`。

- [ ] **Step 5: Ztest 回归**

验证此前通过的 identity、cache、PDF、tool JSON 完整性、system、canary、UTF-8 与空响应重试不回归。不得因 CCTest 修复重新注入大型 system prompt。

- [ ] **Step 6: 最终复审与本地合并**

执行规格复审、代码质量复审和完整测试后，才合并 `fix/ztest-d3-d7` 回本地 master。除非用户明确要求，不推送 GitHub、不修改生产 8990。

---

## 3. 明确不做的事情

- 不硬编码 CCTest/Ztest nonce、城市、天气值或报告 UUID。
- 不从用户 prompt 猜工具参数。
- 不把客户 Schema 字段和值重新复制进工具 description。
- 不伪造 Anthropic 私钥签名；只能修复本地占位符和回传协议。
- 不修改当前已通过的缓存计费与 Token 审计口径。
- 不为文档识别引入 OCR；本轮只保证文本型 PDF。
- 不把调试取证默认带入生产，取证目录默认关闭且 fixture 必须脱敏。
- 不通过全量 `RUST_LOG=debug` 长期开启请求体和 Authorization 输出；工具失败只记录 ID 合法性、配对计数、Schema hash 与 trace ID。

## 4. 完成定义

只有同时满足以下条件才可宣称本轮完成：

1. 工具调用的名称、JSON、Schema、固定值全部通过，含非法历史 ID 的 tool_use/tool_result 续轮不再返回 400；
2. 结构化输出 stream/non-stream 均只返回一个符合 Schema 的 JSON；
3. 原始 SSE 严格状态机通过，无前导非标准 ping；
4. WebSearch、文本 PDF 和 thinking 回传不导致客户对话断裂；
5. CCTest 至少达到 80 分；若签名是上游密码学硬限制，文档中明确列出证据；
6. 原有 14 项 probe、787+ 主测试与新增测试全部 0 failed；
7. Token 审计、图片、知识库、非流结构和身份归一化无回归；
8. 只部署到 8991，生产 8990 未改变。
