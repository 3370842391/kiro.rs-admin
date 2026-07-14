# 2026-07-14 已完成任务汇总

本文记录本轮在 `kiro.rs-admin` 中完成的工具调用、Anthropic 协议兼容、错误日志和管理端优化，以及这些改动对客户对话的实际影响。

## 一、工具调用与续轮 400 修复

### 1. 历史工具 ID 规范化

- 找到续轮请求立即返回 `400 REQUEST_BODY_INVALID` 的确定原因：历史 `tool_use.id` 中含 `/`、`.`、`:`、空值或过长内容时，RS 原样转发给 Kiro，而 Kiro 只接受 ASCII 字母、数字、`_`、`-` 且不超过 64 字节的 ID。
- 在发往 Kiro 的请求副本中，将非法 ID 稳定映射为 `tooluse_` 加 SHA-256 摘要。
- 同步改写对应的 `tool_result.tool_use_id`，保证工具调用和工具结果仍然成对。
- 合法 ID 保持不变；不改写客户端保存的历史，也不改写此前已经返回给客户端的响应。
- 重复 ID、孤立 `tool_result`、顺序错误或映射冲突会在 RS 本地 fail-closed，不再把损坏历史继续发给 Kiro。

主要实现：`src/anthropic/tool_history.rs`、`src/anthropic/converter.rs`。

### 2. 工具 Schema 和参数值校验

- 在工具调用交付客户端前校验 `type`、`properties`、`required`、`const`、`enum`、`items` 和 `additionalProperties`。
- 只允许修复客户 Schema 明确声明的确定值：required 字段中的 `const` 或单值 `enum`。
- 不从用户问题中猜城市、nonce 或其他业务参数，不做字符串转数字，不填空字符串或零值。
- 缺少普通 required 字段、类型错误、额外字段或未声明工具时返回明确协议错误，不把错误参数交给客户端执行。
- 修复 Schema 后再次去重，优先保留原生结构化工具调用，避免原生调用和文本 `<invoke>` 被客户端执行两次。
- Mixed WebSearch 会先校验客户工具；客户工具非法时不会先调用 MCP 搜索再失败。

主要实现：`src/anthropic/tool_schema.rs`、`src/anthropic/handlers.rs`、`src/anthropic/stream.rs`、`src/anthropic/websearch_loop.rs`。

## 二、Anthropic 协议兼容优化

### 3. 结构化输出 `output_config.format`

- 完整解析并保留 `output_config.format.type=json_schema`。
- stream 和 non-stream 都会校验最终输出必须是一个完整 JSON 值，并符合客户给出的 Schema。
- 拒绝 Markdown 代码块、解释文字、多 JSON、缺字段、错误类型和额外字段。
- 首轮请求不附加额外 Schema 提示，避免正常请求产生隐藏 Token 注入。
- 首轮输出不合法且尚未向客户交付语义内容时，最多进行一次恢复请求；第二次仍不合法则返回明确错误。

主要实现：`src/anthropic/structured_output.rs`、`src/anthropic/exact_output.rs`、`src/anthropic/types.rs`、`src/anthropic/handlers.rs`。

### 4. New API 约 1 秒可见首响应

- 在启用提前握手时立即发送 `: connected`，上游仍未响应时约 1 秒发送标准 Anthropic `event: ping`。
- `message_start` 仍是首个正式消息事件；comment/ping 不计入模型正文或 Rust `first_token_ms`。
- OpenAI Chat/Responses 转换器会忽略 comment/ping，不把它们转成客户正文。
- 客户端断开会取消仍在等待的 provider future，不产生脱离请求生命周期的后台调用。
- 由于响应头和 SSE body 已提交，后续上游错误会通过标准 SSE error 返回，无法再改写 HTTP 状态码。

主要实现：`src/anthropic/stream.rs`、`src/bin/anthropic_probe.rs`。

### 5. Thinking 签名兼容

- 上游提供原生 thinking signature 时继续原样透传。
- 上游没有原生签名时，生成 request-scoped 的 `krs1_...` 不透明回放令牌。
- 不伪造 Anthropic 私钥签名，不修改 thinking 正文。

主要实现：`src/anthropic/thinking_signature.rs`、`src/anthropic/stream.rs`。

### 6. 工具 JSON 截断与协议错误处理

