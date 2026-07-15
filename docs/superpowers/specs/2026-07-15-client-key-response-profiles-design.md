# 客户端 Key 双回复模式设计

## 背景与目标

当前 RS 对所有客户端 Key 使用同一套 Anthropic/Claude 兼容行为。此前为协议兼容、客户稳定性和检测站一致性加入的逻辑混在同一条请求链中，导致希望直接使用 Kiro 原始身份与回答风格的客户也会看到 Claude/Anthropic 身份归一化和部分本地确定性回答。

本功能在客户端 Key 上增加固定的回复模式，让同一个 RS 实例可以同时服务两类客户：

- `detection`：保持当前已经上线的 Claude 兼容和检测优化行为；
- `kiro_native`：保留 RS 的协议桥接和可靠性修复，但关闭身份伪装以及检测探针专用的本地回复。

旧 Key 必须无感升级为 `detection`，不允许升级后突然改变现有客户的身份、工具、计费或对话行为。新建 Key 默认仍为 `detection`，管理员可在创建或编辑 Key 时显式选择 `kiro_native`。

## 非目标

本功能不提供一个绕过 RS、直接暴露 Kiro 私有 HTTP/event-stream 协议的端点。`kiro_native` 仍通过现有 Anthropic API 入口调用，因此必要的请求/响应协议转换继续存在。

本功能不做以下事项：

- 不增加客户端请求 Header、query 参数或模型名后缀来临时覆盖模式；
- 不允许同一个 Key 在单次会话中自动切换模式；
- 不拆分端口、容器、数据库或上游凭据池；
- 不改变 Key 绑定分组、RPM、模型映射、用量统计和计费规则；
- 不把全局 `ToolCompatibilityMode` 与回复模式合并；
- 不在第一版增加批量切换 Key 模式。

## 方案比较

### 方案 A：模式持久化到客户端 Key（采用）

每个 `ClientKey` 保存 `responseMode`，鉴权成功时把模式和 Key ID、分组一起注入不可变的请求上下文。所有需要区分行为的处理器只读取该上下文。

优点是隔离边界清楚、同一会话稳定、管理端直观，并且不需要客户修改 Base URL。缺点是所有检测专用入口都必须完成一次集中盘点和显式分流。

### 方案 B：独立端口或路由

为 Kiro 原生模式增加新端口或 `/native/v1` 路由。它在网络层隔离明显，但会增加 NewAPI 渠道、反向代理、容器和部署配置，也容易让两条处理链逐渐漂移。

### 方案 C：请求 Header 临时切换

允许客户用 Header 选择模式。它实现表面上最轻，但客户可以在同一会话中前后切换，缓存、历史 assistant 身份和错误诊断会出现不可预测组合，也无法由管理员可靠治理，因此不采用。

## 核心数据模型

后端新增独立枚举 `ClientResponseMode`：

```rust
pub enum ClientResponseMode {
    Detection,
    KiroNative,
}
```

持久化与 Admin API 使用稳定值 `detection`、`kiro_native`。该枚举不复用 `ToolCompatibilityMode`，原因是两者控制不同维度：

- `ClientResponseMode` 决定是否启用身份归一化和检测型本地短路；
- `ToolCompatibilityMode` 决定 Claude Code 工具名、参数和 Kiro 内置工具之间如何转换。

`ClientKey` 增加 `response_mode` 字段。反序列化旧 `client_api_keys.json` 时，字段缺失必须得到 `Detection`。管理端 API 对未知模式返回 400，不允许静默纠正为另一个模式。

创建、列表、单次创建响应和编辑 API 都返回模式。创建请求可以省略模式，省略时使用 `detection`；编辑请求省略模式表示不修改。轮换 Key 明文、重置统计、启禁用、分组改名和系统 Key 迁移必须保留原模式。

## 鉴权与请求上下文

当前中间件先通过 `verify_and_touch()` 获取 ID，再单独读取分组。新设计让客户端 Key 管理器在一次锁作用域中返回鉴权快照：

