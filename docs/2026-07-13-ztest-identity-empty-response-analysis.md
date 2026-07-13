# Ztest D1、模型身份探针与空响应问题分析

> 日期：2026-07-13
> 范围：生产 `8990`、公开测试实例 `8991`、当前 `kiro-rs 0.9.1`
> 本文只记录证据、根因判断和下一轮方案，尚未修改业务代码或服务器路由。

## 1. 结论摘要

本轮实际存在三个彼此独立的问题，不能用同一个补丁处理：

1. **8991 的 Ztest 0 分不是 AI 无法回复。** 公网 API、临时 Key、非流式和流式生成均已实测成功；但 Ztest 的 D1 检测没有形成任何一条 8991 `/v1/messages` trace。现有证据表明失败发生在 Ztest 后端到 `IP + HTTP + 8991` 的连接/协议探测阶段，尚未进入 RS 的消息处理链路。
2. **`context_window` 和 `recent_event` 失败的根因已复现。** 本地资料和开关都正常；严格探针只要携带一个非空 Claude Code `system`，当前分类器就拒绝本地回答并放行到 Kiro，上游随后给出 `I can't discuss that.` 或“不知道可靠截止日期”。
3. **`upstream returned no assistant content` 的上游起点在 Kiro，但 RS 仍有处理缺口。** Kiro 返回 HTTP 200，却没有产生可见正文、thinking 或完整工具调用；RS 当前会丢失大部分 event-stream 内的 `Error/Exception` 详情，也不会对纯空响应做一次安全重试，因此客户只看到泛化错误。

## 2. 环境与配置证据

### 2.1 版本

- 生产容器镜像标签仍显示 `ghcr.io/3370842391/kiro-rs:sha-218ae0`，但容器内二进制已在线更新为 `kiro-rs 0.9.1`。
- 公开测试容器运行镜像 `kiro-rs-test:61a2d53a013c`，二进制为 `kiro-rs 0.9.1`，健康状态为 `healthy`。
- 测试站公网地址为 `http://43.225.196.10:8991`，管理端路径为 `/admin`；根路径 `/` 返回 404 是当前路由设计，不代表 API 离线。

### 2.2 模型资料

生产资料文件 revision 为 1，目标资料已存在：

| 模型 | 上下文窗口 | 最大输出 | 知识截止 | 来源 |
|---|---:|---:|---|---|
| `claude-opus-4-8` | 1,000,000 | 128,000 | 2026-01 | Kiro + models.dev |
| `claude-sonnet-5` | 1,000,000 | 128,000 | 2026-01-31 | Kiro + models.dev |

`modelProfileExactAnswersEnabled` 未显式写入配置，按当前默认值等价于 `true`。因此问题不是开关关闭，也不是 `claude-opus-4-8` 缺少资料。

## 3. Ztest 8991 D1 离线问题

### 3.1 报告表现

Ztest 在 D1 阶段直接判定：

- `d1_offline`
- “API 端点不响应或返回非法格式”
- “Claude Code 客户端校验失败”
- 后续协议、身份、能力和安全项全部跳过

### 3.2 公网实测

使用同一个临时 `csk` 从公网完成了以下调用：

| 调用 | 结果 | 耗时 |
|---|---|---:|
| `GET /v1/models` | HTTP 200，返回 34 个模型 | 约 1 秒 |
| 非流式 `POST /v1/messages` | HTTP 200，正文 `TEST_OK` | 2.54 秒 |
| 流式 `POST /v1/messages` | HTTP 200，8 个 SSE 事件，正文 `STREAM_OK` | 2.64 秒 |
| 上下文窗口严格探针 | HTTP 200，`1000000` | 0.87 秒 |
| 知识截止严格探针 | HTTP 200，`January 2026` | 0.88 秒 |

这证明 8991 的公网端口、鉴权、模型调用、非流式转换和 SSE 转换均可工作。

### 3.3 服务器侧反证

