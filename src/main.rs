mod admin;
mod admin_ui;
mod anthropic;
mod common;
mod http_client;
mod image_resize;
mod kiro;
mod model;
mod openai;
pub mod token;
mod wholesale;

use std::collections::HashMap;
use std::sync::Arc;

use axum::serve::ListenerExt;
use clap::Parser;
use kiro::endpoint::{
    AmazonQEndpoint, CliEndpoint, CodeWhispererEndpoint, IdeEndpoint, KiroEndpoint,
    RuntimeCliEndpoint, RuntimeEndpoint,
};
use kiro::model::credentials::{CredentialsConfig, KiroCredentials};
use kiro::provider::KiroProvider;
use kiro::token_manager::MultiTokenManager;
use model::arg::Args;
use model::config::Config;

const QUIET_TRANSPORT_MODULES: [&str; 3] = ["h2", "hyper", "reqwest"];

fn effective_log_filter(configured: Option<&str>) -> String {
    let mut directives = configured
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("info")
        .to_string();
    for module in QUIET_TRANSPORT_MODULES {
        let explicitly_configured = directives.split(',').any(|directive| {
            directive
                .trim()
                .split_once('=')
                .is_some_and(|(target, _)| target.trim() == module)
        });
        if !explicitly_configured {
            directives.push(',');
            directives.push_str(module);
            directives.push_str("=info");
        }
    }
    directives
}

fn log_env_filter() -> tracing_subscriber::EnvFilter {
    let configured = std::env::var("RUST_LOG").ok();
    let directives = effective_log_filter(configured.as_deref());
    tracing_subscriber::EnvFilter::try_new(directives).unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new("info,h2=info,hyper=info,reqwest=info")
    })
}

