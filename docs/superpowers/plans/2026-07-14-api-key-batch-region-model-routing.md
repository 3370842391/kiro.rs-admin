# Kiro API Key 批量导入、Region 与模型路由优化 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 `kiro.rs-admin` 中增加兼容 `账号标识 | ksk_xxx` 的批量 API Key 导入方式，固定 API Key 的 Auth Region 为 `us-east-1`，按订单指定的 API Region 路由模型、用量和生成请求，并修复 EU API Key 因错误区域或错误主机导致模型列表不完整的问题。

**Architecture:** 保留现有 JSON/KAM/OAuth/SSO 导入链路，新增独立的纯文本 API Key 解析器和批量导入 UI；账号标识保存为 `nickname`，不冒充邮箱。后端把认证区域、数据区域和服务主机解析拆成三个明确概念，并让 API Key 的模型/用量/生成请求全部以显式 `apiRegion` 为权威来源；OAuth/SSO 继续保留 profile ARN 与兼容回退逻辑。

**Tech Stack:** Rust 2024、Axum、Serde、Reqwest、Tokio、React 19、TypeScript 6、TanStack Query、Bun Test、Cargo Test。

---

## 1. 已确认的现状与根因

### 1.1 Go 端已有能力

参考项目 `D:\kiro2api\kirogo-api` 已实现：

- `config.Account` 同时保存 `kiroApiKey`、`authRegion`、`apiRegion` 和 `nickname`；
- API Key 作为 Bearer Token 使用，并发送 `tokentype: API_KEY`；
- Auth Region 与 API Region 分开解析；
- `ListAvailableModels`、`getUsageLimits` 和生成请求使用账号的 API Region；
- `us-east-1` 的 CodeWhisperer 服务使用 `codewhisperer.us-east-1.amazonaws.com`；
- 非 `us-east-1` 的 CodeWhisperer/REST 数据面改走 `q.{region}.amazonaws.com`，避免 `codewhisperer.eu-central-1.amazonaws.com` 的 DNS/公网不可用问题；
- 新账号添加后会主动刷新账号信息与模型列表。

关键参考文件：

- `D:\kiro2api\kirogo-api\config\config.go`
- `D:\kiro2api\kirogo-api\proxy\handler.go`
- `D:\kiro2api\kirogo-api\proxy\kiro_api.go`
- `D:\kiro2api\kirogo-api\proxy\kiro.go`
- `D:\kiro2api\kirogo-api\proxy\kiro_headers.go`
- `D:\kiro2api\kirogo-api\web\app.js`

### 1.2 Rust 端已有基础

`kiro.rs-admin` 已经具备：

- `KiroCredentials.kiro_api_key`；
- `auth_region` 与 `api_region` 字段；
- API Key 去重、掩码和 `tokentype: API_KEY`；
- 单条 API Key 添加；
- JSON/KAM 批量导入 API Key；
- 按凭据查询上游 `ListAvailableModels`；
- `us-east-1` / `eu-central-1` REST 回退逻辑。

因此本次不重建凭据系统，只补齐导入格式、Region 权威来源与服务主机规则。

### 1.3 当前缺口

1. 批量导入框只接受 JSON，不接受：

   ```text
   account-a | <ksk_xxx>
   account-b | <ksk_yyy>
   ```

2. Rust 端没有独立 `nickname`；若把左侧账号标识塞入 `email`，后续刷新真实邮箱时会覆盖，且字段语义错误。

3. `rest_api_region_candidates_for()` 当前优先级为“真实 profile ARN 区域 > Auth/SSO Region”，没有使用凭据显式 `api_region`。因此 API Key 配置为：

   ```text
   authRegion = us-east-1
   apiRegion  = eu-central-1
   ```

   时，模型和用量请求仍可能先访问美国区。

4. `ListAvailableModels` 固定使用旧版 `KiroIDE-0.9.2`，而 Go 端使用当前 Kiro 版本。旧版本是为无真实 profile ARN 的 OAuth/IdC 兼容而保留，不应无条件套用到 API Key。

5. Rust 的 `CodeWhispererEndpoint` 会直接构造 `codewhisperer.{api_region}.amazonaws.com`；在 `eu-central-1` 等非美国区应按 Go 端规则收敛到 `q.{api_region}.amazonaws.com`。

