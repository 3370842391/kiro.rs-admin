# 利润计费账本准确性设计

## 背景与问题

当前利润报表以 NewAPI `logs.upstream_request_id` 精确关联 RS `traces.db.trace_id`，只有关联成功的 trace 才贡献 Credits 成本。生产现场最近两小时出现以下证据：

- NewAPI RS 渠道有约 1.1 万条消费日志；
- `usage_log` 有约 1.08 万条成功记录、约 8894 Credits；
- `traces.db` 只覆盖其中少量请求，利润页匹配率约 7.39%；
- 页面因此只显示约 326 Credits 和数元成本，而号池实际已消耗约 8900 Credits。

`usage_log` 在请求完成时已记录上游真实 `meteringEvent.usage`，是成本事实来源；`traces.db` 是诊断数据，不应决定成本是否进入利润报表。

## 目标

1. 总 Credits、总成本、总利润以 RS `usage_log` 为权威来源，即使 trace 缺失也不得漏算。
2. 新请求通过 `traceId` 将 NewAPI 收入与 RS usage 成本精确关联，可继续按 Key、分组、模型、用户拆分。
3. 旧 usage 记录没有 `traceId` 时，其成本仍进入总成本，并单独显示为“未归属成本”。
4. 自动识别本次时间窗口内实际命中 RS 的 NewAPI 渠道，排除同一 NewAPI 实例中的 GPT 等非 RS 渠道。
5. 不修改客户请求、响应、SSE、缓存、工具调用和 NewAPI 实际扣费逻辑。

## 非目标

- 不修改 Kiro Credits 的计量公式；继续直接采用上游 `meteringEvent.usage`。
- 不回填历史 JSONL 的伪造 `traceId`。
- 不依赖时间、模型或用户名进行模糊单请求匹配。
- 不把诊断 trace 改造成计费数据库。

## 方案概述

复用现有 `usage_log.YYYY-MM-DD.jsonl` 作为轻量计费账本，不新增第二套高频写入数据库：

1. `UsageRecord` 新增可选 `traceId`。旧 JSONL 没有该字段时正常反序列化为 `None`。
2. `UsageRecordHook` 在请求入口保存中间件生成的 `x-oneapi-request-id`，每次落 usage 时一并写入。
3. `UsageRecorder` 新增精确时间范围读取接口，只扫描覆盖查询窗口的日期文件，读取前先 flush 当前 writer。
4. 利润报表同时读取 NewAPI 日志、usage 账本和 legacy trace：
   - 新数据优先用 `usage.traceId` 精确关联；
   - 没有带 `traceId` 的 usage 时，允许用 legacy trace 补充分组/Key 归属；
   - 总成本始终来自查询窗口内的 usage Credits，而不是 trace Credits。
5. 从精确关联记录中收集 NewAPI `channelId` 和 RS `keyId`：
   - 收入只统计已识别 RS 渠道；
   - 成本只统计已识别 RS Key；
   - 若窗口内完全无法识别渠道或 Key，则 fail closed，不给出虚假的利润，而是在响应中标记账本范围未确认。

## 数据模型

### UsageRecord

