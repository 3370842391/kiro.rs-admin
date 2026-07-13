# 严格 JSON 长上下文误判修复设计

## 事故结论

生产环境错误 `Upstream did not produce one complete JSON value after one retry` 不是网络、凭据或上游 HTTP 故障。对应 trace 的两次上游尝试均为 HTTP 200，错误由 rs 的严格 JSON 恢复分支主动产生。

截至调查时，trace 数据库中至少存在 5 次同类失败：

- 请求均为 `claude-opus-4-8`、非流式、未开启 thinking。
- 输入规模约 13.5 万至 14.7 万 tokens。
- 每次调用两个不同凭据，上游均返回 HTTP 200。
- 同一形状附近也存在第二次恢复成功的请求，说明上游输出具有随机性，而不是解码器全局损坏。

根因是 `strict_json_requested()` 对最新 user 消息全文执行互不相邻的 `contains()`：只要任意位置分别出现 `json`、`exactly one` 和 no-extra 提示，就会进入严格 JSON 路径。实际失败请求的当前消息约 55 万字符，包含 480 个 `json` 和 9 个 `exactly one`，主要来自代码、测试注释、历史工具参数与序列化上下文；它并没有要求本轮只输出 JSON。

普通 Claude Code 工具工作流因此被误判为严格 JSON 请求。上游正常返回文本或工具行为后，rs 把它视为无效 JSON，再重试一次并最终主动中断对话。

## 目标

- 只在最新用户指令明确要求单个纯 JSON 输出时启用严格 JSON 恢复。
- 超长代码、历史、工具参数中的零散 `json` / `exactly one` 不得组合成命中。
- 保持 Ztest 小型严格 JSON 探针和真实 JSON-only 客户端请求可用。
- 保持已有的单 JSON 提取、显式字段约束验证和最多一次恢复重试语义。
- 不把真正的严格 JSON 失败静默降级为普通文本。
- 为未来失败记录可区分的拒绝原因，不记录完整模型输出或敏感正文。

## 检测器设计

### 指令尾窗口

只检查最新 user 文本末尾最多 4096 个 UTF-8 字节，并确保截取点位于字符边界。Claude Code 将大量历史和代码拼入同一消息时，当前动作通常位于尾部；Ztest 严格 JSON 提示本身远小于该窗口。

如果长消息确实要求 JSON，客户端可以在最终指令中明确表达，例如 `Return exactly one JSON object and no explanation`，仍会被尾窗口识别。

### 局部共现

不再对整个文本分别执行全局 `contains()`。检测器按行及句子边界构造最多 512 字符的局部指令片段；同一片段必须同时满足：

1. 包含 JSON 目标：`json`。
2. 包含输出命令：`return`、`reply`、`respond`、`output`、`provide`、`只返回`、`仅返回`、`回复` 或 `输出`。
3. 包含单值约束：`exactly one`、`exactly a single`、`single minified`、`one minified`、`只返回` 或 `仅返回`。
4. 包含无额外内容约束：沿用现有 `has_no_extra_cue()`，但只在该局部片段上判断。

这样代码中的 `Exactly one purchase should succeed` 与其他位置的 `json.Marshal` 不会跨数十万字符拼成命中。

### 安全边界

- 小型明确提示继续命中：`Reply with exactly one minified JSON object and no markdown or explanation.`
- 超长消息尾部明确要求 JSON 继续命中。
- 超长代码正文含许多 JSON/Exactly one，但尾部是普通构建、测试或工具请求时不命中。
- 只有 `return JSON`、没有单值和 no-extra 约束时不命中，仍交给普通上游路径。
- tools、tool_choice、thinking、web search 和 document 的既有 route guard 保持不变。

## 恢复失败诊断

`strict_json_from_events()` 的结果由单一 `Option<String>` 改为内部枚举，仅用于诊断：

```rust
enum StrictJsonAttemptResult {
    Valid(String),
    ToolUse,
    NoCompleteValue,
    MultipleValues,
    ConstraintMismatch,
    TerminalError,
}
```

对外协议不增加字段。日志和 trace error message 仅记录安全原因标签、attempt 数、可见文本字节数，不记录正文：

```text
strict JSON recovery exhausted: attempts=2 reason=no_complete_value visible_bytes=137
```

如果实现诊断枚举会扩大本轮改动或改变既有提取语义，则第一提交只完成检测器误判修复；诊断增强作为第二个独立提交，不阻塞生产修复。

## 测试设计

### RED 回归

在 `src/anthropic/exact_output.rs` 新增真实形状的最小化回归：

1. 构造超过 500KB 的前缀，反复包含 `json.Marshal`、`Exactly one purchase should succeed` 和历史 `no markdown`。
2. 尾部追加普通指令：`Build the project and run the tests.`。
3. 断言 `strict_json_requested()` 为 false。

该测试在当前全局 `contains()` 实现下必须失败，证明能复现生产误判。

### 兼容回归

- 小型 Ztest 风格严格 JSON 提示返回 true。
- 500KB 上下文尾部追加明确 JSON-only 指令返回 true。
- JSON、single、no-extra 分散在不同远距离段落时返回 false。
- 中文 `仅返回一个 JSON 对象，不要解释` 返回 true。
- 现有 tools/thinking/document route guard 测试继续通过。
- 现有提取有效 JSON、截断 JSON、字段约束不匹配和第二次恢复成功测试继续通过。

## 验证环境

本机在基线链接阶段因 Windows 页面文件不足报 `os error 1455`，不是断言失败。实现仍在独立 worktree 完成；RED、GREEN、全量 Rust 测试和 `cargo check` 使用 40 核、62GB 内存的 `43.225.196.10` 测试构建环境执行。本机负责 `rustfmt --check`、diff/secret 检查和前端构建。

## 发布与回退

- 修复先进入独立分支 `fix/strict-json-false-positive`。
- 不直接替换生产 8990；先在独立 8991 测试容器运行回归和真实 Claude Code 工具调用。
- 生产部署后观察 `strict JSON recovery exhausted` 数量以及普通工具调用中断率。
- 如真实 JSON-only 客户端出现漏检，可回退该单一提交；不得恢复当前全文全局共现逻辑，应调大尾窗口或扩充局部命令词。

## 非目标

- 不增加 JSON 重试次数。
- 不修改上游模型、凭据调度、token 或缓存计算。
- 不为 Ztest 报告 ID、nonce 或固定提示硬编码。
- 不把普通工具响应伪造成 JSON。