6. 模型查询成功时只返回模型数组，缺少“实际命中的 Region/Host”诊断信息，容易把地区配置问题误判成账号只支持 GPT。

## 2. 方案比较与采用结论

### 方案 A：仅在前端把文本转成现有 JSON

优点：改动最小。

缺点：只解决输入便利性，不解决 `apiRegion` 未被模型 REST 请求采用、EU 主机错误、旧 Kiro 版本和模型诊断问题。

### 方案 B：文本导入 + 统一 Region/Host 路由（采用）

优点：在复用现有凭据系统的前提下，同时修复导入、模型、用量和生成请求；可用单元测试完整覆盖，不需要硬编码 Claude 模型名单。

缺点：涉及前端解析、凭据 DTO、REST 请求和端点模块，需要一次完整回归。

### 方案 C：自动轮询两个 Region，选择“模型最多”的响应

优点：用户可以不填 Region。

缺点：与“订单指定 API Region 必须精确选择”的业务规则冲突；不同区域返回 200 不代表账号属于该区，按模型数量猜测会掩盖配置错误并增加上游请求。

**结论：采用方案 B。API Key 的 API Region 必须显式确定，不以模型数量自动猜区。**

## 3. 输入格式与产品规则

### 3.1 支持的文本格式

基础格式完全兼容用户给出的双列文本：

```text
account-a | <ksk_xxx>
account-b | <ksk_yyy>
```

扩展格式允许一批文本混合两个 API Region：

```text
account-a | <ksk_xxx> | us-east-1
account-b | <ksk_yyy> | eu-central-1
```

规则：

- 分隔符为半角 `|`，两侧空白自动裁剪；
- 空行忽略；
- `#` 开头的整行作为注释忽略；
- 第一列为 `nickname`，必填；
- 第二列为 Kiro API Key，必须以 `ksk_` 开头；
- 第三列可选，只允许 `us-east-1` 或 `eu-central-1`；
- 第三列缺失时使用批次级“API Region”选择；
- 批次级 API Region 默认不替用户猜测，提交前必须选择；
- API Key 的 Auth Region 固定为 `us-east-1`，UI 只读展示，不允许改成 API Region；
- 同一批次内重复 Key、系统已存在 Key、空 nickname、空 Key、非法 Region 都在预览阶段逐行标错；
- UI、日志、SSE 事件和错误信息都不得回显完整 Key。

### 3.2 单条添加同步收紧

单条“API Key”模式同步采用：

- `authRegion = us-east-1` 固定值；
- `apiRegion` 必须从 `us-east-1` / `eu-central-1` 选择；
- 增加可选 `nickname`；
- 后端仍做同样校验，不能只依赖前端。

### 3.3 兼容边界

- 现有 JSON、KAM、OAuth、IdC、Social、external_idp 导入行为不变；
- JSON 中已有 `authRegion` / `apiRegion` 时继续保留；
- 旧 API Key 凭据缺少 `apiRegion` 时不静默改库：卡片显示“API Region 未配置”，模型/用量查询返回可操作错误，引导用户编辑；
- 不把用户提供的真实 Key 写入测试、文档、日志或 Git 历史。

## 4. 文件职责与改动地图

### 新建

- `admin-ui/src/lib/api-key-import.ts`：纯文本解析、规范化、掩码和逐行错误模型。
- `admin-ui/src/lib/api-key-import.test.ts`：双列/三列/空白/注释/重复/非法 Key/非法 Region 测试。
- `src/kiro/region.rs`：统一 Auth Region、API Region、REST Host 和流式 Host 的纯函数规则。

### 修改

