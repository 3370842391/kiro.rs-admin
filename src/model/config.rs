use anyhow::Context;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TlsBackend {
    Rustls,
    NativeTls,
}

impl Default for TlsBackend {
    fn default() -> Self {
        Self::Rustls
    }
}

/// 工具兼容模式。
///
/// - `ClaudeCode`（默认）：把 Claude Code 内置工具（Write/Edit/Bash/Read/Glob/Grep/LS/WebSearch）
///   的工具名与入参双向适配为 Kiro 内置工具（fs_write/str_replace/... ），并替换为 Kiro 内置 schema。
/// - `Raw`：保留旧行为，直接透传客户端工具名/schema，用于排障。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ToolCompatibilityMode {
    #[default]
    ClaudeCode,
    Raw,
}

/// 普通 429 的重试策略模式。
///
/// `Failover` 是本项目默认策略：普通 429 先用同一凭据切换 q/runtime 独立限流桶，
/// 备用端点仍失败时再在本次请求内换凭据，且不给凭据施加跨请求冷却。其它模式来自
/// Kiro-RS-Tool，用于按需切换为更激进或更保守的普通 429 冷却与重试节奏。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub enum RetryMode {
    #[default]
    Failover,
    Turbo,
    Fast,
    Balanced,
    Steady,
    Polite,
    Custom,
}

impl std::fmt::Display for RetryMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Failover => "failover",
            Self::Turbo => "turbo",
            Self::Fast => "fast",
            Self::Balanced => "balanced",
            Self::Steady => "steady",
            Self::Polite => "polite",
            Self::Custom => "custom",
        };
        f.write_str(value)
    }
}

impl std::str::FromStr for RetryMode {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "failover" | "current" | "default" => Ok(Self::Failover),
            "turbo" => Ok(Self::Turbo),
            "fast" => Ok(Self::Fast),
            "balanced" => Ok(Self::Balanced),
            "steady" => Ok(Self::Steady),
            "polite" => Ok(Self::Polite),
            "custom" => Ok(Self::Custom),
            _ => anyhow::bail!("无效的重试模式: {}", value),
        }
    }
}

/// 普通 429 的可配置重试策略。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RetryPolicy {
    /// 普通 429 后的跨请求冷却时间；0 表示不进入跨请求冷却。
    pub rate_limit_cooldown_ms: u64,
    /// 每个凭据的请求重试预算。非默认策略会按账号数放大，并受全局上限保护。
    pub max_request_retries: usize,
    /// 指数退避基础时长。
    pub base_backoff_ms: u64,
    /// 指数退避最大时长。
    pub max_backoff_ms: u64,
    /// 普通 429 后是否优先切换其它凭据。
    pub credential_switch_on_429: bool,
    /// 是否尊重上游 Retry-After 头。
    pub respect_retry_after: bool,
}

