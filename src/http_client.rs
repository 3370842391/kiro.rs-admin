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

fn build_client_with_redirect_policy(
    proxy: Option<&ProxyConfig>,
    timeout_secs: u64,
    read_timeout_secs: Option<u64>,
    tls_backend: TlsBackend,
    redirect_policy: Option<Policy>,
) -> anyhow::Result<Client> {
    // 不配置连接池、强制 HTTP/1.1 —— 与上游 ZyphrZero/kiro.rs 保持一致的保守模式。
    //
    // provider 对上游请求发 `Connection: close`（一请求一连接），因此连接池参数
    // （pool_idle_timeout / pool_max_idle_per_host）不会有可复用的空闲连接，
    // 留着只是误导读者，一并去掉。
    //
    // `http1_only()` 是把上面那个语义显式化：只要连接可能被复用，hyper 就会与
    // 上游协商 HTTP/2 并把所有请求多路复用到一条 TCP 连接上，超过服务端
    // SETTINGS_MAX_CONCURRENT_STREAMS 的请求会在客户端静默排队（2026-07-26 线上
    // 事故：320 并发对应上游 1 条连接，延迟双峰 1.5s / 50s+）。显式禁用 h2 让这条
    // 风险在协议层就不成立，而不是依赖「close 头恰好使 h2 失效」这个间接效果。
    //
    // 代价：每请求固定多约 141ms 握手（实测 q.us-east-1 的 TCP 66ms + TLS 75ms）。
    // 这是刻意选择——用确定的 141ms 换掉整类队头阻塞与陈旧连接复用风险。
    let mut builder = Client::builder()
        .timeout(Duration::from_secs(timeout_secs))
        .http1_only();

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

    /// 上游必须保持「一请求一连接 + 显式 HTTP/1.1」的保守模式。
    ///
    /// 2026-07-26 线上事故回归守卫：曾为省 141ms 握手改用 keep-alive + 连接池，
    /// 结果 hyper 与上游协商了 HTTP/2，把所有请求多路复用到一条 TCP 连接上。
    /// 320 个并发只对应上游 1 条连接，超出流槽的请求静默排队 50s+，且加账号无效。
    ///
    /// 断言源码而非行为：reqwest 未暴露「当前是否 http1_only」的查询接口，而这两个
    /// 设置一旦被误删不会有任何报错，只会在高并发时重新出现队头阻塞。
    #[test]
    fn upstream_uses_one_connection_per_request() {
        let client_source = include_str!("http_client.rs");
        assert!(
            client_source.contains(".http1_only()"),
            "必须显式禁用 h2，不能依赖 close 头间接生效"
        );
        // 这里不去断言「不含 pool_max_idle_per_host」：`include_str!` 读的是整个文件，
        // 断言自身的字面量也会被扫到，写成否定式必然自我命中（本次就踩了）。
        // 改为正面断言 builder 链的形状 —— 一请求一连接不需要任何连接池参数。
        assert!(
            client_source.contains(".timeout(Duration::from_secs(timeout_secs))\n        .http1_only()"),
            "client builder 应保持「仅 timeout + http1_only」的最小形状"
        );

        let provider_source = include_str!("kiro/provider.rs");
        assert!(
            provider_source.contains(r#".header("Connection", "close")"#),
            "上游请求必须发 Connection: close"
        );
        assert!(
            !provider_source.contains(r#""Connection", "keep-alive""#),
            "不得改回 keep-alive —— 连接复用会让 hyper 重新协商 h2"
        );
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