- `admin-ui/src/components/batch-import-dialog.tsx`：增加“JSON 导入 / API Key 文本导入”模式、批次 Region、预览和提交映射。
- `admin-ui/src/components/add-credential-dialog.tsx`：API Key 模式固定 Auth Region、强制选择 API Region、增加 nickname。
- `admin-ui/src/types/api.ts`：为请求和凭据状态增加 `nickname`，为模型响应增加诊断元数据。
- `admin-ui/src/components/credential-card.tsx`：显示优先级改为 `nickname > email > #id`，显示 Auth/API Region。
- `admin-ui/src/components/available-models-dialog.tsx`：展示模型响应实际使用的 Region/Host/版本。
- `src/kiro/model/credentials.rs`：持久化 `nickname`；保持 `email` 为真实邮箱；接入统一 Region helper。
- `src/admin/types.rs`：`AddCredentialRequest`、凭据状态、导入/导出 DTO 增加 `nickname`；模型响应增加诊断字段。
- `src/admin/service.rs`：API Key 参数校验、默认 Auth Region、强制 API Region、创建后模型验真。
- `src/kiro/token_manager.rs`：模型/用量请求按凭据类型选择 Region、Host 和 Kiro 版本；保留 OAuth 兼容策略。
- `src/kiro/endpoint/ide.rs`：复用统一 Host helper。
- `src/kiro/endpoint/codewhisperer.rs`：非 `us-east-1` 不再构造 `codewhisperer.{region}`。
- `src/kiro/endpoint/amazonq.rs`：复用统一 Host helper，避免规则分散。
- `src/kiro/endpoint/runtime.rs`：只复用 API Region 校验，不改变 `runtime.{region}.kiro.dev` 语义。
- `src/kiro/mod.rs`：导出 `region` 模块。
- `CHANGELOG.md`：记录新增格式、Region 规则、旧凭据迁移提示和模型诊断变化。
- `README.md`：增加批量格式及 Region 配置示例。

## 5. 核心接口设计

### 5.1 前端文本解析器

```ts
export type SupportedApiRegion = 'us-east-1' | 'eu-central-1'

export interface ParsedApiKeyLine {
  lineNumber: number
  nickname: string
  kiroApiKey: string
  apiRegion: SupportedApiRegion
}

export interface ApiKeyLineError {
  lineNumber: number
  maskedLine: string
  message: string
}

export function parseApiKeyLines(
  input: string,
  defaultApiRegion?: SupportedApiRegion
): { entries: ParsedApiKeyLine[]; errors: ApiKeyLineError[] }
```

解析器是纯函数，不直接操作 React 状态，不发网络请求，方便单测和后续 CLI 复用。

### 5.2 后端 Region/Host helper

```rust
pub const API_KEY_AUTH_REGION: &str = "us-east-1";

pub enum KiroService {
    Rest,
    Ide,
    CodeWhisperer,
    AmazonQ,
    Runtime,
}

pub fn validate_api_region(region: &str) -> anyhow::Result<&str>;
pub fn data_plane_host(service: KiroService, api_region: &str) -> anyhow::Result<String>;
pub fn rest_region_candidates(credentials: &KiroCredentials, config: &Config) -> Vec<String>;
```

主机规则：

| 服务 | `us-east-1` | `eu-central-1` |
|---|---|---|
| REST / CodeWhisperer | `codewhisperer.us-east-1.amazonaws.com` | `q.eu-central-1.amazonaws.com` |
| IDE / AmazonQ | `q.us-east-1.amazonaws.com` | `q.eu-central-1.amazonaws.com` |
| Runtime | `runtime.us-east-1.kiro.dev` | `runtime.eu-central-1.kiro.dev` |

候选规则：

- API Key：只使用显式 `credentials.api_region`；不因另一区返回 200 而自动换区；
- OAuth/SSO：真实 profile ARN 区域 > 显式 `api_region` > 现有 Auth/SSO Region 推断；保留 400/403 的兼容回退；
- API Key 缺少 `api_region`：返回配置错误，不回退到全局 Region；
- Auth Region 只负责 Token 刷新。API Key 不刷新 Token，但仍持久化固定值 `us-east-1` 以保持配置可读性。

### 5.3 模型查询策略

API Key 的 `ListAvailableModels`：

- 使用显式 API Region；
- 使用上述 REST Host 规则；
- 发送 `Authorization: Bearer <key>`；
- 发送 `tokentype: API_KEY`；
- 使用当前有效 Kiro 版本，而不是固定 `0.9.2`；
- 请求参数包含 `origin=AI_EDITOR&maxResults=50`；
- 不注入占位 profile ARN；
- 原样保留上游模型 ID，不按 `gpt-*` / `claude-*` 人为过滤；
- 响应附带 `resolvedApiRegion`、`resolvedHost`、`kiroVersion`，便于确认是否打到订单指定区域。