impl RetryPolicy {
    pub fn preset(mode: RetryMode) -> Self {
        match mode {
            RetryMode::Failover => Self {
                rate_limit_cooldown_ms: 0,
                max_request_retries: 3,
                base_backoff_ms: 1_000,
                max_backoff_ms: 8_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Turbo => Self {
                rate_limit_cooldown_ms: 1_000,
                max_request_retries: 12,
                base_backoff_ms: 100,
                max_backoff_ms: 1_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Fast => Self {
                rate_limit_cooldown_ms: 3_000,
                max_request_retries: 9,
                base_backoff_ms: 200,
                max_backoff_ms: 2_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Balanced => Self {
                rate_limit_cooldown_ms: 10_000,
                max_request_retries: 9,
                base_backoff_ms: 500,
                max_backoff_ms: 5_000,
                credential_switch_on_429: true,
                respect_retry_after: false,
            },
            RetryMode::Steady => Self {
                rate_limit_cooldown_ms: 30_000,
                max_request_retries: 6,
                base_backoff_ms: 1_000,
                max_backoff_ms: 10_000,
                credential_switch_on_429: true,
                respect_retry_after: true,
            },
            RetryMode::Polite => Self {
                rate_limit_cooldown_ms: 60_000,
                max_request_retries: 4,
                base_backoff_ms: 2_000,
                max_backoff_ms: 30_000,
                credential_switch_on_429: false,
                respect_retry_after: true,
            },
            RetryMode::Custom => Self::preset(RetryMode::Fast),
        }
    }

    pub fn effective(mode: RetryMode, custom: Option<&RetryPolicy>) -> anyhow::Result<Self> {
        let policy = if mode == RetryMode::Custom {
            custom
                .cloned()
                .unwrap_or_else(|| Self::preset(RetryMode::Fast))
        } else {
            Self::preset(mode)
        };

        policy.validate()?;
        Ok(policy)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.rate_limit_cooldown_ms > 120_000 {
            anyhow::bail!("rateLimitCooldownMs 必须在 0..=120000 之间");
        }
        if !(1..=30).contains(&self.max_request_retries) {
            anyhow::bail!("maxRequestRetries 必须在 1..=30 之间");
        }
        if !(50..=30_000).contains(&self.base_backoff_ms) {
            anyhow::bail!("baseBackoffMs 必须在 50..=30000 之间");
        }
        if self.max_backoff_ms < self.base_backoff_ms || self.max_backoff_ms > 120_000 {
            anyhow::bail!("maxBackoffMs 必须在 baseBackoffMs..=120000 之间");
        }
        Ok(())
    }
}

/// KNA 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_host")]
    pub host: String,

    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default = "default_region")]
    pub region: String,

    /// Auth Region（用于 Token 刷新），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_region: Option<String>,

    /// API Region（用于 API 请求），未配置时回退到 region
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_region: Option<String>,

    #[serde(default = "default_kiro_version")]
    pub kiro_version: String,

    #[serde(default)]
    pub machine_id: Option<String>,

    #[serde(default)]
    pub api_key: Option<String>,

    #[serde(default = "default_system_version")]
    pub system_version: String,

    #[serde(default = "default_node_version")]
    pub node_version: String,

    #[serde(default = "default_tls_backend")]
    pub tls_backend: TlsBackend,

    /// 外部 count_tokens API 地址（可选）
    #[serde(default)]
    pub count_tokens_api_url: Option<String>,

    /// count_tokens API 密钥（可选）
    #[serde(default)]
    pub count_tokens_api_key: Option<String>,

    /// count_tokens API 认证类型（可选，"x-api-key" 或 "bearer"，默认 "x-api-key"）
    #[serde(default = "default_count_tokens_auth_type")]
    pub count_tokens_auth_type: String,

    /// HTTP 代理地址（可选）
    /// 支持格式: http://host:port, https://host:port, socks5://host:port
    #[serde(default)]
    pub proxy_url: Option<String>,

    /// 代理认证用户名（可选）
    #[serde(default)]
    pub proxy_username: Option<String>,

    /// 代理认证密码（可选）
    #[serde(default)]
    pub proxy_password: Option<String>,

    /// Admin API 密钥（可选，启用 Admin API 功能）
    #[serde(default)]
    pub admin_api_key: Option<String>,

    /// 上一次成功更新前正在运行的版本号，用于在前端展示「回退到 vX.Y.Z」按钮。
    /// 实际回退动作通过 `<exe>.backup` 文件完成，无需访问网络。
    #[serde(default)]
    pub update_previous_version: Option<String>,

    /// GitHub Personal Access Token（可选）。设置后 GitHub Releases 接口会带上
    /// `Authorization: Bearer <token>`，把限流从匿名 60/h 提到认证 5000/h。
    /// 仅需 `public_repo` 读取权限即可。
    #[serde(default)]
    pub github_token: Option<String>,

    /// 上一次成功完成在线更新的时间（RFC3339）。前端用于显示「上次更新于 …」。
    #[serde(default)]
    pub update_last_applied_at: Option<String>,

    /// 是否启用无人值守自动更新。开启后服务会在每天的 `update_auto_apply_time`
    /// 时刻检查 GitHub Releases，发现新版本即自动下载二进制并替换重启。
    #[serde(default)]
    pub update_auto_apply: bool,

