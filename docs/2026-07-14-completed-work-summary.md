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

### 4. 严格 SSE 状态机

- 移除 `message_start` 之前的 `: connected` 和非标准 ping。
- 校验 `content_block_start`、delta、`content_block_stop` 成对和顺序。
- 校验 `message_delta` 以及唯一的 `message_stop` 终态。
- 增加原始 SSE wire 探针，避免只解析 `data:` 而漏掉非法前导事件。

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

## 六、Git 状态

- 协议与错误快照分支已本地合并回 `master`。
- 合并提交：`d26f6f8 merge: 合并 CCTest/Ztest 协议与错误快照优化`。
- 状态文档提交：`909f314 docs(protocol): 更新本地合并状态`。
- 尚未推送 GitHub。
- 生产 `8990` 未修改。

## 七、尚未完成或不能仅靠本地测试确认

- 尚未把当前 `master` 部署到隔离的公网 `8991`。
- 尚未运行新的真实 CCTest/Ztest 报告，因此不能声称最终分数已经达到 98。
- 真实 WebSearch、文本 PDF、上游 thinking 能力和检测站 fingerprint 仍需用 8991 的真实流量验收。
- 上游本身不提供的原生 thinking 内容或 Anthropic 私钥签名，RS 无法凭空生成；当前只保证协议不会因此断流。
- 生产 `8990` 的升级和正式客户流量验收需在 8991 外部检测通过后再执行。
