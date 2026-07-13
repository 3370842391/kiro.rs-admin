# 错误快照持久化设计

**状态：** 已确认

**日期：** 2026-07-14

**目标分支：** `fix/ztest-d3-d7`

## 1. 背景

当前系统已经通过 `traces.db` 保存请求汇总和每次上游尝试，但单次尝试只保留最多 2 KiB 的上游错误片段，不保存客户端请求、转换后的 Kiro 请求、工具配对结构和流式事件尾部。正式端为了排障开启了全量 `RUST_LOG=debug`，结果每个请求都会输出完整请求体、HTTP/2 帧和 Authorization；Docker 采用 50 MiB × 3 轮转，日志在数分钟内被覆盖。

2026-07-13 正式端出现多批 `Invalid tool use format / REQUEST_BODY_INVALID`。`traces.db` 证明错误跨多个凭据稳定复现，但原始请求已经随 DEBUG 日志轮转丢失，只能通过额外黑盒对照推断非法历史 `tool_use.id` 是高置信度根因。现状无法满足持续修复协议、工具和流式故障的需要。

## 2. 目标

1. 每个失败、流中断或重试恢复请求都能通过 trace ID 找到完整错误现场。
2. 保存完整客户文本、工具 JSON、Schema、转换后的 Kiro JSON、上游错误响应和重试链路。
3. 永不保存 Authorization、API Key、refresh token、client secret、Cookie 等认证秘密。
4. 图片、PDF 和其他大型 base64 只保存长度与 SHA-256，不保存原始编码。
5. 正常成功请求不写入大快照，不增加明显延迟。
6. 快照写入故障不能覆盖或改变原本应返回给客户的 API 结果。
7. 支持管理端筛选、查看、下载、永久保留和删除。
8. 独立保留 90 天、自动管理 200GB，并始终为服务器预留至少 100GB 空闲磁盘。

## 3. 非目标

- 不把所有成功请求长期保存为完整正文。
- 不用错误快照替代现有 `traces.db`、usage log 或计费记录。
- 不依赖全量 DEBUG 作为正式端长期取证方式。
- 不在本功能中修复具体工具、Schema、SSE 或签名协议错误；本功能负责稳定保存这些错误的现场。
- 不保存图片/PDF 原始 base64，也不提供恢复这些二进制内容的能力。

## 4. 方案选择

### 4.1 采用：独立 SQLite 压缩快照库

使用 `config/error_snapshots.db`。查询字段与压缩 payload 分表存放，所有 payload 通过 zstd 压缩后作为 BLOB 写入。同一快照的元数据和 payload 在一个事务中提交。

选择原因：

- 单机 1TB 硬盘适合 SQLite WAL；
- 元数据、正文和重试链路可以原子提交；
- 不存在数据库记录与外部文件不一致的问题；
- 管理端可以直接分页、筛选和关联 trace；
- 只有失败请求写大 BLOB，写入量远小于全量请求日志。

### 4.2 未采用：SQLite 索引 + 压缩文件

大 payload 独立存放时写入吞吐更高，但会产生孤立文件、跨介质事务、备份和清理一致性问题。当前单机规模不需要这层复杂度。

### 4.3 未采用：轮转 JSONL

写入简单，但无法高效分页、筛选、手动标记和按 trace 关联，不满足管理端长期排障需求。

## 5. 总体架构

现有轻量路径保持不变：

```text
/v1/messages
  -> RequestTracer
  -> traces.db（每次请求的轻量汇总和 attempts）
```

新增错误快照路径：

```text
/v1/messages
  -> ErrorSnapshotContext（请求内收集，内存态）
     -> 客户端请求
     -> 转换诊断
     -> Kiro 出站请求/attempt
     -> 上游响应/流尾
  -> 请求结束时判断触发条件
     -> 正常成功：丢弃上下文
     -> 失败/中断/恢复：脱敏 -> zstd -> error_snapshots.db
                                      -> 失败时 fallback 目录
```

`traces.db` 通过 `snapshot_id` 与快照关联，但不复制任何大字段。管理端列表只查元数据，用户打开详情或下载时才读取并解压 BLOB。

## 6. 组件边界

### 6.1 `src/anthropic/error_snapshot.rs`

负责请求内采集、触发判断和脱敏：

- `ErrorSnapshotContext`：一个请求一个实例；
- 保存客户端请求、Headers 摘要、Kiro 请求、attempt、事件尾和最终状态；
- 只在最终触发时执行深度脱敏和压缩；
- 生成稳定的 `snapshot_id` 与 `trace_id` 关联；
- 不负责 SQLite 查询和管理端 JSON。

### 6.2 `src/admin/error_snapshot_db.rs`

负责持久化和生命周期：