/// 启动关键路径分段计时。
///
/// 只在 `main` 里用一次，但没有它就只能猜哪一段慢——而线上日志轮转得比一次启动的
/// 回溯需求快得多，事后补不上。
struct BootTimer {
    started: std::time::Instant,
    last: std::time::Instant,
    phases: Vec<(&'static str, u128)>,
}

impl BootTimer {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last: now,
            phases: Vec::new(),
        }
    }

    fn mark(&mut self, phase: &'static str) {
        let now = std::time::Instant::now();
        self.phases
            .push((phase, now.duration_since(self.last).as_millis()));
        self.last = now;
    }

    /// 输出一行「总耗时 + 各段耗时」。放在 `bind` 之后、`serve` 之前，
    /// 这样这行日志的时间点就等于「开始能接客户流量」的时刻。
    fn log_summary(&self) {
        let breakdown = self
            .phases
            .iter()
            .map(|(phase, ms)| format!("{phase}={ms}ms"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            total_ms = self.started.elapsed().as_millis(),
            "启动完成，开始接受连接：{}",
            breakdown
        );
    }
}

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(log_env_filter())
        .init();

    // 绑定端口之前的每一步都挡在客户流量前面，但线上日志轮转很快（实测 9 小时 124 MB），
    // 事后回溯不到启动那一段。所以把分段耗时打进启动横幅，别让优化只能靠猜。
    let mut boot = BootTimer::new();

    // 解析配置/凭证路径
    let config_path = args
        .config
        .unwrap_or_else(|| Config::default_config_path().to_string());
    let credentials_path = args
        .credentials
        .unwrap_or_else(|| KiroCredentials::default_credentials_path().to_string());

    // 文件不存在时自动初始化（Docker 首次部署友好）
    ensure_config_files(&config_path, &credentials_path);

    // 加载配置
    let mut config = Config::load(&config_path).unwrap_or_else(|e| {
        tracing::error!("加载配置失败: {}", e);
        std::process::exit(1);
    });

    // 加载凭证（支持单对象或数组格式）
    let credentials_config = CredentialsConfig::load(&credentials_path).unwrap_or_else(|e| {
        tracing::error!("加载凭证失败: {}", e);
        std::process::exit(1);
    });

    // 判断是否为多凭据格式（用于刷新后回写）
    let is_multiple_format = credentials_config.is_multiple();

    // 转换为按优先级排序的凭据列表
    let mut credentials_list = credentials_config.into_sorted_credentials();

    // 检查 KIRO_API_KEY 环境变量，自动创建 API Key 凭据
    if let Ok(kiro_api_key) = std::env::var("KIRO_API_KEY") {
        if kiro_api_key.is_empty() {
            tracing::warn!("KIRO_API_KEY 环境变量已设置但为空，视为未配置");
        } else {
            tracing::info!("检测到 KIRO_API_KEY 环境变量，添加 API Key 凭据（最高优先级）");
            let api_key_cred = KiroCredentials {
                kiro_api_key: Some(kiro_api_key),
                auth_method: Some("api_key".to_string()),
                priority: 0,
                ..Default::default()
            };
            credentials_list.insert(0, api_key_cred);
        }
    }

    tracing::info!("已加载 {} 个凭据配置", credentials_list.len());

    // 仅显示安全的元数据，避免在日志里泄露 token / client_secret
    let first_credentials = credentials_list.first().cloned().unwrap_or_default();
    tracing::debug!(
        id = ?first_credentials.id,
        email = ?first_credentials.email,
        auth_method = ?first_credentials.auth_method,
        priority = first_credentials.priority,
        endpoint = ?first_credentials.endpoint,
        "已选定主凭证"
    );

    // apiKey 仅用于首次启动时 bootstrap 第一条客户端 Key；
    // 后续 /v1 认证全部走客户端 Key 系统。adminApiKey 仍是管理面板登录密钥。
    let bootstrap_key = config.api_key.clone().filter(|k| !k.trim().is_empty());

    // 构建代理配置
    let proxy_config = config.proxy_url.as_ref().map(|url| {
        let mut proxy = http_client::ProxyConfig::new(url);
        if let (Some(username), Some(password)) = (&config.proxy_username, &config.proxy_password) {
            proxy = proxy.with_auth(username, password);
        }
        proxy
    });

    if proxy_config.is_some() {
        tracing::info!("已配置 HTTP 代理: {}", config.proxy_url.as_ref().unwrap());
    }

    // 启动 Kiro IDE 版本自动获取：从官方元数据端点拉取 currentRelease，
    // 用于流式端点 User-Agent（替代写死的版本号）；失败时回退 config.kiroVersion。
    kiro::kiro_version::spawn_refresher(
        proxy_config.clone(),
        config.tls_backend,
        std::time::Duration::from_secs(12 * 3600),
    );

    // 构建端点注册表
    let mut endpoints: HashMap<String, Arc<dyn KiroEndpoint>> = HashMap::new();
    {
        // 主协议端点（可作为 default_endpoint / 凭据 endpoint 字段）
        let ide = IdeEndpoint::new();
        endpoints.insert(ide.name().to_string(), Arc::new(ide));
        let cli = CliEndpoint::new();
        endpoints.insert(cli.name().to_string(), Arc::new(cli));

        // 429 降级桶（换桶不换号）：均为内部 fallback 目标，无需出现在配置里。
        // 参考 demo 的多端点重试，并把 runtime 建成独立限流桶。
        //
        // IDE 协议链（origin=AI_EDITOR）：ide(q) ↔ runtime(kiro.dev) ↔ codewhisperer(独立 host) ↔ amazonq(q 上不同服务)
        // runtime.kiro.dev：与 q.amazonaws.com 限流桶独立
        let runtime = RuntimeEndpoint::new();
        endpoints.insert(runtime.name().to_string(), Arc::new(runtime));
        // codewhisperer.amazonaws.com：独立 host 的 IDE 协议桶（demo index 1）
        let codewhisperer = CodeWhispererEndpoint::new();
        endpoints.insert(codewhisperer.name().to_string(), Arc::new(codewhisperer));
        // q host 上的 AmazonQ Developer 服务（demo index 2，不同 x-amz-target）
        let amazonq = AmazonQEndpoint::new();
        endpoints.insert(amazonq.name().to_string(), Arc::new(amazonq));

        // CLI 协议链（origin=KIRO_CLI）：cli(q) ↔ runtime_cli(kiro.dev)——同协议降级，不改凭据身份
        let runtime_cli = RuntimeCliEndpoint::new();
        endpoints.insert(runtime_cli.name().to_string(), Arc::new(runtime_cli));
    }

    // 校验默认端点存在
    if !endpoints.contains_key(&config.default_endpoint) {
        tracing::error!("默认端点 \"{}\" 未注册", config.default_endpoint);
        std::process::exit(1);
    }

    // 校验所有凭据声明的端点都已注册
    for cred in &credentials_list {
        let name = cred.endpoint.as_deref().unwrap_or(&config.default_endpoint);
        if !endpoints.contains_key(name) {
            tracing::error!(
                "凭据 id={:?} 指定了未知端点 \"{}\"（已注册: {:?}）",
                cred.id,
                name,
                endpoints.keys().collect::<Vec<_>>()
            );
            std::process::exit(1);
        }
    }

    let mut endpoint_names: Vec<String> = endpoints.keys().cloned().collect();
    endpoint_names.sort();

    // 启动时打印限流/重试/负载相关配置，便于运维确认开关是否生效
    tracing::info!("已注册端点桶: {:?}", endpoint_names);
    tracing::info!("默认端点: {}", config.default_endpoint);
    tracing::info!("负载均衡模式: {}", config.load_balancing_mode);
    if config.account_throttle_failover {
        tracing::info!(
            "账号级风控转移: 开启（检测到 suspicious activity 时冷却 {}s 并切换凭据）",
            config.account_throttle_cooldown_secs
        );
    } else {
        tracing::info!(
            "账号级风控转移: 关闭（suspicious activity 429 按普通瞬态错误退避重试，不冷却/不换号）"
        );
    }

    boot.mark("config_and_credentials_parse");

    // 创建 MultiTokenManager 和 KiroProvider
    let token_manager = MultiTokenManager::new(
        config.clone(),
        credentials_list,
        proxy_config.clone(),
        Some(credentials_path.into()),
        is_multiple_format,
    )
    .unwrap_or_else(|e| {
        tracing::error!("创建 Token 管理器失败: {}", e);
        std::process::exit(1);
    });
    let token_manager = Arc::new(token_manager);
    let proxy_pool_path = token_manager.cache_dir().map(|d| d.join("proxy_pool.json"));
    let proxy_pool = Arc::new(admin::proxy_pool::ProxyPoolManager::new(
        proxy_pool_path,
        config.tls_backend,
    ));

    // 代理封号台账：与凭据生命周期解耦，死号被保留期清理后统计依然在。
    let ban_ledger = Arc::new(admin::proxy_ban_stats::ProxyBanLedger::new(
        token_manager
            .cache_dir()
            .map(|d| d.join("proxy_ban_stats.json")),
    ));
    token_manager.set_ban_ledger(ban_ledger.clone());

    // 出口 IP 信誉档案：判断出口是否已被公开情报库标记为代理。
    // 线上实测过剂量-反应关系（本机 VPS IP 中位存活 8 分钟 vs 租用机房 IP 63 分钟），
    // 出口的「已被标记程度」直接决定账号能活多久，所以这件事必须能查、能看。
    let proxy_reputation = Arc::new(admin::proxy_reputation::ProxyReputationStore::new(
        token_manager
            .cache_dir()
            .map(|d| d.join("proxy_reputation.json")),
        config.tls_backend,
    ));
    // 代理池据此对烧号多的出口降权（相对池内中位数，全池一样烂时不降）
    proxy_pool.set_ban_ledger(ban_ledger.clone());
    {
        // 升级首日的历史回填：credentials.json 里尚未被清理的死号先进台账，
        // 否则统计要从零重新积累。幂等，重复启动不会重复计数。
        let snapshot = token_manager.snapshot();
        let backfilled = ban_ledger.backfill_from_credentials(snapshot.entries.iter().filter_map(
            |entry| {
                Some(admin::proxy_ban_stats::BanObservation {
                    credential_id: entry.id,
                    email: entry.email.clone(),
                    banned_at: entry.died_at.clone()?,
                    added_at: entry.added_at.clone(),
                    reason: Some("backfilled from credentials.json".to_string()),
                    proxy_url: entry.proxy_url.clone(),
                    successes_before_ban: Some(entry.success_count),
                    requests_before_ban: Some(
                        entry.success_count.saturating_add(entry.total_failure_count),
                    ),
                })
            },
        ));
        if backfilled > 0 {
            tracing::info!("代理封号台账：回填 {} 条历史封号记录", backfilled);
        }
        ban_ledger.observe_bindings(
            snapshot
                .entries
                .iter()
                .map(|entry| (entry.proxy_url.clone(), entry.id)),
        );
    }
    let kiro_provider = Arc::new(KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
        Some(proxy_pool.clone()),
    ));
    boot.mark("token_manager_and_provider");

    // 初始化 count_tokens 配置
    token::init_config(token::CountTokensConfig {
        api_url: config.count_tokens_api_url.clone(),
        api_key: config.count_tokens_api_key.clone(),
        auth_type: config.count_tokens_auth_type.clone(),
        proxy: proxy_config,
        tls_backend: config.tls_backend,
    });

    // 客户端 Key 管理器 + 用量记录器 + 聚合器（与凭据文件同目录）
    let cache_dir = token_manager
        .cache_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let key_supplier_service = initialize_key_supplier_service(
        &mut config,
        &config_path,
        &cache_dir,
        token_manager.clone(),
    );
    boot.mark("open_key_supplier_db");

    let client_keys_path = admin::client_keys::default_path_in(&cache_dir);
    let client_key_manager = std::sync::Arc::new(
        admin::ClientKeyManager::load(&client_keys_path).unwrap_or_else(|e| {
            tracing::warn!(
                "加载客户端 Key 失败 ({}): {}",
                client_keys_path.display(),
                e
            );
            admin::ClientKeyManager::new()
        }),
    );
    let usage_recorder = std::sync::Arc::new(admin::UsageRecorder::with_retention(
        cache_dir.clone(),
        config.usage_log_retention_days as i64,
    ));
    let usage_aggregator = std::sync::Arc::new(admin::UsageAggregator::new());
    usage_aggregator.rebuild_from_logs(&cache_dir);

    // 账号分组注册表（持久化到 groups.json）。
    // 启动时若文件不存在则首次创建，并把现有凭据 / 客户端 Key 的 groups 字段反向迁移进去，
    // 保证老用户升级后所有已用分组都自动注册，不会因为本次改造而消失。
    let groups_path = admin::groups::default_path_in(&cache_dir);
    let group_manager =
        std::sync::Arc::new(admin::GroupManager::load(&groups_path).unwrap_or_else(|e| {
            tracing::warn!("加载分组注册表失败 ({}): {}", groups_path.display(), e);
            admin::GroupManager::new()
        }));
    {
        let mut all_used: Vec<String> = token_manager.list_credential_groups();
        all_used.extend(client_key_manager.used_group_names());
        let added = group_manager.bootstrap_from_existing(all_used);
        if added > 0 {
            tracing::info!("分组注册表：自动迁移 {} 个已用分组", added);
        }
    }

    boot.mark("pre_trace_db");

    // 请求链路追踪存储（SQLite，traces.db）。失败不致命：trace 不可用但服务正常。
    let trace_store: Option<admin::SharedTraceStore> = match admin::TraceStore::open(
        cache_dir.join("traces.db"),
        config.trace_enabled,
        config.trace_retention_days,
    ) {
        Ok(s) => Some(std::sync::Arc::new(s)),
        Err(e) => {
            tracing::warn!("打开 traces.db 失败，请求链路追踪不可用: {}", e);
            None
        }
    };
    boot.mark("open_traces_db");

    if let Some(store) = &trace_store {
        let store = store.clone();
        common::drain::register_exit_task("traces.db wal_checkpoint", move || {
            if let Err(error) = store.checkpoint_truncate() {
                tracing::warn!(%error, "traces.db WAL 截断失败，下次启动仍需为它做恢复");
            }
        });
    }

    // ───── 把请求路径上的同步磁盘 I/O 全部挪到后台 ─────
    //
    // 这三处此前都在 Tokio worker 上做同步写：trace 是 SQLite 事务、usage_log 是每条
    // 记录一次 flush、客户端 Key 是每个请求重写整个 JSON。并发一高，worker 全部堵在
    // 磁盘上，运行时整体停转——线上表现为上游一条 TCP 连接都没有、入站连接堆积、
    // 吞吐从 219/分钟塌到个位数，只有重启才能恢复。
    if let Some(store) = &trace_store {
        store.spawn_writer();
    }
    usage_recorder
        .clone()
        .spawn_flusher(std::time::Duration::from_secs(2));
    client_key_manager
        .clone()
        .spawn_flusher(std::time::Duration::from_secs(5));

    let snapshot_policy = admin::error_snapshot_db::ErrorSnapshotPolicy::from_config(&config);
    let error_snapshot_store = match admin::ErrorSnapshotStore::open(
        cache_dir.join("error_snapshots.db"),
        cache_dir.join("error-snapshot-fallback"),
        snapshot_policy,
    ) {
        Ok(store) => std::sync::Arc::new(store),
        Err(error) => {
            tracing::error!(%error, "打开 error_snapshots.db 失败，使用内存索引和磁盘 fallback");
            std::sync::Arc::new(
                admin::ErrorSnapshotStore::open_in_memory_with_fallback(
                    cache_dir.join("error-snapshot-fallback"),
                    admin::error_snapshot_db::ErrorSnapshotPolicy::from_config(&config),
                )
                .expect("内存错误快照 store 初始化失败"),
            )
        }
    };
    boot.mark("open_error_snapshots_db");

    {
        let store = error_snapshot_store.clone();
        common::drain::register_exit_task("error_snapshots.db wal_checkpoint", move || {
            if let Err(error) = store.checkpoint_truncate() {
                tracing::warn!(%error, "error_snapshots.db WAL 截断失败，下次启动仍需为它做恢复");
            }
        });
    }

    // fallback 导入、清理与 trace 回链全部在 blocking pool 中分批执行。
    // 服务会先继续启动，历史快照库再大也不会占住 Tokio 请求线程。
    admin::error_snapshot_maintenance::spawn_scheduler(
        error_snapshot_store.clone(),
        trace_store.clone(),
    );

    // 启动后定期清理过期 usage_log 与 trace 记录
    {
        let recorder = usage_recorder.clone();
        let trace_store = trace_store.clone();
        tokio::spawn(async move {
            let day = std::time::Duration::from_secs(24 * 3600);
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                recorder.cleanup_old_logs();
                if let Some(ts) = &trace_store {
                    ts.cleanup();
                }
                tokio::time::sleep(day).await;
            }
        });
    }

    // 判死凭据的保留期清理：403 封号后凭据先禁用留档，过期再删。
    // 每小时一轮 —— 与线上约每小时一次的封号节奏同量级，不必更频繁。
    {
        let manager = token_manager.clone();
        tokio::spawn(async move {
            let hour = std::time::Duration::from_secs(3600);
            // 启动后先等一会：此时 credentials.json 刚加载完，让回填的持久化先落地
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            loop {
                // 每轮重新读取，管理端改完保留期后下一轮即生效，无需重启
                let hours = u64::from(manager.dead_credential_retention_hours());
                let removed =
                    manager.cleanup_dead_credentials(std::time::Duration::from_secs(hours * 3600));
                if removed > 0 {
                    tracing::info!("已清理 {} 个超过 {} 小时保留期的判死凭据", removed, hours);
                }
                tokio::time::sleep(hour).await;
            }
        });
    }

    // 每次启动幂等确保 config.apiKey 对应的系统 Key 存在（不可删除 / 不可轮换）。
    // 老部署升级时会把已有的 apiKey 补成系统 Key，保证根密钥始终可用于 /v1 流量。
    if let Some(initial_key) = bootstrap_key.as_ref() {
        client_key_manager.ensure_system_key(
            "默认密钥".to_string(),
            Some("由 config.json apiKey 自动导入（系统密钥）".to_string()),
            initial_key.clone(),
        );
    }

    // CacheMeter：模拟 Anthropic 缓存、计量 cache_read/creation token 的进程内组件。
    // 持久化到 cache_dir/cache_metering.json，启动时自动加载未过期条目。
    let cache_policy = anthropic::cache_metering::CachePolicy {
        enabled: config.cache_metering_enabled,
        default_ttl_secs: config.cache_default_ttl_secs,
        auto_without_cache_control: config.cache_auto_without_control,
        rolling_prefix_enabled: config.cache_rolling_prefix_enabled,
        rolling_prefix_limit: config.cache_rolling_prefix_limit,
        capacity: config.cache_capacity,
        flush_interval_secs: config.cache_flush_interval_secs,
    }
    .validate()
    .unwrap_or_else(|error| {
        tracing::warn!(%error, "缓存策略配置无效，回退默认值");
        anthropic::cache_metering::CachePolicy::default()
    });
    let cache_meter = std::sync::Arc::new(anthropic::cache_metering::CacheMeter::with_policy(
        Some(cache_dir.join("cache_metering.json")),
        cache_policy,
    ));
    cache_meter.clone().spawn_background();

    // 模型映射：请求时把源模型名（如 gpt-5.5）转发到目标模型名（如 claude-opus-4.8）。
    // 首次启动写入内置默认映射；源名不会出现在 /v1/models 列表里。
    let model_mappings_path = admin::model_mapping::default_path_in(&cache_dir);
    let model_mapping_manager = std::sync::Arc::new(
        admin::ModelMappingManager::load(&model_mappings_path).unwrap_or_else(|e| {
            tracing::warn!(
                "加载模型映射失败 ({}): {}",
                model_mappings_path.display(),
                e
            );
            admin::ModelMappingManager::new()
        }),
    );

    let model_profiles_path = cache_dir.join("model_profiles.json");
    let model_profile_store = std::sync::Arc::new(
        anthropic::model_profile::ModelProfileStore::load(&model_profiles_path).unwrap_or_else(
            |error| {
                tracing::warn!(%error, path = %model_profiles_path.display(), "模型资料加载失败，使用空持久化资料");
                anthropic::model_profile::ModelProfileStore::new_empty_at(&model_profiles_path)
            },
        ),
    );
    model_profile_store.set_exact_answers_enabled(config.model_profile_exact_answers_enabled);
    let model_profile_sync = std::sync::Arc::new(
        admin::model_profile_sync::ModelProfileSyncService::new(token_manager.clone()),
    );

    // ───── 批发号池系统（wholesale）─────
    // 独立 SQLite（wholesale.db）+ 复用 token_manager 探活/建 ksk。
    let wholesale_state: Option<wholesale::WholesaleState> =
        match wholesale::WholesaleStore::open(cache_dir.join("wholesale.db")) {
            Ok(store) => {
                let store = std::sync::Arc::new(store);
                let ws_config = wholesale::WholesaleConfig::default();
                let probe_interval = ws_config.probe_interval_secs;
                let service = std::sync::Arc::new(wholesale::WholesaleService::new(
                    store.clone(),
                    token_manager.clone(),
                    ws_config,
                ));
                // 后台探活轮询：更新号池状态 + 母号死亡联动 + 质保退款
                {
                    let svc = service.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        loop {
                            svc.probe_round().await;
                            // 加 jitter，避免整点对上游齐发
                            let jitter = fastrand::u64(0..(probe_interval / 5 + 1));
                            tokio::time::sleep(std::time::Duration::from_secs(
                                probe_interval + jitter,
                            ))
                            .await;
                        }
                    });
                }
                let admin_key_arc = std::sync::Arc::new(parking_lot::RwLock::new(
                    config.admin_api_key.clone().unwrap_or_default(),
                ));
                tracing::info!("批发号池系统已启用: /wholesale");
                Some(wholesale::WholesaleState::new(
                    service,
                    store,
                    admin_key_arc,
                ))
            }
            Err(e) => {
                tracing::warn!("打开 wholesale.db 失败，批发号池系统不可用: {}", e);
                None
            }
        };

    let anthropic_app = anthropic::create_router(
        Some(kiro_provider.clone()),
        config.extract_thinking,
        config.tool_compatibility_mode,
        Some(client_key_manager.clone()),
        Some(usage_recorder.clone()),
        Some(usage_aggregator.clone()),
        Some(cache_meter.clone()),
        trace_store.clone(),
        Some(error_snapshot_store.clone()),
        Some(model_mapping_manager.clone()),
        Some(model_profile_store.clone()),
    );

    // 构建 Admin API 路由（配置了非空 adminApiKey 时启用）
    // 安全检查：空字符串被视为未配置，防止空 key 绕过认证
    let app = if let Some(admin_key) = &config.admin_api_key {
        if admin_key.trim().is_empty() {
            tracing::warn!("admin_api_key 配置为空，Admin API 未启用");
            anthropic_app.nest(
                "/api/admin",
                admin::create_key_supplier_webhook_router(key_supplier_service.clone()),
            )
        } else {
            // Admin 查询需要一个确定的 store；traces.db 打开失败时用内存兜底（仅本进程有效）
            let admin_trace_store = trace_store.clone().unwrap_or_else(|| {
                std::sync::Arc::new(
                    admin::TraceStore::open_in_memory().expect("内存 trace store 初始化失败"),
                )
            });
            let admin_service = admin::AdminService::new(
                token_manager.clone(),
                endpoint_names.clone(),
                proxy_pool.clone(),
            )
            .with_kiro_provider(kiro_provider.clone())
            .with_cache_meter(cache_meter.clone())
            .with_model_profiles(model_profile_store.clone(), model_profile_sync.clone())
            .with_log_governance(
                Some(admin_trace_store.clone()),
                Some(usage_recorder.clone()),
                Some(error_snapshot_store.clone()),
            )
            .with_usage_aggregator(usage_aggregator.clone());
            let admin_state = admin::AdminState::new(
                admin_key,
                admin_service,
                client_key_manager.clone(),
                usage_aggregator.clone(),
                admin_trace_store,
                error_snapshot_store.clone(),
                group_manager.clone(),
                model_mapping_manager.clone(),
                key_supplier_service.clone(),
            );

            // 启动余额后台刷新调度器（每 5 分钟一次，与缓存 TTL 对齐）
            admin_state
                .service
                .start_balance_refresher(std::time::Duration::from_secs(300));

            // 把余额缓存接到供货商补货闸上，让「额度水位」判定能拿到剩余额度。
            // 必须在这里做而不是构造时：供货商服务先于 AdminService 建好。
            // 漏掉这步是静默降级——补货只认封号与 402，额度快用光的号仍算可用。
            if let Some(supplier_service) = key_supplier_service.as_ref() {
                supplier_service.set_quota_source(admin_state.service.clone());
            }

            // 启动代理池健康检查调度器（每 5 分钟一次）
            admin_state
                .service
                .start_proxy_health_checker(std::time::Duration::from_secs(300));

            // 启动烧号出口隔离守卫：封号事件即触发，隔离脏出口并把幸存号迁走
            admin_state.service.start_proxy_guard();

            admin_state
                .service
                .set_proxy_reputation(proxy_reputation.clone());

            // 实测卖价（¥/credit）：跑一次利润报表就更新一次，之后每号收益核算都用它。
            // 落盘是因为凭据列表接口不能每次都去打 NewAPI。
            admin_state
                .service
                .set_sell_rate_store(Arc::new(admin::credential_earnings::SellRateStore::new(
                    token_manager
                        .cache_dir()
                        .map(|d| d.join("profit_sell_rate.json")),
                )));

            // 启动自动更新调度器：每分钟检查一次本地时间，到达 update_auto_apply_time
            // 且开启 update_auto_apply 时执行一次更新；否则静默等待。
            admin_state.service.start_auto_update_scheduler();

            let admin_app = admin::create_admin_router(admin_state);

            // 创建 Admin UI 路由
            let admin_ui_app = admin_ui::create_admin_ui_router();

            tracing::info!("Admin API 已启用");
            tracing::info!("Admin UI 已启用: /admin");
            anthropic_app
                .nest("/api/admin", admin_app)
                .nest("/admin", admin_ui_app)
        }
    } else {
        anthropic_app.nest(
            "/api/admin",
            admin::create_key_supplier_webhook_router(key_supplier_service.clone()),
        )
    };

    // 挂载批发号池路由（若可用）
    let app = if let Some(ws_state) = wholesale_state {
        app.nest("/wholesale", wholesale::create_wholesale_router(ws_state))
    } else {
        app
    };

    // 启动服务器
    let addr = format!("{}:{}", config.host, config.port);
    tracing::info!("启动 Anthropic API 端点: {}", addr);
    tracing::info!("可用 API:");
    tracing::info!("  GET  /v1/models");
    tracing::info!("  POST /v1/messages");
    tracing::info!("  POST /v1/messages/count_tokens");
    tracing::info!("  POST /v1/chat/completions");
    tracing::info!("  POST /v1/responses");
    tracing::info!("Admin API:");
    tracing::info!("  GET  /api/admin/credentials");
    tracing::info!("  POST /api/admin/credentials/:index/disabled");
    tracing::info!("  POST /api/admin/credentials/:index/priority");
    tracing::info!("  POST /api/admin/credentials/:index/reset");
    tracing::info!("  GET  /api/admin/credentials/:index/balance");
    tracing::info!("Admin UI:");
    tracing::info!("  GET  /admin");

    // 统计在途流式响应，供在线更新挑「没有流在跑」的时刻退出，避免把客户的回答
    // 砍在半句话上。只包 SSE：非流式响应是毫秒级，计入会让计数永远不归零。
    let app = app.layer(axum::middleware::from_fn(track_streaming_responses));
    boot.mark("build_router");

    // 下游连接开 TCP_NODELAY：SSE 是「大量小写」，Nagle 会把小帧攒到对端 ACK 回来
    // 才发，撞上 delayed ACK 时单次最坏加约 40ms。axum/tokio 都不默认开，必须自己设。
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap()
        .tap_io(|tcp_stream| {
            if let Err(err) = tcp_stream.set_nodelay(true) {
                tracing::warn!(%err, "设置 TCP_NODELAY 失败，该连接的流式输出可能被 Nagle 攒包");
            }
        });
    boot.mark("bind_listener");
    // 这行日志的时间点就是「开始能接客户流量」的时刻，减去容器 StartedAt 即完整冷启动。
    boot.log_summary();
    axum::serve(listener, app).await.unwrap();
}