- 对未完成的 `tool_use` JSON、非法 UTF-8、SSE 状态错误和上游空内容记录明确错误分类。
- 不把未完成的工具调用转发给客户端执行。
- 保留流尾部和上游尝试链，便于复现 `Upstream ended before completing tool_use` 等问题。

## 三、错误日志和诊断系统

### 7. 持久化错误快照

- 新增独立 `error_snapshots.db`，只在失败、中断、协议错误或配置允许的“重试后恢复”场景保存快照。
- 正常单跳成功请求不保存大型正文。
- 记录脱敏后的客户请求、工具 JSON/Schema、Kiro 请求、上游响应、重试链、HTTP 状态、凭据编号、endpoint、trace ID 和流尾部。
- 快照写入、压缩或数据库失败不会改变客户的 HTTP 状态、SSE 事件或错误正文。

主要实现：`src/anthropic/error_snapshot.rs`、`src/admin/error_snapshot_db.rs`、`src/kiro/provider.rs`、`src/admin/trace_db.rs`。

### 8. 脱敏与数据完整性

- Authorization、API Key、access/refresh/id token、client secret、Cookie、密码和凭据字段不会明文入库。
- 图片、PDF、data URI 和长 base64 只保存长度及 SHA-256，不保存完整二进制内容。
- 大 payload 使用 UTF-8 安全分块和 zstd 压缩，读取时校验分片数量、长度和 SHA-256。
- 解压设置硬上限，避免损坏数据或压缩炸弹占满内存。

### 9. 容量治理

- 支持启用开关、保留天数、最大容量、最小剩余磁盘、是否捕获恢复请求和是否保存正文。
- 当前默认策略为保留 90 天、最大 200GB、至少保留 100GB 空闲空间。
- 磁盘压力较大时自动退化为 metadata-only。
- Critical 快照和管理员手动 pin 的快照不参与普通自动清理。

### 10. 管理端错误快照页面

- 新增错误快照列表、筛选、分页、存储状态和容量治理配置。
- 支持查看详情、按需加载 payload、复制、下载、pin、取消 pin、删除和手动清理。
- Trace 日志可以直接下钻到关联错误快照。
- 测试文件已从生产 TypeScript 构建范围排除，不影响管理端正式构建。

主要实现：`admin-ui/src/components/error-snapshot-page.tsx`、`admin-ui/src/components/error-snapshot-dialog.tsx`、`admin-ui/src/hooks/use-error-snapshots.ts`。

### 11. 管理端批量 RPM 与运行容量可视化

- 新增 `PUT /api/admin/credentials/batch`，一次请求批量修改 RPM、分组和来源渠道。
- 服务端先校验全部 ID 和补丁，再在单次账号写锁内修改；不存在或重复 ID 时零修改，成功批次最多持久化一次。
- 单批最多 10000 个账号；RPM 范围为 `0..=100000`，其中 `0` 表示不限速。
- 分组支持 replace/add/remove；分组值最多 100 个，每个名称最多 64 个 Unicode 字符，成员判断使用集合避免大批量退化。
- 凭据状态响应新增最近 60 秒 RPM 汇总和 `inFlight`；已发生负载包含禁用账号尚未过期的窗口记录，容量只统计启用账号。
- 管理端状态条显示最近 60 秒 RPM、有限容量/不限速、有限账号剩余、满载账号和进行中请求，不新增独立轮询。
- 批量编辑改为单个服务端请求；失败时保留弹窗和选择，成功后才刷新并清空选择。
- RPM 输入支持内联校验、错误焦点和 ARIA 关联；分组模式暴露当前 pressed 状态；320px 页面和弹窗无横向溢出。

主要实现：`src/kiro/token_manager.rs`、`src/admin/types.rs`、`src/admin/service.rs`、`src/admin/handlers.rs`、`src/admin/router.rs`、`admin-ui/src/lib/rpm-operations.ts`、`admin-ui/src/components/batch-edit-credential-dialog.tsx`、`admin-ui/src/components/rpm-status-bar.tsx`。

## 四、客户可感知影响

