# CCTest/Ztest 本轮实现状态

更新时间：2026-07-14

## 已完成并验证

- 历史 `tool_use.id` 含 `/`、`.`、`:`、空值或过长时，在发往 Kiro 的请求副本中做稳定映射，并同步对应 `tool_result`；客户可见历史 ID 不改写。
- 工具 Schema/值契约校验：只对客户明确声明的 `required`、`const`、单值 `enum` 做确定性处理；未声明工具、孤立结果、重复 ID 和无法安全配对的历史会 fail-closed。
- 修复 Schema 修复后可能重复交付的问题：原生结构化调用优先，文本 `<invoke>` 只在语义不重复时交付。
- 混合 WebSearch 在调用 MCP 之前先验证客户工具 Schema；非法客户端工具不会触发搜索调用。
- `output_config.format.type=json_schema` 支持 stream/non-stream 严格校验，拒绝 Markdown、解释文字、多 JSON 值、缺失字段、错误类型和额外字段。首轮保持原始请求，只有恢复轮才附加客户端 Schema，避免正常请求产生隐藏 Token 注入。
- 严格 SSE wire 探针拒绝 `message_start` 前的 comment/ping，校验 content block start/delta/stop 配对、`message_delta` 与唯一 `message_stop` 终态。
- thinking 无上游原生签名时使用 request-scoped `krs1_...` 不透明回放令牌；原生签名仍原样透传，不伪造 Anthropic 官方签名。
- 错误快照、Provider 诊断和 Admin UI 已完成：失败路径保留脱敏现场，正常成功请求不写大正文；提供列表、详情、payload、下载、pin、删除、清理和容量治理。

## 客户可感知影响

| 改动 | 影响 | 计费/Token |
|---|---|---|
| 工具 ID/Schema | 原本因 Kiro 400 断掉的续轮可继续；非法参数不再下发执行，可能收到明确协议错误；首轮无语义交付时最多一次安全重试 | 不注入大型 system prompt；只在安全重试时增加一次上游调用 |
| `output_config.format` | 完整缓冲并校验后再返回，首字节延迟增加；校验失败最多一次恢复轮 | 普通请求不变；恢复轮可能增加少量上游消耗 |
| 严格 SSE | `message_start` 前不再发非标准 comment/ping，慢上游时首字节可能稍晚 | 不变 |
| thinking token | 仅 signature 字段由固定占位改为 request-scoped 令牌，正文不变 | 不变 |
| 错误快照/诊断 | 失败路径增加脱敏、压缩和磁盘写入开销；磁盘压力时自动 metadata-only；正常成功不写大 BLOB | 不改变 token 总量、cache 拆分或计费 |

## 当前交付状态

- 已在本地合并回 `master`，合并提交：`d26f6f8 merge: 合并 CCTest/Ztest 协议与错误快照优化`。
- `master` 工作区已通过合并后回归测试；生产 `8990` 未修改。
- 尚未推送 GitHub，也尚未部署隔离 `8991` 运行真实 CCTest/Ztest 报告。

## 尚未完成的外部验收

- CCTest/Ztest 的最终分数、真实 WebSearch fixture、文本 PDF fixture 和上游 thinking 能力仍需在隔离 8991 上运行；本地测试不会伪造检测站 nonce、签名或答案。
- `scripts/error-snapshot-smoke.sh` 默认只读，需在 8991 使用管理员 Key 运行；设置 `ERROR_SNAPSHOT_SMOKE_MUTATE=1` 前必须确认挂载目录是 `data-test`。
- Admin UI 已将 `*.test.*` 排除出生产 TypeScript 编译范围；标准 `bun run build` 与 `bun test` 均可直接运行。不要把临时 `tsconfig.production-check.json` 提交。

## 本地验证证据

- Rust：`cargo test --quiet --locked --no-default-features` → 探针 18/18、主测试 869/869，0 failed。
- Rust：`cargo check --all-targets --locked --no-default-features` → exit 0；仅有既有 dead-code/unused 警告。
- Admin UI：`bun test` → 19 pass / 0 fail。
- Admin UI：`bun run build` → 2577 modules built；`bun test` → 19 pass / 0 fail。
- 8990 未修改；本分支只允许部署/验收 8991，当前未推送 GitHub、未合并 master。
