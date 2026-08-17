//! 代理出口 IP 信誉检测（proxy_reputation.json）
//!
//! 回答一个买 IP 之前和用 IP 之中都必须知道的问题：**这个出口在公开情报库里是不是
//! 已经被标记成代理了**。
//!
//! 为什么这件事值得单独做一套。2026-08-17 的线上数据给出过一组剂量-反应关系：
//! 同一批账号、同样的用量、同一时段，唯一变量是出口 IP——
//!
//! | 出口 | 样本 | 中位存活 |
//! |------|------|----------|
//! | 服务器本机 VPS IP | 3 | 8 分钟 |
//! | 租来的机房/代理 IP | 33 | 63 分钟 |
//!
//! 8 倍差距。两者都是机房 IP，区别只在「已被标记的程度」。所以判据不是
//! 「机房还是家宽」，而是**有没有被标记**——一个干净的机房 IP 完全可用，一个被标记
//! 的家宽同样会烧号。而这件事只能靠查，不能靠猜。
//!
//! 探测走**代理自身**而不是直接查配置里的 host：
//! - 顺带拿到真实出口 IP，能发现「配置写着 A 实际从 B 出去」的轮换池
//! - host 与出口不一致时，按 host 查到的信誉是错的

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use crate::admin::proxy_ban_stats::normalize_proxy_key;
use crate::http_client::{ProxyConfig, build_client};
use crate::model::config::TlsBackend;

/// 单次探测超时（秒）。要串一次代理再打一次情报接口，给宽一点。
const PROBE_TIMEOUT_SECS: u64 = 15;
/// 并发探测数。免费情报接口限速 45 次/分钟，压着上限的一半走。
const PROBE_CONCURRENCY: usize = 6;
/// 情报接口。免费档不需要 key，返回 ASN / ISP / hosting / proxy 标记。
const LOOKUP_URL: &str =
    "http://ip-api.com/json/?fields=status,message,query,country,regionName,isp,org,as,asname,mobile,proxy,hosting";