    /// 自动更新的每日触发时间（本地时区，`HH:MM` 24 小时制）。
    /// 默认 03:00 凌晨执行，对在线服务影响最小。
    #[serde(default = "default_update_auto_apply_time")]
    pub update_auto_apply_time: String,

    /// 负载均衡模式（"priority" / "balanced" / "least_conn"，默认 "least_conn" 最少负载）
    #[serde(default = "default_load_balancing_mode")]
    pub load_balancing_mode: String,

    /// 代理均衡模式（"sticky" / "round_robin" / "least_load"）
    #[serde(default = "default_proxy_balancing_mode")]
    pub proxy_balancing_mode: String,

    /// 账号级 429 风控触发时是否对当前凭据进入冷却并故障转移（默认 true）。
    ///
    /// 关闭后：429 + suspicious activity 仍按普通瞬态错误重试，不切换凭据。
    /// 开启后：识别到 suspicious activity 字符串时，把当前凭据冷却 `account_throttle_cooldown_secs` 秒，
    /// 立即切换到下一个可用凭据。
    #[serde(default = "default_account_throttle_failover")]
    pub account_throttle_failover: bool,

    /// 账号级风控冷却时长（秒，默认 1800 = 30 分钟）。
    #[serde(default = "default_account_throttle_cooldown_secs")]
    pub account_throttle_cooldown_secs: u64,

    /// 普通 429 重试策略模式。默认 `failover` 保持当前项目行为。
    #[serde(default = "default_retry_mode")]
    pub retry_mode: RetryMode,

    /// `retry_mode = custom` 时使用的普通 429 自定义策略。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RetryPolicy>,

    /// 是否开启非流式响应的 thinking 块提取（默认 true）
    ///
    /// 启用后，非流式响应中的 `<thinking>...</thinking>` 标签会被解析为
    /// 独立的 `{"type": "thinking", ...}` 内容块,与流式响应行为一致。
    #[serde(default = "default_extract_thinking")]
    pub extract_thinking: bool,

    /// 客户端请求 thinking 但 Kiro 未返回 reasoning 时，是否按严格协议返回错误。
    /// 默认 false：保留上游已经产生的正文或工具调用，不伪造 thinking。
    #[serde(default)]
    pub strict_thinking_validation: bool,

    /// 是否把无 system/tools/thinking/历史/多模态的单轮 `ping` 作为本地健康检查，
    /// 直接返回 `pong`。默认开启，可关闭以恢复完全上游行为。
    #[serde(default = "default_true")]
    pub local_ping_response: bool,

    /// 是否启用严格模型资料探针的本地确定性回复。
    #[serde(default = "default_true")]
    pub model_profile_exact_answers_enabled: bool,

    /// 工具兼容模式。默认 `claude-code`：把 Claude Code 内置工具名/入参双向适配为
    /// Kiro 内置工具；`raw` 保留旧行为、直接透传客户端工具 schema，用于排障。
    #[serde(default = "default_tool_compatibility_mode")]
    pub tool_compatibility_mode: ToolCompatibilityMode,

    /// 默认端点名称（凭据未显式指定 endpoint 时使用，默认 "ide"）
    #[serde(default = "default_endpoint")]
    pub default_endpoint: String,

    /// 是否启用请求链路追踪（写 traces.db）。默认 true。
    ///
    /// 关闭后：不再写入 trace 记录、不走 TraceSink，但 `GET /api/admin/traces`
    /// 仍可查询历史已存记录。适合隐私敏感或磁盘紧张的场景。
    #[serde(default = "default_trace_enabled")]
    pub trace_enabled: bool,

    /// 请求链路追踪记录保留天数（默认 7）。后台任务每天清理超期记录。
    #[serde(default = "default_trace_retention_days")]
    pub trace_retention_days: u32,

    /// 请求用量日志（usage_log.*.jsonl + 聚合桶）保留天数（默认 31）。
    #[serde(default = "default_usage_log_retention_days")]
    pub usage_log_retention_days: u32,

