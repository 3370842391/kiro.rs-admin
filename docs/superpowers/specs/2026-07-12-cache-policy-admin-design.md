# 缓存策略管理端设计

## 1. 目标

为 rs 的 Anthropic prompt cache 计量模拟增加可运行时调整、可持久化、可观测的管理能力。

本设计实现：

- 默认模拟缓存 TTL 从 5 分钟调整为 30 分钟。
- 管理端可选择 5 分钟、30 分钟或 1 小时。
- 客户端显式 TTL 优先于管理端默认值。
- 增加缓存计量开关、无 `cache_control` 自动缓存开关、容量、落盘周期、命中率上下限、实时状态和清空缓存。
- 所有设置运行时生效并持久化到 `config.json`。
- 明确区分“rs 计量模拟”与“Kiro 上游真实缓存”，避免把模拟命中误认为真实成本下降。

## 2. 当前行为与问题

当前 `CacheMeter`：

- 默认 TTL 固定为 300 秒。
- 只识别 `5m` 和 `1h`。
- `30m` 或其他值会静默回退到 5 分钟。
- 最大 TTL 固定为 3600 秒。
- 最大条目数固定为 4096。
- 每 60 秒清理并落盘。
- 即使请求没有 `cache_control`，只要存在可复用历史前缀，也会自动模拟缓存。
- 命中率整形已有管理端入口，但 TTL、容量、开关和状态不可管理。
- 管理端仍提示“冷启动命中率会被抬高”，与后端已经实施的冷启动护栏不一致。

缓存算法本身只拆分：

```text
input_tokens + cache_creation_input_tokens + cache_read_input_tokens
```

三者总量不变。Kiro 上游没有返回真实 cache token，因此本模块不会减少真实上游 token、延迟或成本。

## 3. 设计原则

1. 客户端明确配置优先：显式 `5m`、`30m`、`1h` 优先于管理端默认 TTL。
2. 管理端默认 TTL 仅在客户端没有显式 TTL 时使用。
3. 最大允许 TTL 固定为 1 小时，不开放任意超长值。
4. 设置修改不打断正在处理的请求。
5. 修改 TTL 不自动清空现有条目；新写入或续期时使用新策略。
6. 降低容量时立即按 LRU 淘汰到新容量。
7. 关闭缓存计量时，API usage 全部计入普通 input，不读取、不写入缓存。
8. 不伪造冷启动命中；原始 `cache_read == 0` 时命中率整形仍不生效。
9. 管理端必须展示费用影响警告。
10. 不提供任意环境变量编辑器，只提供有类型、有范围校验的结构化设置。

## 4. TTL 解析与优先级

### 4.1 支持值

客户端协议扩展支持：

| 字符串 | 秒数 |
|---|---:|
| `5m` | 300 |
| `30m` | 1800 |
| `1h` | 3600 |

管理端默认 TTL 也只能选择这三个值。

### 4.2 解析规则

单个请求的有效 TTL 按以下顺序确定：

1. 扫描 tools、system 和 message content block 中的 `cache_control.ttl`。
2. 如果存在一个或多个受支持的显式 TTL，沿用当前整体策略，取其中最大值。
3. 如果存在 `cache_control` 但未填写 `ttl`，使用管理端默认 TTL。
4. 如果完全没有 `cache_control`：
   - `autoWithoutCacheControl=true`：使用管理端默认 TTL，并沿用当前自动前缀缓存。
   - `autoWithoutCacheControl=false`：不建立缓存段，usage 全部计入 input。
5. 不支持的 TTL 不扩大到更长窗口；回退管理端默认 TTL并记录不包含原始请求正文的 DEBUG 分类日志。
6. 所有 TTL 最终限制在 60–3600 秒。

显式 5 分钟必须能够覆盖管理端默认 30 分钟，不能继续使用当前 `default.max(explicit)` 的写法。只有同一个请求中出现多个显式 TTL 时才取最大显式值。

## 5. 配置模型

在 `Config` 中增加：

```rust
#[serde(default = "default_cache_metering_enabled")]
pub cache_metering_enabled: bool,

#[serde(default = "default_cache_default_ttl_secs")]
pub cache_default_ttl_secs: u64,

#[serde(default = "default_cache_auto_without_control")]
pub cache_auto_without_control: bool,

#[serde(default = "default_cache_capacity")]
pub cache_capacity: usize,

#[serde(default = "default_cache_flush_interval_secs")]
pub cache_flush_interval_secs: u64,
```

默认值：

```text
cacheMeteringEnabled=true
cacheDefaultTtlSecs=1800
cacheAutoWithoutControl=true
cacheCapacity=4096
cacheFlushIntervalSecs=60
cacheHitRateMinPct=0
cacheHitRateMaxPct=0
```

生产环境当前 `cacheHitRateMaxPct=95` 应在升级时原样保留，不被新默认覆盖。