Ztest 报告生成后，8991 的 `traces.db` 中仍只有人工复测产生的两条上游调用记录，没有 Ztest 的 D1 请求。两个严格身份探针因为本地返回，本来就不会形成上游 trace；但 Ztest 的协议验证如果通过鉴权并进入 `/v1/messages`，至少应产生接收日志或 trace，实际均未出现。

因此当前可以确认：

- Ztest D1 失败发生在 RS handler 之前；
- 不能把这份 0 分报告用于判断模型或转换器质量；
- 目前还不能仅凭报告区分是 Ztest 的 SSRF/端口策略、明文 HTTP 限制、到该 IP 的路由问题，还是它使用了不兼容的探测路径。

### 3.4 推荐的基础设施处理

下一次检测前，给 8991 增加一个标准公网入口：

1. 使用独立域名，例如 `https://rs-test.example.com`；
2. 由 Nginx/Caddy 在 443 终止 TLS，再反向代理到 `127.0.0.1:8991`；
3. SSE 路由关闭代理缓冲，保持长连接；
4. 记录最小访问日志：时间、源 IP、方法、路径、状态码、耗时，不记录 Key 和正文；
5. Ztest 只填写 HTTPS 域名，不再填写裸 IP、HTTP 和非标准端口。

这是优先级最高的前置工作。只有 D1 请求确实到达测试实例，后续 Ztest 日志才具有诊断价值。

## 4. `context_window` 与 `recent_event` 不匹配

### 4.1 对照实验

对 `claude-opus-4-8` 使用相同 prompt：

| 请求差异 | context_window | recent_event |
|---|---|---|
| 无 `system` | `1000000` | `January 2026` |
| 仅增加 `You are Claude Code, Anthropic's official CLI for Claude.` | `I can't discuss that.` | `I don't have a documented knowledge cutoff date...` |

第二组与用户看到的生产报告高度一致，且耗时从不足 1 秒上升到约 9 秒，说明请求确实绕过了本地回答并进入 Kiro。

### 4.2 代码原因

`classify_profile_probe` 当前采用 fail-closed 规则。出现以下任意条件就拒绝本地资料回答：

- 消息数量不是 1；
- 非 user 消息；
- `tools` 字段存在，包含空数组也算；
- 存在 `tool_choice`；
- 存在 `thinking`；
- 存在 `output_config`；
- 存在 Web Search 标志；
- **任意非空 `system` 文本。**

Claude Code 协议探测通常会带官方身份 system。当前规则把这类安全且确定的身份锚点与任意业务 system 同等处理，因此资料功能在真实 Claude Code 检测链路中不可达。

### 4.3 建议修复边界

建议只放宽确定安全的白名单，不做宽泛关键词匹配：

1. 继续要求单轮、单 user、单 text block、严格 prompt 模板；
2. 允许空 `tools`，但继续拒绝非空工具列表和所有 `tool_choice`；
3. 允许已验证的 Claude Code 官方身份 system 精确行；
4. system 中出现任何额外非空行时继续拒绝；
5. 继续拒绝 thinking、output_config、Web Search、多模态和多轮历史；
6. 增加只记录“未命中原因码”的 debug 日志，例如 `system_not_whitelisted`、`tools_nonempty`，不记录正文。

这样只会改变两个严格资料探针的本地回答，不会改写普通客户对话、工具参数或多轮历史。

## 5. `upstream returned no assistant content`

### 5.1 生产证据

截图对应请求的 trace 显示：

- 模型：`claude-sonnet-5`
- 非流式
- effort：`high`
- thinking：开启
- Kiro 端点：`ide`
- 上游 HTTP：200
- 重试次数：0
- 最终错误：`upstream returned no assistant content`

同一时段不同凭据均出现该错误，排除了单个凭据损坏。生产 trace 聚合中至少包括：

| 请求形态 | 失败数 |
|---|---:|
| `claude-sonnet-5`、非流式、high thinking | 181 |
| `claude-opus-4-8`、非流式、high thinking | 178 |
| `claude-sonnet-5`、流式、high thinking | 111 |
| `claude-opus-4-8`、流式、high thinking | 21 |