/// 一个出口的信誉档案
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyReputation {
    /// 实测出口 IP。与配置里的 host 不一致即说明是轮换出口。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_ip: Option<String>,
    /// 实测出口与配置 host 不一致
    #[serde(default)]
    pub exit_ip_mismatch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// 形如 `AS62240 Clouvider`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub isp: Option<String>,
    /// 情报库判定为机房 / 托管。可用，但比未标记的差。
    #[serde(default)]
    pub hosting: bool,
    /// 情报库**已知为代理 / VPN**。这一项是最致命的：连免费库都认得出来，
    /// 上游的数据源只会更全。
    #[serde(default)]
    pub proxy: bool,
    #[serde(default)]
    pub mobile: bool,
    /// 最近一次探测时间（RFC3339）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    /// 探测失败原因；非空时上面各字段不可信
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 信誉等级。给 UI 上色与自动分配排序用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ReputationGrade {
    /// 没查过
    Unknown,
    /// 查失败
    Unreachable,
    /// 已被公开标记为代理 / VPN —— 优先淘汰
    FlaggedProxy,
    /// 机房 / 托管，但未被标记成代理 —— 可用
    Hosting,
    /// 既非机房也未被标记 —— 最干净
    Clean,
}

impl ProxyReputation {
    pub fn grade(&self) -> ReputationGrade {
        if self.error.is_some() {
            return ReputationGrade::Unreachable;
        }
        if self.checked_at.is_none() {
            return ReputationGrade::Unknown;
        }
        if self.proxy {
            ReputationGrade::FlaggedProxy
        } else if self.hosting {
            ReputationGrade::Hosting
        } else {
            ReputationGrade::Clean
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreData {
    #[serde(default = "schema_version")]
    version: u32,
    /// 键为归一化 `host:port`
    #[serde(default)]
    proxies: BTreeMap<String, ProxyReputation>,
}

fn schema_version() -> u32 {
    1
}

/// 一轮批量检测的摘要
#[derive(Debug, Clone, Default)]
pub struct CheckSummary {
    pub checked: usize,
    pub flagged_proxy: usize,
    pub hosting: usize,
    pub clean: usize,
    pub unreachable: usize,
    /// 实测出口与配置 host 不一致的数量（轮换池特征）
    pub mismatched: usize,
}

pub struct ProxyReputationStore {
    data: Mutex<StoreData>,
    path: Option<PathBuf>,
    tls_backend: TlsBackend,
}

impl ProxyReputationStore {
    pub fn new(path: Option<PathBuf>, tls_backend: TlsBackend) -> Self {
        let data = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<StoreData>(&s).ok())
            .unwrap_or_else(|| StoreData {
                version: schema_version(),
                proxies: BTreeMap::new(),
            });
        Self {
            data: Mutex::new(data),
            path,
            tls_backend,
        }
    }

    pub fn get(&self, proxy_url: &str) -> Option<ProxyReputation> {
        let key = normalize_proxy_key(Some(proxy_url));
        self.data.lock().proxies.get(&key).cloned()
    }

    pub fn all(&self) -> BTreeMap<String, ProxyReputation> {
        self.data.lock().proxies.clone()
    }

    /// 检测一个出口。走代理本身，顺带确认真实出口 IP。
    pub async fn check_one(&self, proxy_url: &str) -> ProxyReputation {
        let key = normalize_proxy_key(Some(proxy_url));
        let configured_host = key.split(':').next().unwrap_or_default().to_string();
        let mut rep = ProxyReputation {
            checked_at: Some(chrono::Utc::now().to_rfc3339()),
            ..Default::default()
        };

        let proxy = ProxyConfig::new(proxy_url);
        match self.lookup_through(&proxy).await {
            Ok(info) => {
                rep.exit_ip = info.query.clone();
                rep.exit_ip_mismatch = info
                    .query
                    .as_deref()
                    .is_some_and(|ip| !configured_host.is_empty() && ip != configured_host);
                rep.country = info.country;
                rep.region = info.region_name;
                rep.asn = info.r#as.filter(|s| !s.is_empty());
                rep.isp = info.isp.or(info.org).filter(|s| !s.is_empty());
                rep.hosting = info.hosting.unwrap_or(false);
                rep.proxy = info.proxy.unwrap_or(false);
                rep.mobile = info.mobile.unwrap_or(false);
            }
            Err(error) => rep.error = Some(error),
        }

        self.data.lock().proxies.insert(key, rep.clone());
        self.persist();
        rep
    }

    /// 批量检测。并发受 [`PROBE_CONCURRENCY`] 限制，避免把免费情报接口打限速。
    pub async fn check_many(&self, proxy_urls: Vec<String>) -> CheckSummary {
        let mut summary = CheckSummary::default();
        for chunk in proxy_urls.chunks(PROBE_CONCURRENCY) {
            let results =
                futures::future::join_all(chunk.iter().map(|url| self.check_one(url))).await;
            for rep in results {
                summary.checked += 1;
                if rep.exit_ip_mismatch {
                    summary.mismatched += 1;
                }
                match rep.grade() {
                    ReputationGrade::FlaggedProxy => summary.flagged_proxy += 1,
                    ReputationGrade::Hosting => summary.hosting += 1,
                    ReputationGrade::Clean => summary.clean += 1,
                    _ => summary.unreachable += 1,
                }
            }
        }
        summary
    }

    async fn lookup_through(&self, proxy: &ProxyConfig) -> Result<IpApiResponse, String> {
        let client = build_client(Some(proxy), PROBE_TIMEOUT_SECS, self.tls_backend)
            .map_err(|e| e.to_string())?;
        let response = tokio::time::timeout(
            Duration::from_secs(PROBE_TIMEOUT_SECS),
            client.get(LOOKUP_URL).send(),
        )
        .await
        .map_err(|_| "探测超时".to_string())?
        .map_err(|e| e.to_string())?;

        if !response.status().is_success() {
            return Err(format!("情报接口返回 HTTP {}", response.status().as_u16()));
        }
        let info: IpApiResponse = response.json().await.map_err(|e| e.to_string())?;
        if info.status.as_deref() != Some("success") {
            return Err(info
                .message
                .unwrap_or_else(|| "情报接口未返回 success".to_string()));
        }
        Ok(info)
    }

    /// 清掉某个出口的信誉记录（换了 IP 之后重新检测）
    pub fn forget(&self, proxy_url: &str) -> bool {
        let key = normalize_proxy_key(Some(proxy_url));
        let removed = self.data.lock().proxies.remove(&key).is_some();
        if removed {
            self.persist();
        }
        removed
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let json = {
            let data = self.data.lock();
            match serde_json::to_string_pretty(&*data) {
                Ok(j) => j,
                Err(error) => {
                    tracing::warn!(%error, "代理信誉档案序列化失败");
                    return;
                }
            }
        };
        if let Err(error) =
            atomicwrites::AtomicFile::new(path, atomicwrites::OverwriteBehavior::AllowOverwrite)
                .write(|f| std::io::Write::write_all(f, json.as_bytes()))
        {
            tracing::warn!(%error, path = %path.display(), "代理信誉档案落盘失败");
        }
    }
}

#[derive(Debug, Deserialize)]
struct IpApiResponse {
    status: Option<String>,
    message: Option<String>,
    query: Option<String>,
    country: Option<String>,
    #[serde(rename = "regionName")]
    region_name: Option<String>,
    isp: Option<String>,
    org: Option<String>,
    r#as: Option<String>,
    hosting: Option<bool>,
    proxy: Option<bool>,
    mobile: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(proxy: bool, hosting: bool, checked: bool, error: Option<&str>) -> ProxyReputation {
        ProxyReputation {
            proxy,
            hosting,
            checked_at: checked.then(|| "2026-08-17T00:00:00+00:00".to_string()),
            error: error.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn grade_puts_flagged_proxy_at_the_bottom() {
        // 「已被公开标记为代理」比「只是机房」严重得多：连免费库都认得出来，
        // 说明上游几乎必然也认得出来
        assert_eq!(
            rep(true, true, true, None).grade(),
            ReputationGrade::FlaggedProxy
        );
        // 机房但未被标记 —— 按线上数据这一档是可用的，不该被当成不可用
        assert_eq!(rep(false, true, true, None).grade(), ReputationGrade::Hosting);
        assert_eq!(rep(false, false, true, None).grade(), ReputationGrade::Clean);
    }

    #[test]
    fn unchecked_and_failed_are_distinguishable() {
        // 「没查过」不能和「查过但干净」混为一谈，否则新加的代理会被当成已验证
        assert_eq!(
            rep(false, false, false, None).grade(),
            ReputationGrade::Unknown
        );
        assert_eq!(
            rep(false, false, true, Some("探测超时")).grade(),
            ReputationGrade::Unreachable
        );
    }

    #[test]
    fn store_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("kiro-rep-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("proxy_reputation.json");
        let url = "socks5://user:pass@1.2.3.4:1080";
        {
            let store = ProxyReputationStore::new(Some(path.clone()), TlsBackend::Rustls);
            store
                .data
                .lock()
                .proxies
                .insert(normalize_proxy_key(Some(url)), rep(true, true, true, None));
            store.persist();
        }
        let reloaded = ProxyReputationStore::new(Some(path.clone()), TlsBackend::Rustls);
        let got = reloaded.get(url).expect("应能按 host:port 取回");
        assert!(got.proxy);
        assert_eq!(got.grade(), ReputationGrade::FlaggedProxy);
        // 落盘内容不得包含代理密码
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("pass"), "信誉档案不应写入代理密码: {raw}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auth_rotation_does_not_split_the_record() {
        // 与封号台账保持同一套归一化：机场轮换密码不该分裂成两条记录
        let store = ProxyReputationStore::new(None, TlsBackend::Rustls);
        store.data.lock().proxies.insert(
            normalize_proxy_key(Some("socks5://a:b@1.2.3.4:1080")),
            rep(false, true, true, None),
        );
        assert!(store.get("socks5://other:secret@1.2.3.4:1080").is_some());
        assert!(store.get("http://1.2.3.4:1080").is_some());
        // 端口不同是不同出口
        assert!(store.get("socks5://1.2.3.4:1081").is_none());
    }
}
