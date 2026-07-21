# 客户端 Key 级缓存命中率覆盖设计

## 目标

允许管理员在创建或编辑客户端 API Key 时，为单个 Key 指定缓存命中率整形的最小值和最大值；未指定时继续继承全局缓存命中率配置。

## 范围与语义

- 覆盖的是响应 usage 中 `input_tokens` 与 `cache_read_input_tokens` 的命中率整形，不改变请求正文、上游 Kiro 调用、本地缓存写入、TTL、容量或缓存隔离。
- 数值单位为百分比整数 `0..=100`。
- `min=0,max=0` 表示该 Key 关闭命中率整形。
- 仅设置一侧时沿用现有全局语义：`min>0,max=0` 表示只有下限，`min=0,max>0` 表示只有上限。
- 未设置覆盖值时使用运行时全局配置；全局配置变化会立即影响继承全局的 Key。
- 旧版 `client_api_keys.json` 没有该字段时按“继承全局”处理。

## 数据模型

持久化的 `ClientKey` 增加可选对象：

```json
{
  "cacheHitRate": {
    "minPct": 0,
    "maxPct": 90
  }
}
```

`null`/字段缺失表示继承全局。鉴权成功后把该覆盖值冻结到 `KeyContext`，保证单个请求期间编辑 Key 不会改变已开始的请求。

## API

- 创建请求可选 `cacheHitRate: { minPct, maxPct }`。
- 更新请求使用显式 patch 模式：缺少字段表示保持原值，`{ mode: "inherit" }` 清除覆盖，`{ mode: "custom", minPct, maxPct }` 设置覆盖。
- 列表返回当前 Key 的覆盖值；不返回明文 Key。
- 后端统一校验 `0..=100` 和非零上下限的 `min<=max`，失败返回 400 且不写入部分状态。

## 请求链路

```text
client_api_keys.json
        ↓
ClientKeyManager / AuthorizedClientKey
        ↓
auth middleware → KeyContext.cache_hit_rate
        ↓
Anthropic handlers / local exact responses
        ↓
覆盖值存在则使用覆盖值，否则读取 provider 全局值
        ↓
CacheUsage::split_against_total
```

所有流式、非流式、精确本地回复和 OpenAI 兼容转接共用同一解析规则，避免不同入口产生不同 usage。

## 客户影响与兼容性

- 不影响首字、SSE、工具调用、Thinking、图片、模型选择或对话内容。
- 只影响该 Key 返回的 token 拆分，从而可能改变下游按缓存 token 计费的金额。
- 未配置覆盖的现有 Key 行为完全不变。
- 正在处理的请求使用鉴权时快照；修改后从下一次请求开始生效。

## 测试策略

- Rust：Key 序列化迁移、创建/更新/清除覆盖、鉴权快照、非法范围回滚、全局 fallback。
- Rust：CacheUsage 验证覆盖优先于全局，且 0/0 能关闭整形。
- 前端：创建/编辑表单、继承/自定义切换、范围校验、列表展示和 API payload 合约。
- 回归：现有客户端 Key、缓存命中率、Anthropic 流式/非流式测试全部通过。