OAuth/SSO 的 `ListAvailableModels` 保留固定 `0.9.2` 的兼容路径，除非凭据已有真实 profile ARN 且已有测试证明当前版本可用。本次不把 API Key 修复扩大成 OAuth 行为重写。

## 6. 实施任务

### Task 1：建立现状回归基线

**Files:**

- Test: `src/kiro/model/credentials.rs`
- Test: `src/kiro/token_manager.rs`
- Test: `admin-ui/src/components/batch-import-dialog.tsx`

- [ ] 记录当前 Rust 与前端测试结果，确保后续失败可归因。

Run:

```powershell
cargo test
Set-Location admin-ui
bun test
bun run build
```

Expected: 所有现有测试通过；若存在既有失败，先记录测试名和错误，不在本功能中顺带重构无关模块。

### Task 2：先写纯文本解析器失败测试

**Files:**

- Create: `admin-ui/src/lib/api-key-import.test.ts`
- Create: `admin-ui/src/lib/api-key-import.ts`

- [ ] 覆盖双列格式，断言批次 Region 被写入每条结果。
- [ ] 覆盖三列格式，断言逐行 Region 覆盖批次 Region。
- [ ] 覆盖空行、注释和两侧空白。
- [ ] 覆盖缺 nickname、缺 Key、非 `ksk_` Key、非法 Region、额外列。
- [ ] 覆盖同批重复 Key，错误信息只包含掩码。
- [ ] 运行单测并确认在实现前失败。

Run:

```powershell
Set-Location admin-ui
bun test src/lib/api-key-import.test.ts
```

Expected: FAIL，原因是解析器尚未实现或行为不完整。

### Task 3：实现纯文本解析器

**Files:**

- Create: `admin-ui/src/lib/api-key-import.ts`
- Test: `admin-ui/src/lib/api-key-import.test.ts`

- [ ] 实现 `parseApiKeyLines()`、Region 白名单、Key 掩码和批内去重。
- [ ] 保证错误对象不持有完整 Key；需要定位时只保留行号和首尾掩码。
- [ ] 运行解析器测试并确认通过。

Run:

```powershell
Set-Location admin-ui
bun test src/lib/api-key-import.test.ts
```

Expected: PASS。

### Task 4：增加 nickname 全链路

**Files:**

- Modify: `src/kiro/model/credentials.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/service.rs`
- Modify: `src/kiro/token_manager.rs`
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/components/credential-card.tsx`

- [ ] 先写 Rust 序列化/反序列化测试，验证旧 `credentials.json` 无 nickname 时兼容，新数据 round-trip 不丢失。
- [ ] 为 `KiroCredentials`、添加请求、状态响应和导出 DTO 增加可选 `nickname`。
- [ ] 卡片显示顺序改为 `nickname > email > #id`。
- [ ] 真实邮箱刷新只更新 `email`，不覆盖 `nickname`。
- [ ] 导入/导出保留 nickname，兼容别名 `name`。

Run:

```powershell
cargo test kiro::model::credentials
cargo test admin::types
```

Expected: PASS，旧凭据文件无需迁移即可加载。

### Task 5：接入 API Key 文本批量导入 UI

**Files:**

- Modify: `admin-ui/src/components/batch-import-dialog.tsx`
- Modify: `admin-ui/src/types/api.ts`
- Test: `admin-ui/src/lib/api-key-import.test.ts`

- [ ] 在批量导入对话框增加“JSON/KAM”和“API Key 文本”两种模式。
- [ ] 文本模式增加只读 Auth Region=`us-east-1`、必选批次 API Region 和格式帮助。
- [ ] 解析后显示行号、nickname、Key 掩码、API Region 和错误状态。
- [ ] 将有效行映射为现有 `AddCredentialRequest`：

  ```ts
  {
    nickname,
    authMethod: 'api_key',
    kiroApiKey,
    authRegion: 'us-east-1',
    apiRegion,
    rpmLimit,
    groups,
    proxyUrl,
  }
  ```

- [ ] 继续复用现有 SSE 批量导入、去重、统一代理、RPM、分组、验活和直接导入逻辑。
- [ ] JSON 模式行为和 placeholder 不回归。

Run:

```powershell
Set-Location admin-ui
bun test
bun run build
```