- 建库、WAL、迁移和事务写入；
- 分页查询元数据；
- 按需读取和解压 payload；
- pin/unpin、单条删除、批量清理；
- 容量统计、90 天清理、200GB 上限、100GB 磁盘保留；
- fallback 文件导入；
- 不解析 Anthropic/Kiro 协议。

### 6.3 `RequestTracer` / `TraceSink`

现有 trace 仍是请求关联主线。扩展点只传递已经脱敏前的请求内诊断事件，不把大字段写进 `TraceAttempt`。`RequestTracer::finalize()` 在生成最终 `TraceRecord` 时同时通知快照上下文最终状态，并把成功写入的 `snapshot_id` 放入 trace。

### 6.4 Kiro provider

provider 每次实际发送上游前，把以下内容交给快照上下文：

- attempt 序号；
- credential ID；
- endpoint；
- 实际 Kiro 请求体；
- HTTP 状态和错误响应；
- 网络错误或超时；
- 单次耗时。

provider 不直接访问 SQLite，避免协议层依赖管理端存储实现。

### 6.5 Admin API 与 UI

后端在 `src/admin/handlers.rs` / `src/admin/router.rs` 增加快照接口。前端在 `admin-ui/src/components/trace-log-page.tsx` 中给有 `snapshotId` 的请求显示“查看快照”，并新增独立详情对话框和查询 hook。

## 7. 触发规则

以下请求保存快照：

1. 最终状态为 `error` 或 `interrupted`；
2. 任意上游 attempt 返回 400、401、403、408、409、422、429 或 5xx；
3. 任意网络错误、超时或客户端连接中断；
4. tool_use/tool_result 不完整、非法、重复或无法配对；
5. 上游工具 JSON 截断、无效或第二次恢复失败；
6. SSE 状态机、事件解析或 UTF-8 解码失败；
7. 空响应、无 assistant 内容或截断输出；
8. structured output、thinking signature、WebSearch 或 PDF 协议处理失败；
9. 首次失败但后续重试成功，记录 `recovered=true`。

纯正常单跳成功请求不保存正文。配额不足、账号限流和认证失败仍保存快照，因为用户要求所有错误可追溯；清理时它们属于较低优先级，先于协议故障删除。

## 8. 严重级别

| 级别 | 条件 | 默认保留 |
|---|---|---|
| `critical` | 工具续轮断裂、协议状态损坏、trace/payload 校验失败 | 自动永久保留 |
| `error` | 400/422/5xx、空响应、解析失败、最终失败 | 90 天 / 容量治理 |
| `warning` | 首次失败但重试恢复、429/认证切换后成功 | 90 天 / 优先清理 |
| `info` | 管理员主动创建的诊断快照 | 90 天，允许手动 pin |

`critical` 和手动 pin 记录不参与普通 200GB 清理；当磁盘空闲空间低于 100GB 时，新快照进入降级模式，但已有 pin/critical 记录仍不自动删除。

## 9. 数据模型

### 9.1 `error_snapshots`

主要字段：

- `snapshot_id TEXT PRIMARY KEY`
- `trace_id TEXT NOT NULL UNIQUE`
- `ts / ts_epoch`
- `model / is_stream`
- `key_id / key_source`
- `final_credential_id / endpoint / http_status`
- `final_status / error_type / severity`
- `error_message`
- `recovered`
- `pinned / retention_exempt`
- `payload_count`
- `original_bytes / compressed_bytes`
- `created_at / updated_at`

索引：时间、trace ID、severity、error_type、HTTP 状态、凭据、pinned。

### 9.2 `error_snapshot_payloads`

主要字段：

- `snapshot_id`
- `seq`
- `kind`：`client_request`、`kiro_request`、`upstream_response`、`tool_diagnostics`、`stream_tail`、`internal_error`
- `attempt`
- `codec`：首版固定 `zstd`
- `content_type`：首版固定 `application/json` 或 `text/plain`
- `part_index / part_count`：超大逻辑 payload 的分片序号与总片数
- `original_bytes`
- `sha256`
- `data BLOB`

主键为 `(snapshot_id, seq)`，并通过外键级联删除。

### 9.3 `snapshot_id` 关联现有 trace

给 `traces` 表增加 nullable `snapshot_id`。老库迁移必须幂等；无快照的历史记录保持 `NULL`。查询请求日志时只返回 ID，不联表读取 payload。

两个数据库不能依赖跨库事务：快照库先提交，随后回写 `traces.snapshot_id`。如果进程恰好在两步之间退出，快照仍可通过唯一 `trace_id` 找到；启动任务会扫描 `snapshot_id IS NULL` 的近期错误 trace 并幂等修复关联。

## 10. 脱敏规则

### 10.1 永久删除的字段

按字段名和 Header 名大小写不敏感匹配并替换为 `[REDACTED]`：