    /// 流式空闲超时（秒，默认 120）。
    ///
    /// 上游返回 200 后，若连续 `stream_idle_timeout_secs` 秒没有收到任何字节
    /// （首字节前挂死或中途停流），中转层主动收尾结束该流，避免空烧到绝对超时。
    /// 复现 Kiro-Go 的 idle watchdog（120s）。`0` = 关闭空闲超时（仅靠绝对超时兜底）。
    /// 运行时可在管理面板调整。
    #[serde(default = "default_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,

    /// 流式请求是否在 Kiro 上游响应前立即提交 SSE，并用注释心跳保活。
    /// false 保留真实上游 HTTP 状态；true 时提交后的上游错误改走 SSE error。
    #[serde(default)]
    pub early_stream_handshake: bool,

    /// 身份归一化：把 Kiro 网关注入的品牌自述（"I'm Kiro" / "made by AWS"）改写回
    /// Claude / Anthropic。默认 true（开启）——底层本就是真实 Claude 模型，用户已授权
    /// 还原对外身份一致性以通过验真检测。设 false 保留上游原始品牌措辞。见 anthropic::identity。
    #[serde(default = "default_true")]
    pub identity_normalization: bool,

    /// 429 降级桶链运行时覆盖。键 = 主端点名（`ide` / `cli`），值 = 该主端点 429 时
    /// 依次尝试的备用桶名（有序）。缺省（None）时回退各 endpoint 的静态 `fallback_chain()`，
    /// 保证老部署零行为变化。空数组 = 该主端点不降级。运行时可在管理面板编辑。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint_chains: Option<HashMap<String, Vec<String>>>,

    /// 单个客户端请求内「备用桶尝试」的总次数硬上限（跨 attempt 累计）。
    /// 防止「链长 × attempt 数」把单请求放大成上百次上游调用。默认 6；`0` = 不限。
    /// 运行时可在管理面板调整。
    #[serde(default = "default_max_bucket_attempts_per_request")]
    pub max_bucket_attempts_per_request: usize,

    /// 是否启用 rs 的模拟 prompt cache 计量。
    #[serde(default = "default_cache_metering_enabled")]
    pub cache_metering_enabled: bool,

    /// 未显式提供 cache_control TTL 时使用的默认缓存窗口（秒）。
    #[serde(default = "default_cache_default_ttl_secs")]
    pub cache_default_ttl_secs: u64,

    /// 请求没有 cache_control 时，是否自动对稳定前缀进行模拟缓存。
    #[serde(default = "default_cache_auto_without_control")]
    pub cache_auto_without_control: bool,

    /// 模拟缓存最多保留的前缀条目数。
    #[serde(default = "default_cache_capacity")]
    pub cache_capacity: usize,

    /// 过期清理和缓存状态落盘周期（秒）。
    #[serde(default = "default_cache_flush_interval_secs")]
    pub cache_flush_interval_secs: u64,

    /// 缓存命中率整形——下界（百分比 0..=100）。
    ///
    /// 上游不下发真实缓存 token，中转层自行模拟；本旋钮把最终呈现（newapi 计费用量）的
    /// 命中率 `cache_read/(input+cache_read)` **钳制**进 `[min, max]` 区间：低于 min 提到
    /// min，高于 max 压到 max，区间内保留真实模拟值。整形只在 `input↔cache_read` 之间挪，
    /// **保持 `input+creation+read` 总量不变**（计费总额不漂），creation 不动。
    /// `min == 0 && max == 0` = 关闭整形（默认，零行为变化）。运行时可在管理面板调整。
    #[serde(default)]
    pub cache_hit_rate_min_pct: u32,

    /// 缓存命中率整形——上界（百分比 0..=100）。见 [`Self::cache_hit_rate_min_pct`]。
    /// `min == 0 && max == 0` = 关闭。仅 max>0 时也生效（下界视为 0）。
    #[serde(default)]
    pub cache_hit_rate_max_pct: u32,

    /// 是否启用 Kiro 出站图片总预算治理。只压缩历史图片，不修改当前轮图片。
    #[serde(default = "default_true")]
    pub image_budget_enabled: bool,

