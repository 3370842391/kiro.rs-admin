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

#[tokio::main]
async fn main() {
    // 解析命令行参数
    let args = Args::parse();

    // 初始化日志
    tracing_subscriber::fmt()
        .with_env_filter(log_env_filter())
        .init();

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
    let config = Config::load(&config_path).unwrap_or_else(|e| {
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
    let kiro_provider = Arc::new(KiroProvider::with_proxy(
        token_manager.clone(),
        proxy_config.clone(),
        endpoints,
        config.default_endpoint.clone(),
        Some(proxy_pool.clone()),
    ));

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

    // 启动时先导入数据库故障期间留下的 fallback，并补齐最近 trace 的 snapshotId 回链。
    match error_snapshot_store.import_fallback() {
        Ok(report) if report.imported > 0 || report.existing > 0 || report.failed > 0 => {
            tracing::info!(
                imported = report.imported,
                existing = report.existing,
                failed = report.failed,
                "错误快照 fallback 启动导入完成"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::error!(%error, "错误快照 fallback 启动导入失败"),
    }
    if let Some(store) = &trace_store {
        match error_snapshot_store.recent_trace_links(chrono::Utc::now().timestamp() - 7 * 86_400) {
            Ok(links) => {
                for (trace_id, snapshot_id) in links {
                    store.link_snapshot(&trace_id, &snapshot_id);
                }
            }
            Err(error) => tracing::error!(%error, "读取最近错误快照回链失败"),
        }
    }

    {
        let store = error_snapshot_store.clone();
        let trace_store = trace_store.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
            loop {
                if let Err(error) = store.run_maintenance() {
                    tracing::error!(%error, "错误快照维护失败");
                }
                if let Some(trace_store) = &trace_store {
                    match store.recent_trace_links(chrono::Utc::now().timestamp() - 7 * 86_400) {
                        Ok(links) => {
                            for (trace_id, snapshot_id) in links {
                                trace_store.link_snapshot(&trace_id, &snapshot_id);
                            }
                        }
                        Err(error) => tracing::error!(%error, "错误快照回链维护失败"),
                    }
                }
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    }

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
    let wholesale_state: Option<wholesale::WholesaleState> = match wholesale::WholesaleStore::open(
        cache_dir.join("wholesale.db"),
    ) {
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
                        tokio::time::sleep(std::time::Duration::from_secs(probe_interval + jitter)).await;
                    }
                });
            }
            let admin_key_arc = std::sync::Arc::new(parking_lot::RwLock::new(
                config.admin_api_key.clone().unwrap_or_default(),
            ));
            tracing::info!("批发号池系统已启用: /wholesale");
            Some(wholesale::WholesaleState::new(service, store, admin_key_arc))
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
            anthropic_app
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
            );
            let admin_state = admin::AdminState::new(
                admin_key,
                admin_service,
                client_key_manager.clone(),
                usage_aggregator.clone(),
                admin_trace_store,
                error_snapshot_store.clone(),
                group_manager.clone(),
                model_mapping_manager.clone(),
            );

            // 启动余额后台刷新调度器（每 5 分钟一次，与缓存 TTL 对齐）
            admin_state
                .service
                .start_balance_refresher(std::time::Duration::from_secs(300));

            // 启动代理池健康检查调度器（每 5 分钟一次）
            admin_state
                .service
                .start_proxy_health_checker(std::time::Duration::from_secs(300));

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
        anthropic_app
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

    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
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
