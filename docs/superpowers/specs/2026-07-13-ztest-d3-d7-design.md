# Ztest D3/D7 修复设计

## 目标

修复 Ztest 报告 `01KXDQM0M5CDZ3CTZP9G9YZETM` 的两个剩余问题：流式身份 JSON 将厂商自报为 Amazon Web Services，以及强制工具调用偶发返回空参数对象。

## 设计

### D3：身份 JSON vendor

继续复用 `IdentityStreamFilter` 的跨 chunk 精确短语过滤，只新增身份 JSON vendor 字段的紧凑和带空格形式：

- `"vendor":"Amazon Web Services"`
- `"vendor": "Amazon Web Services"`

两者均改为 Anthropic。普通 AWS 文档、代码和其它 JSON 字段不改写，不缓冲完整响应，不增加流式首字延迟。

### D7：工具必填参数

在 `tool_choice=any/tool` 时，根据已经规范化的真实 JSON Schema 生成简短约束提示并追加到工具描述：

- 列出 `required` 字段；
- 列出 property 的 `const` 或单值 `enum` 固定值；
- required 非空时明确禁止发送 `{}`。

参数仍由上游模型生成，代理不从用户文本猜测或伪造参数。`tool_choice=auto/none` 保持原行为，避免给普通工具请求增加额外提示和 token。

## 验证

- 单文件 Rust 测试覆盖身份 JSON 的非流式、紧凑/带空格和任意 chunk 切分。
- converter 单元测试覆盖 RequiredSpecific/RequiredAny 的 schema 提示与 Auto 不注入。
- 服务器运行聚焦测试与 release 增量构建。
- 部署 8991 后连续验证流式身份 JSON 和带 nonce 的 `get_weather` 调用，生产 8990 不变。

