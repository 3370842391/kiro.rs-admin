# 自动压缩诊断监控设计

日期：2026-07-29

## 背景与目标

线上长会话已经出现两组同时成立的证据：上游上下文占用到达 99.99%/100%，代理也产生了 `model_context_window_exceeded`；但后续请求体没有明显缩小，并持续撞上约 3 MB 的 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 字节墙。现有日志无法回答以下关键问题：

- 上游 `contextUsageEvent` 是否被代理完整观察到；
- 客户端实际收到的 `message_start.usage` 是否包含同一上下文占用；
- 终止信号是否成功进入响应队列，还是客户端在此前断开；
- 后续同一会话的请求是否发生了可观察的压缩；
- 压缩后是否仍然因为文本历史过大而撞字节墙。

本次只增加诊断监控，不修复或改写自动压缩行为。任何 HTTP 状态、JSON 响应、SSE 事件、事件顺序、重试策略、usage 口径和计费逻辑都必须保持不变。

## 已确认约束

1. Docker 只输出高压力告警和诊断结论，正常低压力请求不新增日志。
2. 详细安全计数异步写入 `traces.db`，不建立第二套数据库或同步写路径。
3. 不保存请求正文、消息文本、工具参数、请求头、Token、Cookie、API Key 或凭证。
4. `metadata.user_id` 只保存 SHA-256 十六进制 `session_hash`，不保存原文。
5. 不开启全局 DEBUG；关键诊断在 `INFO/WARN` 级别可用。
6. 不新增全局会话 Map，不在请求路径查询 SQLite，不新增请求路径数据库锁。
7. 沿用现有 4096 容量 `try_send` 队列、批量事务和 `spawn_blocking` 写入器。队列满时允许丢诊断记录，但不得阻塞客户请求。
8. 增加独立运行时开关 `autoCompactDiagnosticsEnabled`，默认开启，可在 Admin 治理设置中即时关闭并持久化。

## 开关语义

配置字段：

```json
{
  "autoCompactDiagnosticsEnabled": true
}
```

运行时状态存放在 `MultiTokenManager` 的 `AtomicBool` 中。请求入口只做一次原子读取：

- 关闭时，诊断构造器立即返回禁用状态；不计算 session hash、不扫描消息、不序列化请求、不生成诊断 JSON、不输出诊断日志。
- 开启时，创建当前请求独占的诊断状态；状态只通过原子计数更新，不新增全局 Mutex。

开关与普通 trace 相互独立：

| traceEnabled | autoCompactDiagnosticsEnabled | 结果 |
|---|---|---|
| 开 | 开 | Docker 高压力结论 + `traces.db` 详细诊断 |
| 关 | 开 | 仅 Docker 高压力结论 |
| 开 | 关 | 保留普通 trace，不采集自动压缩诊断 |
| 关 | 关 | 两类新记录均关闭 |

## 采集架构

```text
请求安全形状统计
  -> Kiro 上游请求/事件观察
  -> 实际入队的客户端响应事件观察
  -> finalize 生成不可变诊断快照
  -> 高压力时输出结构化 Docker 结论
  -> 随 TraceRecord try_send 到现有异步 SQLite 写入器
```

### 请求侧安全形状

请求入口记录：

- 模型、流式标记、1M beta 标记、客户端版本；
- `Content-Length`，缺失时使用紧凑 JSON 序列化长度作为 `request_body_bytes`；
- message/system/tool/image/tool_use/tool_result 数量；
- message/system/tool schema/image/tool_use input/tool_result content 的字节计数；
- `session_hash`。

所有字节字段都只保存整数。工具 `input`、tool result、system 和 message 的值永不进入诊断对象。

客户端版本只从安全的版本字段或 User-Agent 中提取 `x.y.z` 数字版本，不保存完整请求头。

### 上游观察

利用现有 `TraceSink::on_diagnostic` 观察每次实际下发的 Kiro body，只记录：

- 请求次数；
- 第一、最后、最小、最大 body 字节数；
- 是否观察到包含 `CONTENT_LENGTH_EXCEEDS_THRESHOLD` 的上游响应。

在每个 `Event::ContextUsage` 进入 `StreamContext`/`BufferedStreamContext` 前记录：

- `upstream_context_tokens`；
- `upstream_context_percentage`；
- context usage 事件次数；
- 达到 100% 时的内部上下文终止信号。

Metering 只记录事件次数；费用继续使用现有 usage/trace 字段，不改变计费。

### 客户端响应观察

只在 SSE 事件成功写入现有 mpsc 响应队列后记录，避免把“准备发送”误判为“已入队”：

- `message_start` 是否入队；
- `message_start.usage` 的 input/cache creation/cache read 合计，即 `client_reported_tokens`；
- `message_delta` 是否入队及对外 stop reason；
- `message_stop` 是否入队；
- `error` 事件类型；
- 是否已有语义输出；
- ProbationBuffer 是否提交、是否触发透明重试；
- 客户端是否在信号前断开。

非流式成功响应使用最终 trace usage 作为客户端报告值。此观察只读现有事件，不修改事件对象或序列化结果。

## 当前请求诊断分类

分类函数是纯函数，输入为不可变快照和 finalize 状态。优先级从高到低：