- Authorization / Proxy-Authorization；
- x-api-key / apiKey / adminApiKey；
- accessToken / refreshToken / idToken；
- clientSecret / client_secret；
- Cookie / Set-Cookie；
- credential、password、secret 等认证字段。

只对明确的认证字段做字段级脱敏，不对普通正文中的“token”“key”单词做全局字符串替换，避免破坏复现所需内容。

### 10.2 base64 处理

对 Anthropic image/document source、PDF、data URI 和超过阈值且验证为 base64 的字段，替换为：

```json
{
  "redacted_base64": true,
  "original_bytes": 1234567,
  "sha256": "<hex>"
}
```

工具参数中的普通短字符串即使看似 base64 也不替换；只有协议已知二进制字段或超过阈值且通过严格校验时才处理。

### 10.3 保留内容

- 客户文本和 system 文本；
- 工具名称、description、Schema 和 input；
- tool_use ID、tool_result ID 与 block 顺序；
- Kiro conversationState（认证字段除外）；
- 上游错误体；
- 流式最后若干协议事件。

## 11. 压缩与完整性

- 使用 `zstd` 独立压缩每个 payload；
- 默认压缩级别 3，优先降低错误路径的 CPU 延迟；
- 压缩前计算 SHA-256，读取时校验；
- payload 解压设置最大输出大小，防止损坏数据或压缩炸弹；
- 单 payload 原始大小和压缩大小均入库；
- 单个分片的未压缩上限固定为 16 MiB；脱敏后的逻辑 payload 超过上限时按 UTF-8/JSON 安全边界拆分，使用 `part_index / part_count` 连续保存，读取和下载时按顺序重组，不静默截断。

## 12. 写入与失败处理

### 12.1 正常写入

1. 请求结束，确定需要快照；
2. 在请求内完成脱敏、序列化、SHA-256 和 zstd；
3. 开启 SQLite 事务；
4. 写入 snapshot 和全部 payload；
5. 提交事务；
6. 回写 trace 的 `snapshot_id`；
7. 返回原本的 API 结果。

最终失败和重试恢复都在 API 完成前同步落库，保证进程正常退出时不丢记录。该开销只发生在异常请求。

### 12.2 SQLite 写入失败

- 短暂 busy 时按毫秒级有界重试；
- 仍失败时把已经脱敏和压缩的 envelope 原子写入 `config/error-snapshot-fallback/`；
- fallback 使用临时文件 + rename，避免半文件；
- 启动和定时任务扫描导入，按 snapshot ID 幂等去重；
- fallback 失败时输出不含正文和密钥的高优先级 ERROR；
- 无论快照是否成功，不能改写客户原本的响应状态和错误正文。

### 12.3 磁盘降级

当空闲磁盘低于 100GB：

1. 立即清理最旧、未 pin、非 critical 的 warning/error；
2. 执行 WAL checkpoint 与 incremental vacuum；
3. 空间仍不足时，新快照只保存元数据、工具诊断和上游错误，正文 payload 标记为 `omitted_due_to_disk_pressure`；
4. 管理端和日志持续显示磁盘告警，直到空间恢复。

## 13. 生命周期与容量

- 默认保留：90 天；
- 自动管理上限：200GB；
- 最小空闲磁盘：100GB；
- 每小时容量检查；
- 每天过期清理；
- 清理顺序：过期 warning → 过期 error → 最旧 warning → 最旧 error → 最旧 info；
- pin 和 critical 跳过普通清理；
- 新库启用 `auto_vacuum=INCREMENTAL`；
- 删除后执行有界 incremental vacuum；
- 定期 `wal_checkpoint(TRUNCATE)`；
- 容量依据数据库主文件、WAL、SHM 和 fallback 目录总和计算。

## 14. 配置

在 `Config` 和管理端日志治理设置中增加：

```json
{
  "errorSnapshotEnabled": true,
  "errorSnapshotRetentionDays": 90,
  "errorSnapshotMaxStorageGb": 200,
  "errorSnapshotCaptureRecovered": true,
  "errorSnapshotCaptureBodies": true,
  "errorSnapshotMinFreeDiskGb": 100
}
```

所有字段支持运行时读取和管理端修改。关闭 `captureBodies` 时仍保存元数据、工具诊断、上游错误和流尾。关闭快照功能时不影响 `traces.db`。

## 15. Admin API

- `GET /api/admin/error-snapshots`：分页、筛选元数据；
- `GET /api/admin/error-snapshots/:id`：详情和 payload 清单；
- `GET /api/admin/error-snapshots/:id/payload/:seq`：按需解压一个 payload；
- `GET /api/admin/error-snapshots/:id/download`：下载完整脱敏 JSON 包；
- `POST /api/admin/error-snapshots/:id/pin`：永久保留；
- `POST /api/admin/error-snapshots/:id/unpin`：恢复自动治理；
- `DELETE /api/admin/error-snapshots/:id`：删除单条；
- `POST /api/admin/error-snapshots/cleanup`：立即执行治理；
- `GET /api/admin/error-snapshots/storage`：容量、保留数、fallback 和磁盘告警。