/// 把在途流凭证绑到 SSE 响应体上。
///
/// 凭证必须活到 body 传完：SSE 的 handler 拿到上游第一个字节就返回了，绑在返回值上
/// 会让计数在流刚开始时就归零。客户端中途断开时 body 是被 drop 的，`StreamGuard`
/// 的 `Drop` 同样会释放，不会漏计导致进程永远等不到安静时刻。
async fn track_streaming_responses(
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use futures::StreamExt;

    let response = next.run(request).await;
    let is_stream = response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"text/event-stream"));
    if !is_stream {
        return response;
    }
    let guard = common::drain::StreamGuard::acquire();
    response.map(|body| {
        // 闭包持有凭证，流被读完或被 drop 时闭包一起析构，计数随之释放。
        let chunks = body.into_data_stream().map(move |chunk| {
            let _hold = &guard;
            chunk
        });
        axum::body::Body::from_stream(chunks)
    })
}

fn initialize_key_supplier_service(
    config: &mut Config,
    config_path: &str,
    cache_dir: &std::path::Path,
    token_manager: Arc<MultiTokenManager>,
) -> Option<Arc<admin::key_supplier::service::KeySupplierService>> {
    // 历史单供货商配置 → 多供货商列表。迁移只做一次，落盘后后续启动走正常分支。
    let (mut suppliers, common_import, migrated) =
        match admin::key_supplier::config::load_suppliers_with_common(config) {
            Ok(result) => result,
            Err(error) => {
                tracing::error!(
                    %error,
                    "key supplier configuration is invalid; supplier service is disabled"
                );
                return None;
            }
        };

    // 每家供货商都需要一个 webhook token 才能收回调；缺的补上。
    let mut needs_save = migrated;
    for entry in &mut suppliers {
        if entry.settings.webhook_token.trim().is_empty() {
            entry.settings.webhook_token = admin::key_supplier::config::generate_webhook_token();
            needs_save = true;
        }
    }
    if needs_save {
        config.key_supplier_common = (&common_import).into();
        admin::key_supplier::config::store_suppliers(config, &suppliers);
        if let Err(error) = config.save() {
            tracing::error!(
                %error,
                "key supplier configuration could not be persisted; supplier service is disabled"
            );
            return None;
        }
        if migrated {
            tracing::info!(
                suppliers = suppliers.len(),
                "migrated legacy key supplier config into the multi-supplier list"
            );
        }
    }

    let store = match admin::key_supplier::store::SupplierEventStore::open(
        cache_dir.join("key_supplier.db"),
    ) {
        Ok(store) => Arc::new(store),
        Err(error) => {
            tracing::error!(
                %error,
                "key supplier event store could not be opened; supplier service is disabled"
            );
            return None;
        }
    };
    {
        let store = store.clone();
        common::drain::register_exit_task("key_supplier.db wal_checkpoint", move || {
            if let Err(error) = store.checkpoint_truncate() {
                tracing::warn!(%error, "key_supplier.db WAL 截断失败，下次启动仍需为它做恢复");
            }
        });
    }
    // 全局号池配置。校验失败时**不能**退回默认值（等于关闭）——那会让系统回到
    // 不受限的逐家采购继续花钱，而用户配这个功能的意图明显是要限制采购。
    // 装一份「中毒」配置（启用但目标存量 0），使后续每次触发都跳过。
    let pool = match admin::key_supplier::config::PoolRuntimeConfig::from_persisted(
        &config.key_supplier_pool,
    ) {
        Ok(pool) => pool,
        Err(error) => {
            tracing::error!(
                %error,
                "key supplier pool configuration is invalid; purchases will be skipped until it is fixed"
            );
            admin::key_supplier::config::PoolRuntimeConfig::poisoned()
        }
    };
    if pool.enabled {
        tracing::info!(
            target_count = pool.target_count,
            low_quota_threshold = pool.low_quota_threshold,
            "全局号池已启用：所有采购来的可用号合计不超过目标存量，各家自己的补货闸不再参与判定"
        );
    }
    let service = Arc::new(
        admin::key_supplier::service::KeySupplierService::new_with_token_manager(
            store,
            suppliers,
            token_manager,
        )
        .with_config_path(config_path)
        .with_common_import(common_import)
        .with_pool_config(pool),
    );
    service.start_processor();
    Some(service)
}

