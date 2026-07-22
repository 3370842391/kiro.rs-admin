# RS 利润报表与 NewAPI 分组核算设计

## 目标

在 RS 管理端增加可持久化配置的利润报表：按 Kiro 上游真实 `meteringEvent.usage` 计算成本，连接 NewAPI 消费日志计算收入，并按 RS 客户端 Key、Key 分组、模型和 NewAPI 用户拆分，准确识别 `0.05`、`0.08` 等不同分组的盈亏。

## 范围

包含：

- RS 返回 `X-Oneapi-Request-Id`，让 NewAPI 能把消费日志的 `upstream_request_id` 与 RS trace 关联。
- RS 管理端保存 NewAPI 地址、访问令牌、管理员用户 ID、Kiro Credit 单价和默认 NewAPI quota 单位。
- 服务端调用 NewAPI `/api/log/` 分页获取消费日志。
- 使用 RS trace 中的 `key_id`、真实 Credits、模型和状态计算成本并建立分组明细。
- 新增“利润”管理页、配置编辑、时间范围和汇总表。
- 匹配失败单独统计，不再隐式按 1 Credit 计费。

不包含：

- 不改变客户请求正文、模型映射、缓存拆分或上游调度。
- 不改变 NewAPI 的实际收费规则；报表只读取 NewAPI 已产生的账单。
- 不把 NewAPI 的缓存倍率再次乘到收入上；收入以 NewAPI 返回的 `quota` 为准。

## 成本与收入口径

默认采购口径为 ¥45 / 2000 Credits，因此默认单价为：

```text
creditPrice = 45 / 2000 = 0.0225 元/Credit
```

每条已匹配记录：

```text
收入 = newapi.quota / quotaPerUnit
成本 = max(0, rsTrace.credits) * creditPrice
利润 = 收入 - 成本
```

RS trace 的 `credits` 为上游 `meteringEvent.usage` 累加值。若历史记录没有 Credits，则该记录进入 `missingCost` 计数并显示为成本不完整，不用请求次数替代真实消耗。

## 关联链路

1. API 请求进入 RS 后生成一个 trace ID，并通过 `X-Oneapi-Request-Id` 响应头返回。
2. NewAPI 记录该值为 `upstream_request_id`。
3. 利润报表从 NewAPI `/api/log/?type=2` 获取消费日志。
4. RS 通过 trace ID 找到 `key_id`、模型、真实 Credits 和状态。
5. RS 从客户端 Key 管理器解析 Key 名称和分组；`key_id=0` 显示为系统默认 Key。
6. 未匹配 NewAPI 日志、RS trace 缺失或 NewAPI 没有 `upstream_request_id` 的记录单独列出。

## 配置与安全

配置保存在 RS 的 `config.json`，字段使用 camelCase：

- `profitNewapiBase`
- `profitNewapiToken`
- `profitNewapiUser`
- `profitCreditPrice`（默认 `0.0225`）
- `profitQuotaPerUnit`（默认 `500000`）

管理端接口只返回 `tokenConfigured: true/false`，读取配置时不返回 Token 明文；更新请求中省略 Token 时保留原值。报表请求仅允许管理员 API Key 调用。

## 管理端接口

- `GET /api/admin/config/profit`：读取脱敏配置。
- `PUT /api/admin/config/profit`：更新配置并持久化。
- `POST /api/admin/profit/report`：按时间窗口拉取 NewAPI 日志并返回汇总。

报表响应包含：时间窗口、配置口径、总行数、匹配数、未匹配数、缺少成本数、收入、Credits、成本、利润、毛利率，以及 `byKey`、`byGroup`、`byModel`、`byUser` 明细。

## 前端交互

新增“利润”Tab：

- 配置表单：NewAPI 地址、管理员用户 ID、访问令牌、Credit 单价、quota 单位。
- Token 输入框只显示是否已配置，保存时留空表示不修改。
- 时间范围支持 30 分钟、2 小时、24 小时、7 天。
- 顶部显示收入、实际 Credits、成本、利润、匹配率。
- 明细表默认按分组排序，并可展开到 Key、模型和用户。
- 负利润使用警示色，未匹配记录显示明确原因。

## 兼容性与失败处理

- NewAPI 无法访问时返回明确的 502 管理错误，不影响 `/v1` 客户请求。
- NewAPI 分页部分失败时不返回不完整利润结果，避免误导。
- NewAPI 日志没有 `upstream_request_id` 时显示“无法关联”，不进行模糊时间匹配。
- 现有旧配置没有新增字段时使用默认值，启动和客户 API 行为保持不变。
- 新增响应头对客户不可见，不修改 SSE 数据；NewAPI 会使用该头进行内部关联。

## 验证计划

- Rust 单元测试：成本公式、默认单价、匹配/未匹配、Key/分组聚合、NewAPI 分页和错误处理。
- Rust 路由测试：`X-Oneapi-Request-Id` 在流式和非流式成功响应中存在。
- 前端测试：利润配置脱敏、空 Token 保留、负利润和分组明细渲染。
- `cargo test`、`cargo fmt --check`、`cargo clippy --all-targets --all-features -- -D warnings`。
- `admin-ui` 执行 `bun test` 和 `bun run build`。