所有接口继续使用现有 Admin 鉴权。下载响应明确设置为附件，不让浏览器直接执行内容。

## 16. Admin UI

请求日志有 `snapshotId` 时显示“查看快照”。详情界面分为：

- 概览；
- 客户端请求；
- Kiro 出站请求；
- 上游响应；
- 工具诊断；
- 流式事件尾；
- 重试链路。

支持复制当前 JSON、下载完整快照、pin/unpin 和删除。列表支持 trace ID、模型、错误类型、HTTP 状态、凭据、时间、severity、recovered 和 pinned 筛选。大 payload 仅在用户打开对应页签时请求。

## 17. 日志策略

快照系统上线并验证后，正式端建议使用：

```text
RUST_LOG=info,h2=warn,hyper=warn,hyper_util=warn,reqwest=warn,rustls=warn
```

INFO/WARN/ERROR 只记录 trace ID、snapshot ID、错误类型、状态码、大小和落库结果，不记录认证头或完整正文。全量 DEBUG 仅在短期人工诊断时开启。

## 18. 测试策略

### 18.1 单元测试

- 认证字段脱敏；
- 客户文本和工具 JSON 保真；
- image/PDF/base64 hash 替换；
- 中文 UTF-8、嵌套 Schema、超长 JSON；
- zstd round-trip 与 SHA-256 校验；
- tool_use/tool_result ID、配对和 block 顺序诊断；
- severity 与触发规则。

### 18.2 数据库测试

- 新库建表和旧库幂等迁移；
- snapshot/payload 原子事务；
- 列表查询不读取 BLOB；
- 90 天、200GB、100GB 空间和 pin/critical 规则；
- busy、损坏、fallback 写入和幂等导入；
- WAL checkpoint 与 incremental vacuum；
- 清理后 trace 关联正确处理。

测试使用小容量阈值模拟 200GB，不创建真实巨型文件。

### 18.3 请求链路测试

- 正常成功请求无大快照；
- 400/422/500 保存完整现场；
- 流式中断、空响应、工具 JSON 截断保存事件尾；
- 首次失败后恢复写入 `recovered=true`；
- 快照写入失败不覆盖原 API 结果；
- trace、attempt、credential、snapshot 关联一致。

### 18.4 8991 黑盒验收

1. 发送历史 ID 为 `tool/get_weather/1` 的工具续轮；
2. 确认原始 400 被快照；
3. 通过 trace ID 在管理端定位；
4. 查看客户端请求、Kiro 请求和上游响应；
5. 下载并扫描，确认不存在密钥和 base64；
6. 制造流式断开、空响应和截断工具 JSON；
7. 验证恢复成功请求也有快照；
8. 并发错误请求，确认无记录丢失；
9. 验证正常请求延迟、Token 和缓存口径无变化。

## 19. 上线顺序

1. 先实现存储、脱敏和单元测试；
2. 接入 RequestTracer/provider，仍保持管理端入口隐藏；
3. 在 8991 制造错误并验证快照；
4. 实现 Admin API；
5. 实现 UI 查看、下载、pin 和删除；
6. 在 8991 做并发、磁盘和 fallback 验收；
7. 正式端部署后先保留 INFO 日志；
8. 确认错误快照可用后关闭全量 DEBUG。

## 20. 客户影响

- 正常成功请求：不压缩、不写 BLOB，只有轻量上下文引用，预期无明显延迟变化；
- 失败请求：返回前增加脱敏、压缩和事务写入时间，通常为毫秒到数十毫秒；
- 重试恢复请求：同样增加一次异常快照写入，但保留最终成功结果；
- 快照系统自身失败：不改变客户响应；
- 不修改对话正文、工具参数、Token 计费、缓存拆分或模型行为。

## 21. 完成定义

只有同时满足以下条件才算完成：

1. 每类目标错误均能通过 trace ID 找到快照；
2. 客户请求、Kiro 请求、上游响应、工具诊断和流尾按设计落库；
3. 数据库和下载内容不含认证秘密或原始 base64；
4. 重启后快照存在，fallback 能自动恢复；
5. 90 天、200GB、100GB 空间和 pin/critical 规则可验证；
6. 管理端可以筛选、查看、下载、pin/unpin 和删除；
7. 关闭全量 DEBUG 后仍能定位 `REQUEST_BODY_INVALID` 的具体结构；
8. 正常请求延迟、Token、缓存和对话行为无回归；
9. 所有 Rust、Admin UI 和 8991 黑盒测试通过；
10. 未修改正式 8990，除非用户在完成验收后明确要求部署。