Expected: PASS，TypeScript 无类型错误。

### Task 6：收紧单条 API Key 添加

**Files:**

- Modify: `admin-ui/src/components/add-credential-dialog.tsx`
- Modify: `src/admin/service.rs`
- Modify: `src/kiro/token_manager.rs`

- [ ] API Key 模式增加 nickname。
- [ ] Auth Region 固定为 `us-east-1`，不允许输入其他值。
- [ ] API Region 改为 `us-east-1` / `eu-central-1` 明确选择。
- [ ] 后端规范化空白，验证 `ksk_` 前缀、固定 Auth Region 和 API Region 白名单。
- [ ] 后端拒绝 `authMethod=api_key` 但缺少 `kiroApiKey` 或 `apiRegion` 的请求。
- [ ] 现有 hash 去重继续生效。

Run:

```powershell
cargo test add_credential
Set-Location admin-ui
bun run build
```

Expected: PASS；非法 Region 返回 400 类管理端错误，不写入凭据文件。

### Task 7：抽取统一 Region/Host 规则

**Files:**

- Create: `src/kiro/region.rs`
- Modify: `src/kiro/mod.rs`
- Modify: `src/kiro/model/credentials.rs`
- Modify: `src/kiro/endpoint/ide.rs`
- Modify: `src/kiro/endpoint/codewhisperer.rs`
- Modify: `src/kiro/endpoint/amazonq.rs`
- Modify: `src/kiro/endpoint/runtime.rs`

- [ ] 先写表驱动测试覆盖上方 Host 矩阵。
- [ ] 测试 API Key 缺少 apiRegion 时明确失败。
- [ ] 测试 API Key `authRegion=us-east-1/apiRegion=eu-central-1` 时所有数据面主机均为 EU。
- [ ] 测试 `CodeWhisperer + eu-central-1` 解析为 `q.eu-central-1.amazonaws.com`。
- [ ] 测试 OAuth profile ARN 区域仍高于显式/推断区域。
- [ ] 实现 helper 并替换各端点内重复字符串拼接。
- [ ] 对 429 fallback 链按最终 URL/Host 去重，避免 EU 下 IDE 与 CodeWhisperer 实际落到同一 `q.eu-central-1` 后重复请求。

Run:

```powershell
cargo test kiro::region
cargo test kiro::endpoint
```

Expected: PASS；代码中不再直接构造 `codewhisperer.eu-central-1.amazonaws.com`。

### Task 8：修复模型与用量 REST 路由

**Files:**

- Modify: `src/kiro/token_manager.rs`
- Modify: `src/kiro/model/available_models.rs`
- Modify: `src/admin/types.rs`
- Modify: `admin-ui/src/types/api.ts`
- Modify: `admin-ui/src/components/available-models-dialog.tsx`

- [ ] 为 `rest_region_candidates_for()` 写凭据类型分支测试：API Key 使用显式 apiRegion；OAuth 保留 profile ARN/兼容回退。
- [ ] 为 REST Host 写测试：美国区 CodeWhisperer host、EU 区 q host。
- [ ] 为 Header 策略写测试：API Key 必含 `tokentype: API_KEY`。
- [ ] 为版本策略写测试：API Key 使用当前有效 Kiro 版本；OAuth 无真实 profile ARN 时继续使用 `0.9.2`。
- [ ] `ListAvailableModels` 增加 `maxResults=50`。
- [ ] 模型响应增加 `resolvedApiRegion`、`resolvedHost`、`kiroVersion`，UI 显示这些信息。
- [ ] 不按模型名称过滤响应；“只有 GPT”作为诊断结果显示，而不是自动改写成 Claude。
- [ ] `getUsageLimits`、`setUserPreference` 使用同一 Region/Host helper，避免模型正确而余额仍跨区。

Run:

```powershell
cargo test available_models
cargo test rest_api_region
cargo test get_usage_limits
Set-Location admin-ui
bun test
bun run build
```

Expected: PASS；EU API Key 的模型和用量请求都显示 `eu-central-1` 与 EU host。

### Task 9：添加后模型验真与错误回传

**Files:**