```rust
pub struct AuthorizedClientKey {
    pub id: u64,
    pub group: Option<String>,
    pub response_mode: ClientResponseMode,
}
```

`KeyContext` 同步增加 `response_mode`。请求一旦通过鉴权，本轮始终使用这个快照；管理员在请求执行期间切换模式，只影响随后开始的请求，不改变已经发往上游或已经开始输出 SSE 的请求。

这样既避免 ID、分组和模式分别加锁读取产生竞态，也保证流式重试、非流式重试、WebSearch 循环和本地回复使用相同档位。

## 行为策略边界

代码中新增一个小型纯策略对象，例如 `ResponseBehavior`，由 `ClientResponseMode` 构造。处理器不直接散布字符串比较，而读取清晰的能力方法：

```rust
impl ClientResponseMode {
    pub fn allows_detection_shortcuts(self) -> bool;
    pub fn allows_identity_normalization(self) -> bool;
}
```

第一版只设置这两个正交开关，不建立可任意组合的复杂功能位系统。

### 两种模式都保留的行为

以下逻辑属于 API 桥接、可靠性、客户明确请求的能力或既有商业口径，不因模式而变化：

- Anthropic 请求到 Kiro 请求、Kiro event-stream 到 Anthropic/SSE 的协议转换；
- Claude Code 内置工具名和参数映射、工具 ID 合法化、tool result 配对、Schema 验证与一次安全重试；
- UTF-8 分片恢复、半截 JSON 防护、空 assistant 内容重试、空 user 校验和错误快照；
- thinking 请求转换、上游真实 thinking 透传和缺失 thinking 的既有降级策略；
- PDF 文本提取和文档内容注入，但不包括检测型“唯一标识符本地回答”；
- WebSearch、mixed-tools loop、structured output 和 strict JSON 可靠性处理；
- 1 秒 SSE 握手/心跳和受控的本地 ping 健康响应；
- 缓存计量、TTL、命中率配置、input/cache token 拆分和 credit 计费；
- 模型映射、动态模型目录、分组路由、RPM、重试、日志和用量统计；
- 对 Claude Code 身份锚点的现有 system 清理。它属于客户端到 Kiro 的冲突消解，不向模型注入 Claude 身份，也不会把 Kiro 输出改写成 Claude。

原生模式因此表示“Kiro 原始助手回复与身份”，而不是“Kiro 私有协议裸透传”。

### 仅 `detection` 保留的行为

- 非流式助手文本中的 Kiro/AWS 身份归一化为 Claude/Anthropic；
- 流式 `IdentityStreamFilter` 跨 chunk 身份归一化；
- 模型资料中的 context window、knowledge cutoff、vendor、model identity 等确定性本地回答；
- `exact_system_output` 的静态字面量或固定 JSON 本地短路；
- `exact_user_echo` 的检测型精确回显短路；
- PDF 唯一标识符的确定性本地短路；
- 后续新增的检测探针短路，除非设计文档明确把它归入共享可靠性能力。

### `kiro_native` 的预期表现

- 上游回答“I'm Kiro”或自报 Amazon Web Services 时原样出现在助手文本中；
- 流式响应不会因为跨 chunk 过滤而延迟或替换 `Kiro` 字样；
- context window、knowledge cutoff、vendor 等问题正常发给 Kiro，由上游自行回答；
- 精确 system、精确 user echo 和 PDF 唯一标识符请求正常发给上游；
- 工具调用、SSE 首包、重试、计费和缓存口径与 `detection` 保持一致。

## 请求数据流

1. 客户端携带 `csk_*` 调用现有 `/v1` 接口。
2. 鉴权管理器在一次锁内校验 Key、更新调用时间，并返回 ID、分组、回复模式快照。
3. 中间件把快照写入 `KeyContext`。
4. `/v1/messages` 的所有入口基于模式决定是否尝试检测型本地短路。
5. 未被短路的请求继续走同一份文档展开、WebSearch、工具转换、缓存计量和 Kiro provider。
6. 流式和非流式响应基于同一个模式快照决定是否启用身份归一化。
7. 用量、错误和 trace 仍归属于原 Key；模式切换不重置任何统计。