/// 文件不存在时初始化配置/凭证文件
///
/// - `config.json`：写入带随机 `apiKey`（首次启动自动导入为第一条客户端 Key）/ `adminApiKey`（管理面板登录密钥）
///   的最小默认配置；`host` 设为 `0.0.0.0` 以适配容器场景，端口/默认端点等其余字段沿用代码默认值。
/// - `credentials.json`：写入空数组 `[]`，便于后续通过 Admin UI 添加凭据。
///
/// 任一步失败都仅打印警告，不中断启动；后续 `Config::load` / `CredentialsConfig::load`
/// 仍会按既有逻辑处理（失败再退出）。
fn ensure_config_files(config_path: &str, credentials_path: &str) {
    let config_p = std::path::Path::new(config_path);
    if !config_p.exists() {
        if let Some(parent) = config_p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建配置目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        let api_key = format!("sk-kiro-rs-{}", random_token(24));
        let admin_api_key = format!("sk-admin-{}", random_token(24));
        let default = serde_json::json!({
            "host": "0.0.0.0",
            "port": 8990,
            "apiKey": api_key,
            "adminApiKey": admin_api_key,
            "region": "us-east-1",
            "tlsBackend": "rustls",
            "defaultEndpoint": "ide"
        });
        match serde_json::to_string_pretty(&default)
            .map_err(anyhow::Error::from)
            .and_then(|s| std::fs::write(config_p, s).map_err(anyhow::Error::from))
        {
            Ok(_) => {
                tracing::info!("已生成默认配置: {}", config_p.display());
                tracing::info!(
                    "  apiKey      = {}（首次启动时将自动导入为第一条客户端 Key）",
                    api_key
                );
                tracing::info!("  adminApiKey = {}（管理面板登录密钥）", admin_api_key);
                tracing::info!("请妥善保存上述密钥，可在配置文件中修改");
            }
            Err(e) => tracing::warn!("写入默认配置失败 {}: {}", config_p.display(), e),
        }
    }

    let cred_p = std::path::Path::new(credentials_path);
    if !cred_p.exists() {
        if let Some(parent) = cred_p.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    tracing::warn!("创建凭证目录失败 {}: {}", parent.display(), e);
                }
            }
        }
        if let Err(e) = std::fs::write(cred_p, "[]\n") {
            tracing::warn!("写入空凭证文件失败 {}: {}", cred_p.display(), e);
        } else {
            tracing::info!(
                "已生成空凭证文件: {}（可通过 Admin UI 添加凭据）",
                cred_p.display()
            );
        }
    }
}