| 改动 | 客户影响 | Token/计费影响 |
|---|---|---|
| 非法历史工具 ID 映射 | 原本立即 400 的 session resume/工具续轮通常可以继续 | 无变化 |
| 工具 Schema 校验 | 非法参数不再下发执行；依赖旧版宽松行为的客户端可能更早收到协议错误 | 普通合法调用无变化；安全恢复时最多增加一次上游请求 |
| 修复后去重 | 避免同一个工具被执行两次及其副作用 | 无变化 |
| 结构化输出 | JSON Schema 请求会完整缓冲后返回，首字节可能变慢；非法输出最多恢复一次 | 普通请求无变化；恢复请求可能增加少量上游消耗 |
| 严格 SSE | 慢上游在 `message_start` 前不再发送非标准 ping，首个响应字节可能稍晚 | 无变化 |
| Thinking 签名 | 只改变无原生签名时的 signature 字段，不改变正文 | 无变化 |
| 错误快照 | 失败路径增加脱敏、压缩和磁盘写入；正常成功请求不写大型正文 | 不改变模型 Token、缓存拆分或计费 |
| 批量 RPM 与容量展示 | 只影响管理端配置和只读指标；降低 RPM 后账号会在滚动窗口自然回落前暂停接收新请求 | 不改变模型 Token、缓存拆分或计费 |

本轮没有重新注入大型 system prompt，没有硬编码检测站 nonce/答案，也没有修改缓存命中整形、Token 总量或计费拆分。

## 五、本地验证结果

- `cargo test --quiet --locked --no-default-features -j 1`
  - `anthropic_probe`：18/18 通过。
  - 主测试：869/869 通过。
- `cargo check --all-targets --locked --no-default-features`：通过。
- `cargo fmt -- --check`：通过。
- `git diff --check`：通过。
- Admin UI `bun test`：19/19 通过。
- Admin UI `bun run build`：通过，构建 2577 个模块。
- 错误快照 smoke 脚本契约测试：2/2 通过。

本轮批量 RPM 功能在功能分支上的最终验证结果：

- `cargo test -j 2 --bin kiro-rs --locked --no-default-features`：885/885 通过，0 失败。
- `cargo check --all-targets --locked --no-default-features`：通过；仅保留项目已有的 14 条非阻塞 warning。
- `cargo fmt -- --check`：通过。
- Admin UI `bun test`：57/57 通过，共 148 条断言。
- Admin UI `bun run build`：通过，生产构建处理 2579 个模块。
- `git diff --check`：通过。
- 隔离本地浏览器验收：320px 和 1280px 均无横向溢出；空 RPM 会保持弹窗、聚焦错误字段并暴露 ARIA 错误关联；`rpmLimit=0`、有限账号剩余和分组模式 pressed 状态显示符合协议。

本地合并回 `master` 后仍需对合并结果重新运行 Rust 全量测试、Admin UI 测试与生产构建，不能用分支结果替代合并后验证。

## 六、Git 状态

- 本轮可靠性恢复、API Key 区域、工具 Schema、空 user 兼容和管理端安全门禁已本地合并回 `master`。
- 最终运行代码提交：`fe0ad71 fix(trace): 返回空请求兼容标记`。
- 尚未推送 GitHub。
- 隔离公网 `8991` 已部署镜像 `kiro-rs-test:fe0ad713f9f1`。
- 生产 `8990` 仍运行 `ghcr.io/3370842391/kiro-rs:sha-218ae0`，未修改。

## 七、尚未完成或不能仅靠本地测试确认

- 尚未运行新的真实 CCTest/Ztest 报告，因此不能声称最终分数已经达到 98。
- 真实 WebSearch、文本 PDF、上游 thinking 能力和检测站 fingerprint 仍需用 8991 的真实流量验收。
- 上游本身不提供的原生 thinking 内容或 Anthropic 私钥签名，RS 无法凭空生成；当前只保证协议不会因此断流。
- 生产 `8990` 的升级和正式客户流量验收需在 8991 外部检测通过后再执行。

## 八、本轮可靠性恢复追加

### 12. API Key 批量导入、昵称和区域修复

- 管理端支持 `nickname | API Key` 与 `nickname | API Key | apiRegion` 文本批量导入，逐行预览只显示掩码。
- API Key 的 Auth Region 固定为 `us-east-1`，API Region 只允许 `us-east-1` / `eu-central-1`。
- 模型、用量、偏好和生成请求全部按 API Key 的显式 API Region 路由；EU CodeWhisperer 使用 `q.eu-central-1.amazonaws.com`。
- 旧 API Key 因缺 Region 被标记 `InvalidConfig` 后，可在编辑框选择正确 Region 并自动重新启用，无需重启。
- nickname 与真实 email 分开保存；添加、文本导入和编辑均 trim，最多 128 个 Unicode 字符。

