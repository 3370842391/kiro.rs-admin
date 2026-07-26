//! HTTP Client 构建模块
//!
//! 提供统一的 HTTP Client 构建功能，支持代理配置

use reqwest::{Client, Proxy, redirect::Policy};
use std::time::Duration;

use crate::model::config::TlsBackend;

/// 代理配置
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct ProxyConfig {
    /// 代理地址，支持 http/https/socks5
    pub url: String,
    /// 代理认证用户名
    pub username: Option<String>,
    /// 代理认证密码
    pub password: Option<String>,
}

impl ProxyConfig {
    /// 从 url 创建代理配置
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            username: None,
            password: None,
        }
    }

    fn first_proxy_url(&self) -> Option<String> {
        Self::split_candidates(&self.url)
            .into_iter()
            .next()
            .filter(|candidate| !Self::is_direct(candidate))
    }

    /// `direct` 表示显式直连。代理列表里也允许把它作为兜底候选。
    pub fn is_direct(value: &str) -> bool {
        value.trim().eq_ignore_ascii_case("direct")
    }

    /// 将逗号/空白/换行分隔的代理字符串拆成候选项，保留 `direct`。
    pub fn split_candidates(raw: &str) -> Vec<String> {
        raw.split(|c: char| c == ',' || c == ';' || c.is_whitespace())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .fold(Vec::new(), |mut acc, item| {
                if !acc
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(item))
                {
                    acc.push(item.to_string());
                }
                acc
            })
    }

    /// 单个配置是否是合法代理 URL 或 direct。
    pub fn is_supported_entry(value: &str) -> bool {
        let value = value.trim();
        Self::is_direct(value)
            || value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("socks5://")
            || value.starts_with("socks4://")
    }

    pub fn from_url_with_auth(
        url: impl Into<String>,
        username: Option<&str>,
        password: Option<&str>,
    ) -> Option<Self> {
        let url = url.into();
        if Self::is_direct(&url) {
            return None;
        }
        let mut proxy = Self::new(url);
        if let (Some(username), Some(password)) = (username, password) {
            if !username.is_empty() || !password.is_empty() {
                proxy = proxy.with_auth(username, password);
            }
        }
        Some(proxy)
    }

    /// 设置认证信息
    pub fn with_auth(mut self, username: impl Into<String>, password: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self.password = Some(password.into());
        self
    }
}

/// 构建 HTTP Client
///
/// # Arguments
/// * `proxy` - 可选的代理配置
/// * `timeout_secs` - 超时时间（秒）
///
/// # Returns
/// 配置好的 reqwest::Client
pub fn build_client(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_redirect_policy(proxy, timeout_secs, None, tls_backend, None)
}

pub fn build_client_no_redirect(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_redirect_policy(proxy, timeout_secs, None, tls_backend, Some(Policy::none()))
}

/// 构建带 read timeout（读空闲超时）的 HTTP Client。
///
/// `.timeout()` 是整个请求的**绝对**上限；`.read_timeout()` 是相邻两次成功读取
/// 之间的**空闲**上限。Kiro/Bedrock 上游对大 payload 常在返回 200 后首字节前挂死
/// 不吐字节，绝对超时要等满 `timeout_secs`（如 720s）才断，期间只有 ping 保活在
/// 空烧。设置 read timeout 后，空闲超过阈值即让底层读取报错，配合流层 idle
/// watchdog 尽早收尾，避免长时间挂死。
///
/// `read_timeout_secs` 为 `None` 或 `Some(0)` 时不设置（保持旧行为）。
pub fn build_client_with_read_timeout(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    read_timeout_secs: Option<u64>,
    tls_backend: TlsBackend,
) -> anyhow::Result<Client> {
    build_client_with_redirect_policy(proxy, timeout_secs, read_timeout_secs, tls_backend, None)
}

/// 是否允许对上游使用 HTTP/2 多路复用。
///
/// 默认关闭（见 `build_client_with_redirect_policy` 里的说明）。仅在需要临时回退
/// 验证时通过环境变量打开，读一次并缓存，避免每次建 client 都查环境变量。
fn upstream_http2_enabled() -> bool {
    use std::sync::OnceLock;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        let on = std::env::var("KIRO_UPSTREAM_HTTP2")
            .map(|v| matches!(v.trim(), "1" | "true" | "TRUE"))
            .unwrap_or(false);
        if on {
            tracing::warn!(
                "KIRO_UPSTREAM_HTTP2 已开启：上游将复用单条 h2 连接，高并发下可能出现流槽排队"
            );
        }
        on
    })
}