校验范围：

| 字段 | 允许值 |
|---|---|
| `cacheDefaultTtlSecs` | 300、1800、3600 |
| `cacheCapacity` | 256–65536 |
| `cacheFlushIntervalSecs` | 10–600 |
| `cacheHitRateMinPct` | 0–100 |
| `cacheHitRateMaxPct` | 0–100，非零时不得小于非零 min |

## 6. 运行时架构

### 6.1 CachePolicy

新增纯配置结构：

```rust
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachePolicy {
    pub enabled: bool,
    pub default_ttl_secs: u64,
    pub auto_without_cache_control: bool,
    pub capacity: usize,
    pub flush_interval_secs: u64,
}
```

`CacheMeter` 持有当前策略。读取路径应低成本，不在每次请求重新读取磁盘。可以使用 `RwLock<CachePolicy>`；缓存计算本身已经会获取 `inner` mutex，策略只复制一个小结构。

### 6.2 更新策略

```rust
pub fn policy(&self) -> CachePolicy;
pub fn update_policy(&self, policy: CachePolicy) -> Result<CachePolicy>;
pub fn clear(&self) -> usize;
pub fn stats(&self) -> CacheStats;
```

更新规则：

- AdminService 校验通过后先把新策略持久化到 `config.json`，成功后再更新运行时策略。
- 持久化失败时运行时策略和缓存条目均保持不变。
- 容量降低后立即执行 LRU 淘汰。
- TTL 变化不重写全部条目的 `expires_at`。
- 当前条目下一次被命中并 record 时，按新 TTL 续期。
- `enabled=false` 时保留尚未过期的内存记录，但不参与 usage；管理员可单独清空。

### 6.3 动态落盘周期

后台任务不能继续把 60 秒写死。使用可更新的策略通知：

- 每次循环读取当前 `flush_interval_secs`。
- `tokio::select!` 等待 interval 到期或策略变更通知。
- 策略改变时重新计算下一次周期。
- 服务退出前沿用现有 flush 行为。

## 7. 缓存状态

新增：

```rust
#[derive(Debug, Clone, Serialize)]
pub struct CacheStats {
    pub active_entries: usize,
    pub capacity: usize,
    pub usage_pct: f64,
    pub dirty: bool,
    pub last_flush_at: Option<String>,
    pub persist_enabled: bool,
}
```

状态不返回前缀 hash、请求文本、session id 或任何缓存内容。

`clear()`：

- 清空内存条目。
- 标记 dirty。
- 立即覆盖持久化文件为 `{}`。
- 返回清除的条目数。
- 不清理 usage 历史统计。
- 如果同步写盘失败，保持 dirty 状态让后台任务继续重试，避免进程重启后重新加载旧条目。

## 8. Admin API

### 8.1 读取与更新

新增或替换现有 cache-hit-rate 接口为统一缓存策略接口：

```text
GET /api/admin/config/cache-policy
PUT /api/admin/config/cache-policy
POST /api/admin/config/cache-policy/clear
```

响应：

```json
{
  "enabled": true,
  "defaultTtlSecs": 1800,
  "allowedTtlSecs": [300, 1800, 3600],
  "autoWithoutCacheControl": true,
  "capacity": 4096,
  "flushIntervalSecs": 60,
  "minPct": 0,
  "maxPct": 95,
  "activeEntries": 128,
  "usagePct": 3.13,
  "dirty": false,
  "lastFlushAt": "2026-07-12T13:45:00Z",
  "persistEnabled": true
}
```

更新请求允许部分字段：

```json
{
  "defaultTtlSecs": 1800,
  "capacity": 4096
}
```

### 8.2 兼容旧接口

现有：

```text
GET/PUT /api/admin/config/cache-hit-rate
```

先保留，内部复用统一策略更新逻辑，避免旧管理端或脚本立即失效。新 UI 只调用 cache-policy。

## 9. 管理端界面

把现有“缓存命中率整形”弹窗升级为“缓存策略”。

### 9.1 基础设置

- 缓存计量：开关。
- 默认 TTL：下拉选择 5 分钟、30 分钟、1 小时；默认选中 30 分钟。
- 无 `cache_control` 自动缓存：开关，默认开启并标注“兼容当前行为”。
- 最大缓存条目：数字输入。
- 清理/落盘周期：数字输入，单位秒。

### 9.2 命中率整形

保留最低/最高命中率输入，但修正文案：

- 冷启动或真实 `cache_read=0` 不会被抬高。
- 只有已有真实模拟命中的请求才会进行区间整形。
- `(0,0)` 关闭整形。
- 生产当前 `(0,95)` 代表只压过高命中率。

删除“常用区间 90–99”的默认推荐，避免诱导运营开启不真实的高下界。

### 9.3 状态与操作

显示：

- 有效条目 / 最大容量。
- 容量占比。
- 最近落盘时间。
- 是否有未落盘变化。
- 持久化是否可用。

