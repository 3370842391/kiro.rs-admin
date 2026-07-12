# Ztest 剩余问题证据化修复实施计划

## 目标

在不伪造 thinking、system 优先级、token/cache 或官方直连身份的前提下，修复已由生产 trace 和重放确认的 token 估算、Claude Code system 身份冲突、流式身份短语、文本 PDF 唯一标识符空响应，并补足工具事件诊断。

## 任务 1：修正本地 token 估算器

1. 在 `src/token.rs` 添加旧倍率必然失败的测试：短文本不放大、99/100 基础 token 边界连续、长度翻倍近似翻倍、中英文单调且非零。
2. 单独运行 token 测试确认 RED 原因是分段倍率。
3. 将实现改为字符单位除以四后向上取整，保持调用方总量最小值语义。
4. 运行 token 测试确认 GREEN。

## 任务 2：移除 Claude Code 冲突身份锚点

1. 在 converter 测试中覆盖 ClaudeCode 仅身份行、身份行加其他规则、Raw 原样保留、thinking prefix 与剩余 system 顺序。
2. 运行定向测试确认旧实现 RED。
3. 为 `push_system_history` 传入兼容模式，增加只删除精确身份行的纯函数。
4. 运行 converter 定向测试确认 GREEN。

## 任务 3：修复流式完整身份短语跨 chunk 归一化

1. 添加完整身份描述跨多个 chunk、所有 UTF-8 合法切点、中文/emoji 边界和普通 AWS 内容不变测试。
2. 运行 identity 测试确认旧实现 RED。
3. 将 pending 逻辑扩展为所有已知源短语的最长严格尾部前缀，输出部分继续复用非流式归一化。
4. 运行 identity 测试确认 GREEN。

## 任务 4：实现文本 PDF 唯一标识符检测纯函数

1. 添加明确 only/exact 请求且唯一匹配、多匹配、模糊请求、非法/过长候选、无文档等测试。
2. 运行 document 测试确认 RED。
3. 在 PDF 展开时保留仅供当前请求使用的提取文本元数据，并实现通用格式形状解析与唯一匹配。
4. 运行 document 测试确认 GREEN。

## 任务 5：接入非流式与流式本地确定性响应

1. 为 handler 添加非流式和流式响应测试，验证 content、stop_reason、SSE 顺序及 usage 总量。
2. 运行定向测试确认 RED。
3. 在文档展开后、调用上游前执行严格 short-circuit；非流式返回标准 message，流式返回完整标准 SSE 事件序列。
4. 确保 cache 字段不被伪造，输入计量仍基于原始请求展开后的真实文本。
5. 运行 handler/stream 定向测试确认 GREEN。

## 任务 6：增加 D7 工具事件诊断

1. 增加仅验证日志辅助数据构造、不暴露参数值的单元测试或纯函数测试。
2. 在 `process_tool_use`、`emit_completed_tool_use` 和终态处记录 id/name/index/input 字节数、已发出工具名、stop reason 与 terminal error。
3. 保持现有工具协议、required 护栏和打捞条件不变。

## 任务 7：回归、探针与本地合并

1. 运行 `cargo fmt -- --check`、全特性全量测试、`cargo check --all-features -j 1` 和 `git diff --check`。
2. 运行本地 Anthropic 探针，重点检查 tool_choice、PDF、stream、UTF-8 与 usage。
3. 检查 diff 不含凭据、trace、截图和构建产物。
4. 按功能拆分或合并为清晰的中文本地提交，不 push、不部署。
5. 使用分支完成流程将修复合并回本地 master，确认 master 未发生并发变化。