    /// 所有历史与当前轮图片的 base64 总预算字节数。
    #[serde(default = "default_image_total_budget")]
    pub image_total_base64_budget_bytes: usize,

    /// 普通预检压缩历史图片时的最大边长。
    #[serde(default = "default_image_history_dimension")]
    pub image_history_max_dimension: u32,

    /// 普通预检压缩历史图片时的 JPEG 质量。
    #[serde(default = "default_image_history_quality")]
    pub image_history_jpeg_quality: u8,

    /// 上游请求体长度拒绝后，一次降级重试使用的历史图片最大边长。
    #[serde(default = "default_image_retry_dimension")]
    pub image_retry_history_max_dimension: u32,

    /// 上游请求体长度拒绝后，一次降级重试使用的历史图片 JPEG 质量。
    #[serde(default = "default_image_retry_quality")]
    pub image_retry_history_jpeg_quality: u8,

    /// 端点特定的配置
    ///
    /// 键为端点名（如 "ide" / "cli"），值为该端点自由定义的参数对象。
    /// 未在此表出现的端点沿用实现内置默认值。
    #[serde(default)]
    pub endpoints: HashMap<String, serde_json::Value>,

    /// 配置文件路径（运行时元数据，不写入 JSON）
    #[serde(skip)]
    config_path: Option<PathBuf>,
}

fn default_host() -> String {
    "127.0.0.1".to_string()
}

fn default_port() -> u16 {
    8080
}

fn default_region() -> String {
    "us-east-1".to_string()
}

fn default_kiro_version() -> String {
    "2.3.0".to_string()
}

fn default_system_version() -> String {
    "macos".to_string()
}

fn default_node_version() -> String {
    "22.22.0".to_string()
}

fn default_count_tokens_auth_type() -> String {
    "x-api-key".to_string()
}

fn default_tls_backend() -> TlsBackend {
    TlsBackend::Rustls
}

fn default_load_balancing_mode() -> String {
    // 默认最少负载：把请求优先分给当前在途请求数最少的凭据，避免高优先级凭据被打爆。
    "least_conn".to_string()
}

fn default_proxy_balancing_mode() -> String {
    "sticky".to_string()
}

fn default_account_throttle_failover() -> bool {
    true
}

fn default_account_throttle_cooldown_secs() -> u64 {
    30 * 60
}

fn default_retry_mode() -> RetryMode {
    RetryMode::Failover
}

fn default_update_auto_apply_time() -> String {
    "03:00".to_string()
}

fn default_extract_thinking() -> bool {
    true
}

fn default_true() -> bool {
    true
}

fn default_tool_compatibility_mode() -> ToolCompatibilityMode {
    ToolCompatibilityMode::ClaudeCode
}

fn default_endpoint() -> String {
    crate::kiro::endpoint::ide::IDE_ENDPOINT_NAME.to_string()
}

fn default_trace_enabled() -> bool {
    true
}

fn default_trace_retention_days() -> u32 {
    7
}

fn default_stream_idle_timeout_secs() -> u64 {
    120
}

fn default_max_bucket_attempts_per_request() -> usize {
    6
}

fn default_cache_metering_enabled() -> bool {
    true
}

fn default_cache_default_ttl_secs() -> u64 {
    30 * 60
}

fn default_cache_auto_without_control() -> bool {
    true
}

fn default_cache_capacity() -> usize {
    4096
}

fn default_cache_flush_interval_secs() -> u64 {
    60
}

fn default_image_total_budget() -> usize {
    819_200
}

fn default_image_history_dimension() -> u32 {
    1_280
}

fn default_image_history_quality() -> u8 {
    72
}

fn default_image_retry_dimension() -> u32 {
    960
}

fn default_image_retry_quality() -> u8 {
    60
}

fn default_usage_log_retention_days() -> u32 {
    31
}