### 13. 工具 Schema 定向重试与安全诊断

- 首轮工具参数不合法时只保存工具名、input key、JSON 类型、违规项和 attempt，不保存参数值、客户正文或完整流尾。
- 只有全部违规均为 `MissingRequired`、首轮尚未交付任何语义内容或工具时，才允许一次透明重试。
- 第二轮仅增强失败工具的 description，并只列出 Schema 已公开且安全的缺失路径；不猜路径、城市、nonce 或业务值。
- 类型错误、额外字段、未声明工具、已输出正文或已转发工具均不透明重试。
- 第二轮仍失败时，非流式返回明确 HTTP 502；流式因 HTTP 200 已提交，通过 `upstream_tool_schema_error` SSE 事件结束，非法工具不会交付客户端。

### 14. 空 user 请求兼容开关

- 新增 `emptyUserMessageCompat`，默认关闭，并可在管理端“协议兼容设置”即时修改和持久化。
- 当前 user 为空字符串、空文本块或空数组且会生成空 Kiro current message 时，会在获取凭据前本地返回清晰 400，不再消耗账号并触发模糊 `REQUEST_BODY_INVALID`。
- 开启后仅对“单轮、非空 system、显式空文本、无工具声明/选择”的精确形状补入最小 `Continue.`；多轮、空数组、工具、图片、文档和 tool_result 不使用该兜底。
- trace DB 增加 `emptyUserCompatApplied`，成功兼容请求可被明确检索，不需要依赖 DEBUG 正文日志。

### 15. 管理端批量写盘与字段边界

- 批量 RPM/分组/来源渠道更新在写盘失败时恢复内存旧值，同值重试仍会再次写盘，不再返回假成功。
- 批量事务锁顺序固定为 `persist_lock -> entries`；只在管理员批量保存期间跨磁盘 I/O 持有，不扩大普通模型请求的持续阻塞。
- `sourceChannel` 在批量更新、单条编辑和新增凭据路径均 trim，并限制最多 128 个 Unicode 字符。

### 16. 当前集成分支验证

- Rust：`anthropic_probe` 18/18，主程序 923/923，0 失败。
- `cargo check --all-targets --locked --no-default-features -j 1`：通过；仅保留项目既有非阻塞 warning。
- Admin UI：71/71，226 条断言；`bun run build` 通过，构建 2581 个模块。
- `cargo fmt --all -- --check`、`git diff --check`：通过。
- 密钥扫描仅命中测试保留域名 `example.test`，未发现完整 API Key、Bearer、Cookie 或真实客户邮箱。

### 17. 客户影响结论

- 正常非空对话、合法工具调用、缓存拆分和计费逻辑不变。
- 提前握手开启时，New API 可约 1 秒收到 ping；代价是后续错误只能通过 SSE error 表达。
- 只有首轮未交付内容且工具缺 required 字段时，可能增加一次上游调用和少量 retry-only 描述 Token。
- 默认关闭的空 user 兼容只把原来的上游 400/502 提前为本地 400；开启后仅精确空文本请求增加一个最小输入。
- 管理员修正 API Region 会恢复此前 `InvalidConfig` 账号；超长 nickname/sourceChannel 现在返回 400。

### 18. 隔离公网 8991 验收

- 部署提交/镜像：`fe0ad713f9f1509baaf9d4497d23783b1f3ba263` / `kiro-rs-test:fe0ad713f9f1`。
- 公网 HTTPS 管理端 `https://rs-test.43-225-196-10.sslip.io/admin` 返回 200；测试卷仍为 `/opt/kiro-rs-test/data-test:/app/config`。
- 普通非流式消息返回 200、text content 和 `end_turn`；默认空 user 在凭据调用前返回 400 `invalid_request_error`。
- 开启 `emptyUserMessageCompat` 后，精确空文本请求返回 200；Admin trace 可读取 `emptyUserCompatApplied=true`；验收后开关已恢复为 false。
- 10 次 SSE 首个 `data:` 均为 ping，p95 为 1004.8ms；热修复镜像复测 3 次最大 1003.7ms。
- 测试容器日志未发现 panic/fatal；生产 8990 镜像和监听状态保持不变。