此外，10:22:38、10:22:43、10:22:54、10:22:58 的失败与 handler 日志中的 `message_count=668` 一一对应。这说明截图问题主要出现在非常长的会话历史上，不是简单 prompt 必现。相同模型的短请求在 8991 上可正常返回 `SONNET5_OK`。

### 5.2 当前数据流缺口

Kiro 的 HTTP 200 响应体仍是 event-stream，内部可能包含：

- `AssistantResponse`
- `ReasoningContent`
- `ToolUse`
- `ContextUsage`
- `Error`
- `Exception`
- `Metering`

当前非流式收集器只把正文、thinking 和工具调用转换为客户内容。除 `ContentLengthExceededException` 外，其余 `Error/Exception` 没有进入最终错误状态。若上游只返回异常、上下文占用或计量事件：

1. Provider 因 HTTP 200 把尝试记为 success；
2. 转换器最终得到空 `content`；
3. `validate_non_stream_content` 返回统一的 `upstream returned no assistant content`；
4. 现有安全重试只识别“不完整工具 JSON”，不识别纯空响应；
5. 客户看不到真正的上游异常类型，也不会自动切换凭据再试一次。

### 5.3 根因归属

- **上游起点：Kiro。** 一个成功的 HTTP 200 请求没有提供任何可交付的 assistant 内容，长上下文下尤其集中。
- **RS 可修复部分：异常保真、分类和安全恢复。** 当前 RS 把具体上游事件压缩成了过于泛化的错误，而且没有利用“尚未向客户提交任何语义内容”这一安全重试窗口。

因此不能简单说“完全是 Kiro、我们无能为力”，也不应该伪造正文把错误当成功。

### 5.4 建议修复顺序

1. **保存上游终止原因**
   在 attempt 状态中保存首个 `Error/Exception` 的类型和截断消息，并统计实际收到的事件种类。
2. **优先返回准确错误**
   - 上下文达到 100%：返回 `context_length_exceeded`；
   - 上游显式 validation/model exception：返回对应的 upstream protocol error；
   - 真正零事件/只有计量事件：才使用 `upstream_empty_response`。
3. **增加一次受控重试**
   仅当尚未向客户端转发正文、thinking 或工具调用时，对“纯空响应/可重试上游异常”切换凭据或端点重试一次。若已经转发任何语义内容，绝不重试，避免重复工具调用或重复文本。
4. **保留长上下文语义**
   不应静默删除客户历史。若估算已接近模型真实上限，应在调用前返回明确错误；是否自动压缩历史需要单独设计和用户开关。
5. **补充 trace 字段**
   建议增加 `upstream_event_kinds`、`upstream_exception_type`、`semantic_output_started` 和 `retry_reason`，消息只保留安全截断摘要。

### 5.5 客户影响

- 准确错误映射不会改变正常成功响应。
- 一次空响应重试会让极少数失败请求增加一次上游调用和延迟，但能显著减少瞬时空响应。
- 严格限制为“尚未输出语义内容”后，不会重复已发出的工具调用，也不会造成对话断裂或正文重复。
- 长上下文预检若直接拒绝，会让客户更早看到明确的上下文错误，而不是等待数秒后得到模糊的空内容错误。

## 6. 下一轮建议执行顺序

1. 先为 8991 配置 HTTPS 443 测试域名和最小访问日志；
2. 重新运行 Ztest，确认 D1 请求实际进入 RS；
3. 根据真实请求形态实现 Claude Code system 精确白名单；
4. 为上游 Error/Exception 保真和空响应安全重试编写失败测试；
5. 先部署 8991，复测普通对话、流式、工具调用、长上下文和 Ztest；
6. 证据稳定后再合并并更新生产 8990。

## 7. 本轮未执行的操作

- 未修改 Rust、前端或 Docker 业务代码；
- 未修改生产或测试配置；
- 未给 8991 增加域名/TLS；
- 未提交或推送本分析文档；
- 文档未记录管理 Key、临时 `csk` 或任何凭据正文。