/// 生成一段长度为 `len` 的字母数字随机字符串，用于默认 API Key
fn random_token(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789";
    (0..len)
        .map(|_| {
            let idx = fastrand::usize(..CHARSET.len());
            CHARSET[idx] as char
        })
        .collect()
}

#[cfg(test)]
mod logging_filter_tests {
    use super::effective_log_filter;

    #[test]
    fn debug_filter_suppresses_transport_frame_noise_by_default() {
        assert_eq!(
            effective_log_filter(Some("debug")),
            "debug,h2=info,hyper=info,reqwest=info"
        );
    }

    #[test]
    fn explicit_module_directives_override_transport_defaults() {
        assert_eq!(
            effective_log_filter(Some("kiro_rs=debug,h2=trace,reqwest=warn")),
            "kiro_rs=debug,h2=trace,reqwest=warn,hyper=info"
        );
    }

    #[test]
    fn missing_filter_defaults_to_info_without_duplicate_directives() {
        assert_eq!(
            effective_log_filter(None),
            "info,h2=info,hyper=info,reqwest=info"
        );
    }
}

#[cfg(test)]
mod drain_layer_tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode, header};
    use axum::routing::get;
    use axum::{Router, middleware};
    use futures::StreamExt;
    use tower::ServiceExt;

    use crate::common::drain::{TEST_SERIAL, streams_in_flight};

    fn app() -> Router {
        Router::new()
            .route(
                "/sse",
                get(|| async {
                    // 两块数据之间不结束，模拟一条还在传的流。
                    let chunks = futures::stream::iter(vec![
                        Ok::<_, std::io::Error>("data: a\n\n"),
                        Ok("data: b\n\n"),
                    ]);
                    crate::common::sse::sse_response(Body::from_stream(chunks))
                }),
            )
            .route("/json", get(|| async { axum::Json(serde_json::json!({})) }))
            .layer(middleware::from_fn(super::track_streaming_responses))
    }

    #[tokio::test]
    async fn only_streaming_responses_are_counted_and_they_release_when_the_body_ends() {
        let _serial = TEST_SERIAL.lock().unwrap();
        let before = streams_in_flight();

        // 非流式响应不能计数：它们是毫秒级的，计入会让计数几乎永不归零，
        // 于是每次更新都被迫走超时硬退，等于这套机制白做。
        let json = app()
            .oneshot(Request::builder().uri("/json").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(json.status(), StatusCode::OK);
        assert_eq!(streams_in_flight(), before);

        let sse = app()
            .oneshot(Request::builder().uri("/sse").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(
            sse.headers().get(header::CONTENT_TYPE).unwrap(),
            "text/event-stream"
        );
        // handler 早就返回了，body 还没读完——此时必须已经在计数，否则进程会以为
        // 现在就是安静时刻，把客户的回答砍在半句话上。
        let mut chunks = sse.into_body().into_data_stream();
        assert!(chunks.next().await.is_some());
        assert_eq!(streams_in_flight(), before + 1);

        // 读完剩余数据并释放流，计数必须归零，否则永远等不到安静时刻。
        while chunks.next().await.is_some() {}
        drop(chunks);
        assert_eq!(streams_in_flight(), before);
    }

    #[tokio::test]
    async fn wrapping_the_body_must_not_buffer_chunks() {
        // 这是这层中间件最危险的失手方式：为了统计而重新包装响应体，如果包装引入了
        // 缓冲，第一块就要等整条流结束才吐出去——那等于把 `common::sse` 那套首字节
        // 优化（x-accel-buffering / no-transform / TCP_NODELAY）悄悄抵消掉，而且本地
        // 直连和单测都看不出来，只有反代后面的真实客户会感觉到卡。
        let _serial = TEST_SERIAL.lock().unwrap();
        let before = streams_in_flight();

        let gate = std::sync::Arc::new(tokio::sync::Notify::new());
        let handler_gate = gate.clone();
        let app = Router::new()
            .route(
                "/slow",
                get(move || {
                    let gate = handler_gate.clone();
                    async move {
                        let first = futures::stream::once(async {
                            Ok::<_, std::io::Error>("data: first\n\n")
                        });
                        // 第二块被闸门挡住，模拟「上游还没吐下一段」。
                        let second = futures::stream::once(async move {
                            gate.notified().await;
                            Ok("data: second\n\n")
                        });
                        crate::common::sse::sse_response(Body::from_stream(first.chain(second)))
                    }
                }),
            )
            .layer(middleware::from_fn(super::track_streaming_responses));

        let response = app
            .oneshot(Request::builder().uri("/slow").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let mut chunks = response.into_body().into_data_stream();

        // 第二块还被闸门挡着，第一块必须已经能拿到。超时即证明包装层在攒包。
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), chunks.next())
            .await
            .expect("包装层缓冲了 SSE：第二块还没放行，第一块就应该已经到达");
        assert!(first.is_some());
        // 流还在跑，计数必须是 1。
        assert_eq!(streams_in_flight(), before + 1);

        gate.notify_one();
        while chunks.next().await.is_some() {}
        drop(chunks);
        assert_eq!(streams_in_flight(), before);
    }

    #[tokio::test]
    async fn a_client_that_disconnects_mid_stream_still_releases_the_slot() {
        let _serial = TEST_SERIAL.lock().unwrap();
        // 客户端中途断开时 body 是被 drop 而不是读完的。漏了这条路径，计数会永久
        // 泄漏，之后每次在线更新都退化成超时硬退。
        let before = streams_in_flight();
        let sse = app()
            .oneshot(Request::builder().uri("/sse").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let mut chunks = sse.into_body().into_data_stream();
        assert!(chunks.next().await.is_some());
        assert_eq!(streams_in_flight(), before + 1);

        drop(chunks);
        assert_eq!(streams_in_flight(), before);
    }
}