提供“清空缓存”按钮，必须二次确认：

```text
清空后，后续请求会重新产生 cache_creation；不会删除用量历史。确定继续？
```

### 9.4 费用提示

界面固定展示：

> 这里控制的是 rs 对 Anthropic cache token 的模拟计量，不代表 Kiro 上游实际缓存。延长 TTL 会让更多 token 显示为较便宜的 cache_read，可能降低客户账单，但不会同步降低上游成本。

## 10. 数据流

```text
Admin UI
  -> PUT /config/cache-policy
  -> AdminService 校验
  -> CacheMeter.update_policy + TokenManager hit-rate update
  -> config.json 持久化
  -> 返回运行时策略和状态

Anthropic request
  -> 读取 CachePolicy
  -> enabled / autoWithoutCacheControl 判断
  -> 解析显式 TTL 或使用默认 30m
  -> 最长公共前缀 lookup / record
  -> CacheUsage
  -> hit-rate bounds
  -> input / creation / read 互斥拆分
```

## 11. 错误处理

- 设置值越界：HTTP 400，返回明确字段名和范围。
- 默认 TTL 不是 300/1800/3600：HTTP 400。
- config 持久化失败：不修改运行时策略，HTTP 500。
- 清空落盘失败：内存清空仍生效、dirty 保持 true 供后台重试，同时返回 500 并记录错误；界面提示重试。
- 缓存文件损坏：沿用当前行为，从空缓存启动并记录 warning。
- 不支持的客户端 TTL：回退管理端默认 TTL，DEBUG 记录分类，不记录 prompt 或敏感内容。

## 12. 迁移与兼容

- 老配置缺少新字段时，默认 TTL 变为 1800 秒，这是本需求授权的行为变化。
- `cacheAutoWithoutControl=true` 保留当前“无 cache_control 也自动模拟”的行为。
- 旧 `cacheHitRateMinPct/MaxPct` 原样加载。
- 已落盘缓存条目结构无需迁移；旧条目按原 `expires_at` 继续存活。
- 旧 cache-hit-rate API 暂时保留。
- 不修改 Anthropic usage 字段结构。

## 13. 测试设计

### 13.1 CacheMeter 单元测试

- `parse_ttl("30m") == 1800`。
- 无显式 TTL 使用管理端默认 1800。
- 显式 `5m` 覆盖默认 1800。
- 显式 `1h` 覆盖默认 1800。
- 多个显式 TTL 取最大值。
- `autoWithoutCacheControl=false` 时无 cache_control 不建立段。
- `enabled=false` 时全 input、零 creation/read、无写入。
- TTL 更新不立即修改已有 entry 的 expiry。
- 下次 record 使用新 TTL 续期。
- 容量从 4096 降低时立即 LRU 淘汰。
- clear 清空内存并写出 `{}`。
- 冷启动在任何命中率配置下 read 仍为 0。

### 13.2 Admin 后端测试

- GET 返回策略、allowed TTL 和状态。
- PUT 部分更新运行时生效并持久化。
- 非法 TTL、容量、周期、命中率返回 400。
- 持久化失败时运行时策略保持不变。
- clear 返回清除数量。
- 旧 cache-hit-rate API 仍可读写。

### 13.3 管理端验证

- TypeScript 构建通过。
- 打开弹窗能加载所有字段。
- 保存后 query cache 刷新。
- TTL 下拉只有三个选项。
- 非法数字在前端拦截。
- 清空缓存需要确认。
- 冷启动说明与后端一致。

### 13.4 集成验收

- 默认 30 分钟配置写入并重启后仍存在。
- 修改为 5 分钟、1 小时均无需重启立即生效。
- 客户端显式 5m 不被默认 30m 覆盖。
- direct 和 NewAPI 的 usage 三项总量保持一致。
- 冷请求 `cacheRead=0`。
- 同 session warm 请求出现 cache read。
- 禁用后所有请求 cache creation/read 为 0。
- 容器 restart count=0。

## 14. 客户和商业影响

默认 TTL 从 5 分钟延长到 30 分钟后：

- 5–30 分钟内恢复同一稳定前缀的请求更容易显示 cache read。
- 客户账单可能下降。
- cache creation 次数可能下降。
- Kiro 上游真实费用不会因此自动下降。
- 运营毛利可能下降，需要通过管理端用量统计观察。
- 内存增长受 capacity 限制，默认容量不变时风险较低。

管理端必须把这一影响放在 TTL 设置附近，避免把“更高命中率”等同于“上游更省钱”。

## 15. 非目标

- 不实现 Kiro 上游真实 prompt cache。
- 不伪造上游返回的 cache token。
- 不允许超过 1 小时 TTL。
- 不提供任意环境变量/JSON 编辑器。
- 不修改客户历史用量记录。
- 不为检测站硬编码请求特征。