impl Default for Config {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            region: default_region(),
            auth_region: None,
            api_region: None,
            kiro_version: default_kiro_version(),
            machine_id: None,
            api_key: None,
            system_version: default_system_version(),
            node_version: default_node_version(),
            tls_backend: default_tls_backend(),
            count_tokens_api_url: None,
            count_tokens_api_key: None,
            count_tokens_auth_type: default_count_tokens_auth_type(),
            proxy_url: None,
            proxy_username: None,
            proxy_password: None,
            admin_api_key: None,
            update_previous_version: None,
            github_token: None,
            update_last_applied_at: None,
            update_auto_apply: false,
            update_auto_apply_time: default_update_auto_apply_time(),
            load_balancing_mode: default_load_balancing_mode(),
            proxy_balancing_mode: default_proxy_balancing_mode(),
            account_throttle_failover: default_account_throttle_failover(),
            account_throttle_cooldown_secs: default_account_throttle_cooldown_secs(),
            retry_mode: default_retry_mode(),
            retry_policy: None,
            extract_thinking: default_extract_thinking(),
            strict_thinking_validation: false,
            local_ping_response: default_true(),
            model_profile_exact_answers_enabled: default_true(),
            tool_compatibility_mode: default_tool_compatibility_mode(),
            default_endpoint: default_endpoint(),
            trace_enabled: default_trace_enabled(),
            trace_retention_days: default_trace_retention_days(),
            usage_log_retention_days: default_usage_log_retention_days(),
            stream_idle_timeout_secs: default_stream_idle_timeout_secs(),
            early_stream_handshake: false,
            identity_normalization: true,
            endpoint_chains: None,
            max_bucket_attempts_per_request: default_max_bucket_attempts_per_request(),
            cache_metering_enabled: default_cache_metering_enabled(),
            cache_default_ttl_secs: default_cache_default_ttl_secs(),
            cache_auto_without_control: default_cache_auto_without_control(),
            cache_capacity: default_cache_capacity(),
            cache_flush_interval_secs: default_cache_flush_interval_secs(),
            cache_hit_rate_min_pct: 0,
            cache_hit_rate_max_pct: 0,
            image_budget_enabled: true,
            image_total_base64_budget_bytes: default_image_total_budget(),
            image_history_max_dimension: default_image_history_dimension(),
            image_history_jpeg_quality: default_image_history_quality(),
            image_retry_history_max_dimension: default_image_retry_dimension(),
            image_retry_history_jpeg_quality: default_image_retry_quality(),
            endpoints: HashMap::new(),
            config_path: None,
        }
    }
}