`GET /v1/models` 和 `POST /v1/messages/count_tokens` 不改变输出语义。模型目录仍按 Key 分组动态汇总，原生模式不会隐藏 Claude 别名或 thinking 兼容入口，以免破坏既有客户端模型配置。

## 管理端设计

客户端 Key 页面增加“回复模式”列和状态徽标：

- `Claude 兼容`：对应 `detection`；
- `Kiro 原生`：对应 `kiro_native`。

新建 Key 弹窗增加两个单选项，默认选中 `Claude 兼容`：

- Claude 兼容：启用身份归一化和检测型确定性回复；
- Kiro 原生：保留工具、重试、SSE 和计费兼容，但助手保留 Kiro/AWS 原始身份。

编辑弹窗允许修改模式。由 `detection` 切换到 `kiro_native` 时展示提示：正在进行的请求不受影响，后续回复可能出现 Kiro/AWS 身份，检测站得分可能下降。反向切换时提示后续助手文本可能归一化为 Claude/Anthropic。

系统 Key 也允许编辑回复模式；其不可删除、轮换等既有限制保持不变。列表和创建结果必须显示实际保存的模式，不能只依赖前端默认值猜测。

## 配置优先级

现有全局配置继续作为能力总开关：

- 若全局身份归一化关闭，即使 Key 为 `detection` 也不改写身份；
- 若模型资料精确回复全局关闭，即使 Key 为 `detection` 也不本地回答身份资料；
- Key 为 `kiro_native` 时，无论上述全局检测开关是否开启，都禁止身份归一化和检测型短路。

有效条件统一为“全局能力开启并且 Key 模式允许”。管理端 Key 模式不能绕过全局安全关闭。

## 错误处理与可观测性

Admin API 收到未知模式时返回 `400 invalid_request`，原记录保持不变。Key ID 不存在时维持现有 404；落盘失败维持现有错误处理，不允许内存显示已切换而磁盘未保存。

鉴权失败不暴露该 Key 的模式。运行日志、trace 和错误快照元数据记录本轮 `response_mode`，但不记录明文 Key。这样管理员在 Key 切换后仍能确认历史错误发生时使用的是哪种模式。

正常请求只增加一个低基数字段，不增加正文日志。检测型本地短路被原生模式跳过时可记录 debug 级策略原因，不记录 prompt 内容。

## 并发与缓存

模式是鉴权时复制的小型枚举，不在 token 流中反复读取 Key 管理器。切换模式不清空 CacheMeter；缓存分区继续按 Key ID 和会话语义隔离，因为两种模式的输入请求与计费口径没有改变。

本地模型资料回答不会污染上游缓存。原生模式跳过本地回答后首次真正访问上游属于正常行为，不额外预热或伪造 cache read。

## 测试策略

### 数据与 API

- 旧 JSON 缺少 `responseMode` 时加载为 `detection`；
- 创建省略模式时保存 `detection`，显式创建可保存 `kiro_native`；
- 列表、创建响应、编辑响应返回真实模式；
- 未知模式返回 400，且不修改原记录；
- 编辑名称但省略模式时保留原模式；
- 轮换、重置、启禁用、分组重命名和系统 Key 迁移保留模式；
- 同一 Key 切换模式时 ID、明文、分组和累计统计不变。

### 鉴权隔离

- 两把绑定同一账号分组、但模式不同的 Key，得到不同模式快照；
- 模式切换前已经获得的快照保持不变，新请求得到新模式；
- 并发请求之间不共享或串改模式。

### 消息行为

- 非流式相同 Kiro 身份文本：`detection` 归一化，`kiro_native` 原样保留；
- 流式在任意 chunk 边界拆分 `Kiro`/vendor JSON：`detection` 正确归一化，`kiro_native` 不启用过滤器；
- 模型资料、精确 system、精确 user echo 和 PDF identifier：`detection` 可本地短路，`kiro_native` 必须继续到 provider；
- 普通文本、tool use、thinking、strict JSON、PDF 提取、WebSearch、空响应重试和 UTF-8 恢复在两种模式下均通过；
- 早期 SSE 握手和本地 ping 在两种模式下均保留，不回归 1 秒首响应；
- 两种模式的 input/cache/output/credit 口径对相同非本地请求保持一致。