- Modify: `src/admin/service.rs`
- Modify: `src/admin/types.rs`
- Modify: `src/admin/handlers.rs`
- Modify: `admin-ui/src/api/credentials.ts`
- Modify: `admin-ui/src/components/batch-import-dialog.tsx`

- [ ] “验活导入”对 API Key 同时验证用量接口和 `ListAvailableModels`；“直接导入”保持不发上游请求。
- [ ] 验活成功事件返回模型数量、实际 Region/Host，不返回模型完整列表和 Key。
- [ ] Region/Host/401/403/DNS 错误使用可操作文案区分。
- [ ] 模型数组为空或仅包含某一模型家族时不擅自判 Key 无效；UI 给出警告并展示实际路由信息。
- [ ] 验活失败沿用现有回滚语义，确保半成功凭据不会残留。

### Task 10：安全与可观测性回归

**Files:**

- Modify: `src/kiro/token_manager.rs`
- Modify: `src/admin/service.rs`
- Modify: `admin-ui/src/lib/api-key-import.ts`

- [ ] 日志只记录 credential ID、nickname、Region、Host、HTTP 状态和模型数量。
- [ ] 禁止记录 Authorization、完整 `kiroApiKey`、请求对象 Debug 输出和原始粘贴行。
- [ ] SSE 失败事件只返回行号、掩码、错误类型和可操作信息。
- [ ] 添加测试扫描错误字符串，确认不包含完整测试 Key。

### Task 11：文档与完整验证

**Files:**

- Modify: `README.md`
- Modify: `CHANGELOG.md`

- [ ] 文档写明 Auth Region 永远为 `us-east-1`。
- [ ] 文档写明 API Region 来自订单，只允许 `us-east-1` / `eu-central-1`，必须精确选择。
- [ ] 文档同时给出双列与三列格式，示例只使用假 Key。
- [ ] 写明旧 API Key 缺 apiRegion 的修复方式。
- [ ] 运行格式化、全量测试和生产构建。

Run:

```powershell
cargo fmt --check
cargo test
Set-Location admin-ui
bun test
bun run build
```

Expected: 全部通过。

## 7. 可选实网验收

实网测试不进入默认测试套件，使用环境变量注入临时 Key，且命令和日志不得打印变量值。

验收矩阵：

| Auth Region | API Region | 预期模型/用量 Host | 预期生成 Host |
|---|---|---|---|
| `us-east-1` | `us-east-1` | `codewhisperer.us-east-1.amazonaws.com` | `q.us-east-1.amazonaws.com` 或显式美国区端点 |
| `us-east-1` | `eu-central-1` | `q.eu-central-1.amazonaws.com` | `q.eu-central-1.amazonaws.com` / `runtime.eu-central-1.kiro.dev` |

每个测试账号验收：

1. 批量导入成功，nickname 正确；
2. 卡片显示 Auth Region 与 API Region；
3. 余额查询成功；
4. 可用模型弹窗显示实际 Region/Host/版本；
5. 选择上游明确返回的模型做一次非流式请求；
6. 同一模型做一次流式请求；
7. 请求日志中的端点与订单 Region 一致；
8. 日志、前端错误和导出预览不泄露完整 Key。

## 8. 完成定义

只有同时满足以下条件才算完成：

1. 可直接粘贴多行 `nickname | ksk_key` 并批量导入；
2. 支持批次 API Region，并支持第三列逐行覆盖；
3. API Key Auth Region 始终为 `us-east-1`；
4. API Key 模型、用量、偏好设置和生成请求全部使用订单指定 API Region；
5. EU 请求不会访问 `codewhisperer.eu-central-1.amazonaws.com`；
6. 模型列表展示真实上游结果与实际 Region/Host，不用静态 Claude/GPT 名单伪造；
7. OAuth/SSO/JSON/KAM 现有行为无回归；
8. nickname 与真实 email 分离保存；
9. 完整 Key 不进入日志、错误、测试、文档或 Git diff；
10. `cargo fmt --check`、`cargo test`、`bun test`、`bun run build` 全部通过。

## 9. 实施顺序建议

按 Task 1 → 11 顺序执行。Task 2/3 先把输入格式变成可测试的纯函数；Task 4/5 再接 UI；Task 7/8 是模型与地区问题的核心修复；Task 9 负责把错误尽早暴露给导入用户；最后统一做安全、文档和全量回归。