新增字段：

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub trace_id: Option<String>,
```

保留原有 `key_id`、`credential_id`、`model`、`credits`、`status`。成本包含成功和失败记录中所有大于零的 Credits，因为上游已下发的 metering 均构成真实消耗。

### NewapiLogItem

新增：

```rust
#[serde(default)]
pub channel_id: u64,
```

### ProfitReport

保留现有字段以兼容前端，并新增：

```rust
pub attributed_credits: f64,
pub unattributed_credits: f64,
pub attributed_cost: f64,
pub unattributed_cost: f64,
pub attributed_revenue: f64,
pub unattributed_revenue: f64,
pub observed_channel_ids: Vec<u64>,
pub observed_key_ids: Vec<u64>,
pub ledger_scope_confirmed: bool,
```

字段语义：

- `credits` / `cost`：窗口内已识别 RS Key 的 usage 总量与总成本；
- `attributed_*`：能够通过 `traceId` 或 legacy trace 精确归属的数据；
- `unattributed_*`：确定属于 RS 范围，但无法落到具体 NewAPI 请求或 RS Key 分组的数据；
- `profit = revenue - cost`；
- `marginPct = profit / revenue * 100`；
- 分组表仅显示已归属部分，因此可能小于顶部总计，差额由警示条明确展示。

## 关联与汇总算法

1. 拉取时间窗口内 `type=2` 的 NewAPI 消费日志，并保留 `channelId`。
2. 从 usage JSONL 精确读取 `[start, end]` 内所有记录。
3. 构建 `usage_by_trace_id`，同一 `traceId` 若出现多条记录则累加 Credits，但仅计一次 NewAPI 收入。
4. 查询相同 NewAPI ID 集合的 legacy trace，作为旧记录的归属补充。
5. 对每条 NewAPI 日志：
   - 命中 usage `traceId` 时记录 `channelId -> keyId`，使用 usage Credits；
   - 否则命中 legacy trace 时记录 `channelId -> keyId`，归属信息来自 trace，但成本总量仍以 usage 为准；
   - 两者均未命中时暂不确认其属于 RS。
6. 汇总观察到的 RS `channelId` 与 `keyId`。
7. 收入范围为观察到的 RS 渠道；成本范围为观察到的 RS Key 对应的全部 usage 记录。
8. 能按 `traceId` 命中的 usage 计入已归属成本；其余同 Key usage 计入未归属成本。
9. 如果完全没有观察到 RS 渠道或 Key：
   - `ledgerScopeConfirmed=false`；
   - 返回 NewAPI 原始日志数量供诊断；
   - `profit` 和 `marginPct` 不伪装成可信结果，前端显示“范围未确认”。

## 历史兼容

- 旧 JSONL 不含 `traceId`，反序列化不失败。
- 旧窗口仍能通过少量 legacy trace 识别 RS Key/渠道，然后把同 Key 的全部 usage Credits 纳入总成本。
- 新版本上线后，新增 usage 均带 `traceId`，归属率会随时间窗口推进而快速接近 100%。
- `traces.db` 关闭、写入失败或保留期更短，不再导致总成本归零。

## 前端展示

利润 KPI 保持“收入、Credits、成本、利润、毛利率、匹配率”，但调整说明：

- “匹配率”改名为“归属率”，分母为 RS 渠道收入记录，分子为已归属记录；
- 警示条同时展示未归属收入、未归属 Credits、未归属成本；
- 增加说明：“顶部总成本来自 RS 实际 metering 账本，分组表仅展示可精确归属部分”；
- `ledgerScopeConfirmed=false` 时使用红色阻断提示，不显示误导性的利润结论。

## 错误处理与性能

- usage 文件读取失败时返回明确 500，不回退到 trace 成本并伪装成功。
- 单行 JSON 损坏时记录文件名与行号，跳过坏行并在报告 warning 中计数；其他正常行继续统计。
- 只扫描查询窗口涉及的日期文件，30 分钟、2 小时、24 小时通常只读 1–2 个文件。
- 7 天窗口允许顺序扫描 JSONL；不把整个文件一次性载入内存。
- 当前 writer 在读取前 flush，保证刚完成请求立即可见。

## 测试策略

1. `UsageRecord` 旧 JSON 无 `traceId` 仍可读取，新记录正确序列化字段。
2. 时间范围读取跨日、边界闭合、坏行计数、writer flush。
3. 新 usage 精确匹配时，成本来自 usage 而不是 trace。
4. trace 缺失但已识别同 Key 时，旧 usage Credits 进入未归属成本。
5. 非 RS NewAPI 渠道不会进入收入。
6. 成功与错误 usage 中的大于零 Credits 都进入真实成本。
7. 顶部总成本与分组成本差额正确显示。
8. 无法确认 RS 渠道/Key 时 fail closed。
9. 后端全量测试、前端全量测试和生产构建通过。

## 客户影响

- 客户请求与响应完全不变。
- 每次 usage JSONL 仅增加一个短 UUID 字段，磁盘与 CPU 增量很小。
- 管理员看到的历史利润可能显著下降，这是修复漏算后的真实成本，不是新增扣费。