### 管理端

- TypeScript 类型与 Rust JSON 字段一致；
- 新建默认值、显式选择、编辑回填和提交正确；
- 列表徽标正确，切换提示明确；
- API 错误时不乐观显示未保存模式。

### 完整验证

- Rust 定向单元测试与路由集成测试；
- `cargo fmt --all -- --check`；
- `cargo check -j 1 --all-targets`；
- 完整 Rust 测试和 `anthropic_probe`；
- Admin UI 单元/契约测试、类型检查和生产构建；
- 8991 使用两把不同模式的 Key 对同一组身份、工具、thinking、PDF、strict JSON、缓存和 SSE 首包请求做并排验证；
- 不在本功能阶段修改或部署生产 8990。

## 发布与回滚

升级部署前备份 `client_api_keys.json`。升级后旧 Key 应全部显示 `Claude 兼容`，行为与升级前一致。先在 8991 创建一把 `Kiro 原生` Key 验证，不直接修改现有检测 Key。

若出现工具、计费或 SSE 回归，回滚二进制即可；新增字段对旧版本属于未知 JSON 字段，Serde 默认会忽略，因此现有 Key 文件无需反向迁移。若只出现某一客户身份预期不符，管理员可立即把该 Key 切回 `detection`，无需轮换 Key 或重启 RS。

## 验收标准

1. 同一实例可以同时使用 `detection` 和 `kiro_native` Key，互不串扰。
2. 旧 Key 升级后全部保持当前检测优化行为。
3. 原生 Key 的流式和非流式助手文本不发生 Kiro/AWS 到 Claude/Anthropic 的改写。
4. 原生 Key 不触发模型资料、精确 system/user 和 PDF identifier 本地检测回复。
5. 工具、thinking、PDF 提取、strict JSON、重试、缓存、计费、日志和 1 秒 SSE 首响应不因模式分流而退化。
6. 管理端可在创建和编辑 Key 时明确选择模式，并显示实际持久化结果。
7. 未提供客户端临时覆盖入口，单次请求始终使用鉴权时确定的模式快照。

## 实施结果（2026-07-15）

- 已实现 `detection` / `kiro_native` 两种 Key 级回复模式，旧 Key 和未知持久化值均安全降级为 `detection`。
- 已把鉴权时的模式快照贯穿 `/v1/messages`、`/cc/v1/messages`、trace 和错误快照；未增加 Header 或其他单请求覆盖入口。
- `kiro_native` 只关闭身份归一化和检测型本地短路；工具、thinking、PDF 提取、WebSearch、strict JSON、重试、缓存、计费和早期 SSE 仍走共享路径。
- 管理端已支持创建、编辑和展示模式，保存失败时不做乐观更新。
- 规格复核后补齐了 trace Admin API 的 `responseMode`、Key 文件原子替换写入，以及旧版 fallback 快照缺字段时的 `detection` 兼容。
- 本地验证：`cargo check -j 1 --all-targets` 通过；`cargo test -j 1 --all-targets` 共通过 994 项（`anthropic_probe` 18 项、主程序 976 项）；管理端 `bun test` 通过 75 项，`bun run build` 成功。
- 设计与计划提交：`09a96d9`、`099abfd`；功能实现由本次后续本地提交保存。
- 尚未执行隔离公网 8991 的双 Key 实测，因此没有测试 Key ID，也没有线上首响应实测值；1 秒目标目前仅由既有 early SSE/ping 路径未被门控及本地回归测试证明未发生代码级回退。
- 已知上游限制保持不变：Kiro 未产生真实 thinking、返回空 assistant 或中途截断工具 JSON 时，RS 仍只能按现有降级、重试和错误响应处理。
- 未推送 GitHub，未部署隔离 8991，也未修改生产 8990。