fn build_client_with_redirect_policy(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    read_timeout_secs: Option<u64>,
    tls_backend: TlsBackend,
    redirect_policy: Option<Policy>,
) -> anyhow::Result<Client> {
    // 连接池：显式对齐 kirogo（IdleConnTimeout=120s / MaxIdleConnsPerHost=128）。
    // 请求头改用 Connection: keep-alive 后，同一代理的 TLS 连接可复用，省掉每轮
    // 对话到美国上游 1-3s 的 TCP+TLS 握手（首字延迟的主要来源）。空闲连接在
    // pool_idle_timeout 后自动回收，陈旧连接被 reqwest 复用前会剔除，安全。
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .pool_idle_timeout(Duration::from_secs(120))
        .pool_max_idle_per_host(128);

    // 强制 HTTP/1.1，不走 HTTP/2 多路复用。
    //
    // 上游 `q.{region}.amazonaws.com` 支持 h2，而 hyper 默认把同一 host 的所有请求
    // 复用到**一条** TCP 连接上。一旦并发流数超过服务端的 SETTINGS_MAX_CONCURRENT_STREAMS，
    // 多出来的请求就在客户端排队等流槽 —— 不报错、不返回 429、纯粹干等。
    //
    // 线上实测（2026-07-26）：320 个并发入站请求对应上游**仅 1 条**连接，延迟呈明显
    // 双峰 —— 抢到流槽的 1.5s 返回，没抢到的排队 50s+。这也解释了几个反直觉现象：
    // 重启后好几分钟才复发（新连接流表是空的，流式请求会长时间占着槽位慢慢填满）、
    // 加账号完全无效（同一 host 共用同一条连接，账号数不增加流槽）。
    //
    // 改用 HTTP/1.1 后，连接池按需开多条独立连接（`pool_max_idle_per_host` 只限制
    // 空闲保活数，不限制并发上限），每个请求独占一条，不存在队头阻塞。代价是多一些
    // TLS 握手，但有连接池复用摊薄，远小于排队几十秒的损失。
    //
    // 逃生阀：设 `KIRO_UPSTREAM_HTTP2=1` 可恢复 h2，无需重新构建。
    if !upstream_http2_enabled() {
        builder = builder.http1_only();
    }

    // read timeout 仅在显式给出且 > 0 时设置；否则保持旧行为（仅绝对超时）。
    if let Some(secs) = read_timeout_secs {
        if secs > 0 {
            builder = builder.read_timeout(Duration::from_secs(secs));
        }
    }

    if let Some(policy) = redirect_policy {
        builder = builder.redirect(policy);
    }

    match tls_backend {
        TlsBackend::Rustls => {
            builder = builder.use_rustls_tls();
        }
        TlsBackend::NativeTls => {
            #[cfg(feature = "native-tls")]
            {
                builder = builder.use_native_tls();
            }
            #[cfg(not(feature = "native-tls"))]
            {
                anyhow::bail!("此构建版本未包含 native-tls 后端，请在配置中改用 rustls");
            }
        }
    }

    if let Some(proxy_config) = proxy {
        let Some(proxy_url) = proxy_config.first_proxy_url() else {
            return Ok(builder.build()?);
        };
        let mut proxy = Proxy::all(&proxy_url)?;

        // 设置代理认证
        if let (Some(username), Some(password)) = (&proxy_config.username, &proxy_config.password) {
            proxy = proxy.basic_auth(username, password);
        }

        builder = builder.proxy(proxy);
        tracing::debug!("HTTP Client 使用代理: {}", proxy_url);
    }

    Ok(builder.build()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 上游客户端必须禁用 h2 多路复用。
    ///
    /// 2026-07-26 线上事故回归守卫：hyper 默认把同一 host 的所有请求复用到一条
    /// h2 连接，超过服务端流槽上限的请求会在客户端静默排队。当时 320 个并发入站
    /// 请求只对应上游 1 条连接，延迟双峰（1.5s vs 50s+），加账号完全无效。
    ///
    /// 这条断言看的是源码而非行为——reqwest 没有暴露"当前是否 http1_only"的查询接口，
    /// 而这个设置一旦被误删不会有任何报错，只会在高并发时重新变慢。
    #[test]
    fn upstream_client_disables_http2_multiplexing_by_default() {
        let source = include_str!("http_client.rs");
        assert!(
            source.contains("builder.http1_only()"),
            "上游客户端必须 http1_only，否则高并发会在单条 h2 连接上排队"
        );
        assert!(
            source.contains("KIRO_UPSTREAM_HTTP2"),
            "保留环境变量逃生阀，便于不重新构建即可回退验证"
        );
        // 默认必须是关闭 h2
        unsafe { std::env::remove_var("KIRO_UPSTREAM_HTTP2") };
        assert!(!upstream_http2_enabled(), "默认不得启用 h2");
    }

    #[test]
    fn test_proxy_config_new() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        assert_eq!(config.url, "http://127.0.0.1:7890");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn test_proxy_config_with_auth() {
        let config = ProxyConfig::new("socks5://127.0.0.1:1080").with_auth("user", "pass");
        assert_eq!(config.url, "socks5://127.0.0.1:1080");
        assert_eq!(config.username, Some("user".to_string()));
        assert_eq!(config.password, Some("pass".to_string()));
    }

    #[test]
    fn test_build_client_without_proxy() {
        let client = build_client(None, 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_with_proxy() {
        let config = ProxyConfig::new("http://127.0.0.1:7890");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_build_client_uses_first_non_direct_candidate() {
        let config = ProxyConfig::new("http://127.0.0.1:7890, direct");
        let client = build_client(Some(&config), 30, TlsBackend::Rustls);
        assert!(client.is_ok());
    }

    #[test]
    fn test_split_proxy_candidates() {
        let candidates = ProxyConfig::split_candidates(
            "socks5://a:1080, http://b:8080\ndirect  socks5://a:1080",
        );
        assert_eq!(
            candidates,
            vec![
                "socks5://a:1080".to_string(),
                "http://b:8080".to_string(),
                "direct".to_string(),
            ]
        );
    }
}