1. `payload_limit_preempted`：观察到字节阈值异常，或最终错误包含 `CONTENT_LENGTH_EXCEEDS_THRESHOLD`。
2. `client_disconnected_before_signal`：高压力请求在有效 usage/终止信号入队前客户端断开。
3. `proxy_context_signal_not_exposed`：观察到高上下文占用，但没有可用客户端 usage 信号入队。
4. `client_usage_signal_incomplete`：客户端 usage 已入队，但显著低于同请求的上游上下文 token。
5. `context_signal_enqueued`：高上下文占用已通过客户端 usage 信号完整入队。
6. `upstream_context_unknown`：请求或 Kiro body 已处于高字节压力，但没有观察到 `contextUsageEvent`。
7. `normal`：未达到压力阈值且协议信号完整。

高上下文阈值使用 80%，高字节压力阈值使用 2,500,000 字节；字节墙观察阈值使用 3,000,000 字节。分类名称描述代理当前能确认的事实，不把客户端内部行为写成确定根因。

## 跨请求推断

跨请求推断只在现有后台 SQLite 写入事务中进行，不进入请求路径。写入同一 `session_hash` 的新记录时，读取该会话上一条诊断记录：

- 上一请求已达到高上下文压力并成功入队信号，下一请求体仍不小于上一请求的 85%，且仍大于 2.5 MB：存储主诊断 `suspected_client_compaction_not_triggered`。
- 下一请求体缩小至少 20%，但仍发生 `payload_limit_preempted`：存储主诊断 `suspected_compaction_insufficient`。
- 其他情况保留当前请求分类。

客户端版本处于 `2.1.161` 至 `2.1.220`（含边界）时，在详细 JSON 中设置 `knownThirdPartyAutocompactRegressionPossible=true`。该标记只是已知第三方回归提示，不替代证据分类。

## 数据库设计

`traces` 新增以下可空列，旧记录保持兼容：

```text
session_hash
client_version
compaction_diagnosis
request_body_bytes
upstream_context_tokens
upstream_context_percentage
client_reported_tokens
compaction_diagnostics_json
```

新增幂等索引：

```text
(session_hash, ts_epoch)
compaction_diagnosis
```

`compaction_diagnostics_json` 带 `schemaVersion: 1`，保存其余安全计数和布尔状态。迁移继续使用 `PRAGMA table_info` + 缺列 `ALTER TABLE`，可重复执行。

写入仍由 `TraceStore::insert` 的 `try_send` 完成。请求路径不得调用 `conn.lock()`；跨请求推断、JSON 补充和 SQL 写入全部位于后台批处理事务中。

## Docker 日志

只有满足以下任一条件才输出 `auto_compact_diagnostics` 结构化 `WARN`：

- 上下文占用达到 80%；
- 入站或 Kiro body 达到 2.5 MB；
- 当前分类不是 `normal` 且请求具有压力证据。

日志字段只包含 trace id、session hash、客户端数字版本、模型、整数计数、百分比、布尔状态和诊断枚举。不得包含正文、完整 User-Agent、请求头、错误响应正文、工具名或工具参数。

## Admin 体验

治理设置新增“自动压缩诊断”独立开关，说明关闭后普通 trace 不受影响。

Trace 页面新增：

- 自动压缩诊断原因筛选；
- “只看高压力”筛选；
- session hash 精确筛选；
- 行内诊断徽章；
- 展开详情中的安全计数、信号时间线和“查看同会话”按钮。

Admin 不展示原始 `metadata.user_id`，只展示可复制/筛选的 hash。

## 客户体验与失败策略

- 不增加网络调用，不改变上游请求体，不改变客户端响应。
- 诊断扫描只在开关开启时执行；关闭后为一次原子读取和分支。
- 所有运行时观察使用请求局部不可变字段和原子计数，不引入跨请求共享锁。
- 诊断序列化失败时仅省略 JSON 并记录安全 WARN；客户响应继续。
- trace 队列满时沿用丢记录策略，绝不等待。
- SQLite 迁移或写入失败只影响诊断可见性，不影响 API。

## 测试与验收

1. 配置默认值、camelCase 往返、运行时切换和持久化测试。
2. 开关关闭测试证明不计算 hash、不扫描 payload、不生成 JSON。
3. 请求形状测试证明只产生计数和 hash，序列化结果不含测试正文、工具参数或凭证。
4. 分类表驱动测试覆盖七个当前分类。
5. 客户端版本范围边界测试覆盖 2.1.160/161/220/221。
6. SSE 观察测试比较启用与禁用诊断时的完整输出字节，要求完全相同。
7. SQLite 迁移幂等、字段往返、诊断/高压力/session 筛选和跨请求推断测试。
8. 保留并扩展“持有 conn 锁时 insert 能立即返回”和“队列满丢弃不阻塞”测试。
9. Admin API JSON、查询参数和前端契约测试。
10. 完整前端测试/构建、目标 Rust 测试、全量 Rust 测试；基线既有失败单独报告，不归因于本功能。

## 非目标

- 不伪造 token、提前发送 `model_context_window_exceeded` 或修改 1M 窗口声明。
- 不替客户端执行 `/compact`。
- 不裁剪文本历史、不改变图片压缩策略。
- 不改变计费、usage 或缓存拆分。
- 不部署到生产服务器；本次交付为本地分支代码和可验证诊断能力。