impl Config {
    /// 获取默认配置文件路径
    pub fn default_config_path() -> &'static str {
        "config.json"
    }

    /// 获取有效的 Auth Region（用于 Token 刷新）
    /// 优先使用 auth_region，未配置时回退到 region
    pub fn effective_auth_region(&self) -> &str {
        self.auth_region.as_deref().unwrap_or(&self.region)
    }

    /// 获取有效的 API Region（用于 API 请求）
    /// 优先使用 api_region，未配置时回退到 region
    pub fn effective_api_region(&self) -> &str {
        self.api_region.as_deref().unwrap_or(&self.region)
    }

    /// 从文件加载配置
    pub fn load<P: AsRef<Path>>(path: P) -> anyhow::Result<Self> {
        let path = path.as_ref();
        if !path.exists() {
            // 配置文件不存在，返回默认配置
            let mut config = Self::default();
            config.config_path = Some(path.to_path_buf());
            return Ok(config);
        }

        let content = fs::read_to_string(path)?;
        let mut config: Config = serde_json::from_str(&content)?;
        config.config_path = Some(path.to_path_buf());

        // 用户手工把字符串字段清空（如 `"updateAutoApplyTime": ""`）时，serde 默认值不会
        // 介入；这里把"看起来像空"的关键字段回退到默认值，避免后续业务用到
        // 空字符串导致难以诊断的错误。
        if config.update_auto_apply_time.trim().is_empty() {
            config.update_auto_apply_time = default_update_auto_apply_time();
        }

        Ok(config)
    }

    /// 获取配置文件路径（如果有）
    pub fn config_path(&self) -> Option<&Path> {
        self.config_path.as_deref()
    }

    /// 将当前配置写回原始配置文件
    pub fn save(&self) -> anyhow::Result<()> {
        let path = self
            .config_path
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("配置文件路径未知，无法保存配置"))?;

        let content = serde_json::to_string_pretty(self).context("序列化配置失败")?;
        fs::write(path, content)
            .with_context(|| format!("写入配置文件失败: {}", path.display()))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ping_response_defaults_on_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.local_ping_response);

        let disabled: Config = serde_json::from_value(serde_json::json!({
            "localPingResponse": false
        }))
        .unwrap();
        assert!(!disabled.local_ping_response);
        let encoded = serde_json::to_value(disabled).unwrap();
        assert_eq!(encoded["localPingResponse"], false);
    }

    #[test]
    fn model_profile_exact_answers_default_on_and_round_trip_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.model_profile_exact_answers_enabled);

        let disabled: Config = serde_json::from_value(serde_json::json!({
            "modelProfileExactAnswersEnabled": false
        }))
        .unwrap();
        assert!(!disabled.model_profile_exact_answers_enabled);
        let encoded = serde_json::to_value(disabled).unwrap();
        assert_eq!(encoded["modelProfileExactAnswersEnabled"], false);
    }

    #[test]
    fn cache_policy_defaults_to_thirty_minutes() {
        let config: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(config.cache_metering_enabled);
        assert_eq!(config.cache_default_ttl_secs, 1800);
        assert!(config.cache_auto_without_control);
        assert_eq!(config.cache_capacity, 4096);
        assert_eq!(config.cache_flush_interval_secs, 60);
    }

    #[test]
    fn cache_policy_fields_round_trip_in_camel_case() {
        let value = serde_json::json!({
            "cacheMeteringEnabled": false,
            "cacheDefaultTtlSecs": 300,
            "cacheAutoWithoutControl": false,
            "cacheCapacity": 8192,
            "cacheFlushIntervalSecs": 30
        });
        let config: Config = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(config).unwrap();
        assert_eq!(encoded["cacheMeteringEnabled"], false);
        assert_eq!(encoded["cacheDefaultTtlSecs"], 300);
        assert_eq!(encoded["cacheAutoWithoutControl"], false);
        assert_eq!(encoded["cacheCapacity"], 8192);
        assert_eq!(encoded["cacheFlushIntervalSecs"], 30);
    }

    #[test]
    fn image_budget_defaults_and_round_trips_in_camel_case() {
        let defaulted: Config = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(defaulted.image_budget_enabled);
        assert_eq!(defaulted.image_total_base64_budget_bytes, 819_200);
        assert_eq!(defaulted.image_history_max_dimension, 1_280);
        assert_eq!(defaulted.image_history_jpeg_quality, 72);
        assert_eq!(defaulted.image_retry_history_max_dimension, 960);
        assert_eq!(defaulted.image_retry_history_jpeg_quality, 60);

        let encoded = serde_json::to_value(defaulted).unwrap();
        assert_eq!(encoded["imageBudgetEnabled"], true);
        assert_eq!(encoded["imageTotalBase64BudgetBytes"], 819_200);
        assert_eq!(encoded["imageHistoryMaxDimension"], 1_280);
        assert_eq!(encoded["imageHistoryJpegQuality"], 72);
        assert_eq!(encoded["imageRetryHistoryMaxDimension"], 960);
        assert_eq!(encoded["imageRetryHistoryJpegQuality"], 60);
    }

    #[test]
    fn early_stream_handshake_defaults_off() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.early_stream_handshake);
    }

    #[test]
    fn early_stream_handshake_accepts_camel_case_json() {
        let config: Config = serde_json::from_str(r#"{"earlyStreamHandshake":true}"#).unwrap();
        assert!(config.early_stream_handshake);
    }

    #[test]
    fn strict_thinking_validation_defaults_to_false() {
        let config: Config = serde_json::from_str("{}").unwrap();
        assert!(!config.strict_thinking_validation);
    }

    #[test]
    fn strict_thinking_validation_can_be_enabled() {
        let config: Config = serde_json::from_str(r#"{"strictThinkingValidation":true}"#).unwrap();
        assert!(config.strict_thinking_validation);
    }
}
