//! 客户端环境身份：对上游声明「这台机器是什么」。
//!
//! 拼进两个地方：User-Agent 的 `os/` 与 `md/nodejs#` 段，以及请求体
//! `envState`（`operatingSystem` / `currentWorkingDirectory`）。
//!
//! # 为什么要按号分散
//!
//! 2026-08-31 12:19–12:27 的 8 分钟里，20 个号跨约 18 个不同出口被上游逐个判死，
//! 文案统一是 `unusual user activity`。`machineId` 当时已经是一号一个唯一值，
//! 出口也各不相同——真正全池一致的是**客户端外壳**：每个请求都自称同一台
//! 「装着 Kiro 的 macOS、Node 22.22.0」。一个号被标记后，靠这些共用值就能把
//! 整池枚举出来。
//!
//! # 为什么**不能**每请求随机
//!
//! 同一个号今天报 win32、明天报 darwin，是「一个账号在多台机器之间跳」，比全池
//! 固定一套更容易被认出来。所以这里的做法是：首次加载时按号从 [`PROFILES`] 里挑
//! 一套写进凭据并落盘，之后永不变化——与 `machine_id` 的回填落盘同一套路
//! （见 [`crate::kiro::machine_id`] 与 `MultiTokenManager::new` 里的补全逻辑）。
//!
//! # 三处必须自洽
//!
//! UA 的 `os/` 段、`envState.operatingSystem`、`envState.currentWorkingDirectory`
//! 的路径风格，描述的是同一台假想机器。任一项和其它两项矛盾（例如 UA 说
//! `win32` 而 envState 说 `macos`）比全池共用一套更显眼，所以后两项一律由
//! `system_version` 派生，不单独存储、也无法被配置成不一致的组合。

use sha2::{Digest, Sha256};

use crate::kiro::model::credentials::KiroCredentials;
use crate::model::config::Config;

/// 一套真实存在的 OS / Node 搭配。
///
/// 两个字段必须成组取用：`win32` 配 macOS 上常见的 Node 版本这种交叉拼装，
/// 本身就是一个异常特征。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientProfile {
    /// UA 的 `os/` 段，形如 `win32#10.0.26200`
    pub system_version: &'static str,
    /// UA 的 `md/nodejs#` 段
    pub node_version: &'static str,
}

/// 可分配的环境组合白名单。
///
/// 取值参照官方 Kiro IDE 1.0.395 抓包基线：`os/` 段是 `<平台>#<内核或系统版本>`，
/// 不是 `macos` 这类裸平台名——后者在真实客户端里不存在，一旦出现就是全网唯一
/// 的静态标记（生产曾长期发送 `os/macos`）。
pub const PROFILES: &[ClientProfile] = &[
    ClientProfile {
        system_version: "win32#10.0.26200",
        node_version: "22.22.0",
    },
    ClientProfile {
        system_version: "win32#10.0.22631",
        node_version: "20.18.1",
    },
    ClientProfile {
        system_version: "darwin#24.5.0",
        node_version: "22.22.0",
    },
    ClientProfile {
        system_version: "darwin#23.6.0",
        node_version: "20.18.1",
    },
    ClientProfile {
        system_version: "linux#6.8.0",
        node_version: "22.22.0",
    },
];

/// 派生工作目录时可用的用户名片段。
const HOME_NAMES: &[&str] = &[
    "alex", "chris", "daniel", "emma", "jordan", "kevin", "laura", "marco", "nina", "ryan", "sam",
    "tina",
];

/// 派生工作目录时可用的项目名片段。
const PROJECT_NAMES: &[&str] = &[
    "api-gateway",
    "app",
    "backend",
    "dashboard",
    "data-pipeline",
    "playground",
    "portal",
    "sandbox",
    "service",
    "web",
    "workspace",
];

/// 本次请求对外声明的环境。借用而非拷贝：UA 拼接本身就要分配，这里不再叠一层。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientIdentity<'a> {
    pub system_version: &'a str,
    pub node_version: &'a str,
}

/// 解析该凭据应当声明的环境。
///
/// 优先级：凭据级 > 全局 `config`。凭据级值由 [`assign_missing`] 在首次加载时
/// 回填并落盘，所以正常情况下命中第一条；全局 config 只作回落，以及运营想
/// 强制全池统一时的逃生口。
pub fn resolve<'a>(credentials: &'a KiroCredentials, config: &'a Config) -> ClientIdentity<'a> {
    ClientIdentity {
        system_version: pick(
            credentials.system_version.as_deref(),
            &config.system_version,
        ),
        node_version: pick(credentials.node_version.as_deref(), &config.node_version),
    }
}

fn pick<'a>(own: Option<&'a str>, global: &'a str) -> &'a str {
    own.map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(global)
}

/// 给还没有环境字段的凭据补一套，返回是否发生了改动（调用方据此决定要不要落盘）。
///
/// 两个字段成组补：只补其中一个会拼出白名单里不存在的组合。
pub fn assign_missing(credentials: &mut KiroCredentials) -> bool {
    let has_system = credentials
        .system_version
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    let has_node = credentials
        .node_version
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    if has_system && has_node {
        return false;
    }

    let profile = profile_for_seed(&seed_of(credentials));
    credentials.system_version = Some(profile.system_version.to_string());
    credentials.node_version = Some(profile.node_version.to_string());
    true
}

/// 挑选用的稳定种子。
///
/// 优先 `machine_id`：它在首次加载时就已落盘，**不随 refreshToken 轮换变化**，
/// 因此同一个号永远拿到同一套环境。直接拿 refreshToken 派生会在每次刷新后换一套，
/// 正是本模块要避免的「号在多台机器间跳」。
fn seed_of(credentials: &KiroCredentials) -> String {
    if let Some(machine_id) = credentials
        .machine_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return machine_id.to_string();
    }
    match credentials.id {
        Some(id) => format!("credential/{id}"),
        None => "credential/unknown".to_string(),
    }
}

/// 按种子确定性地取一套环境组合。
pub fn profile_for_seed(seed: &str) -> &'static ClientProfile {
    let index = hash_index(seed, "profile", PROFILES.len());
    &PROFILES[index]
}

/// 从 UA 的 `os/` 段推出请求体 `envState.operatingSystem` 用的短名。
///
/// 两处词表不同：UA 用 `win32` / `darwin` / `linux` + 版本号，envState 用平台短名。
/// 由同一个 `system_version` 派生，保证不会出现「UA 说 win32、envState 说 macos」。
pub fn env_operating_system(system_version: &str) -> &'static str {
    let family = system_version
        .split('#')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    match family.as_str() {
        "win32" | "windows" | "win" => "windows",
        "linux" => "linux",
        _ => "macos",
    }
}

/// 按号派生一个与 OS 风格一致的工作目录。
///
/// 替代 `std::env::current_dir()`：那个值把**容器的工作目录**发给上游，且全池
/// 完全相同——既暴露「这不是某个开发者的项目目录」，又白送一个跨号关联键。
pub fn working_directory(system_version: &str, seed: &str) -> String {
    let user = HOME_NAMES[hash_index(seed, "home", HOME_NAMES.len())];
    let project = PROJECT_NAMES[hash_index(seed, "project", PROJECT_NAMES.len())];
    match env_operating_system(system_version) {
        "windows" => format!("C:\\Users\\{user}\\source\\repos\\{project}"),
        "linux" => format!("/home/{user}/projects/{project}"),
        _ => format!("/Users/{user}/Projects/{project}"),
    }
}

/// 请求体 `envState` 要声明的值。
///
/// 与 UA 的 `os/` 段同源（都由 `system_version` 派生），因此两处不可能互相矛盾。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvState {
    pub operating_system: &'static str,
    pub working_directory: String,
}

/// 该凭据应当声明的 `envState`。
pub fn env_state_for(credentials: &KiroCredentials, config: &Config) -> EnvState {
    let identity = resolve(credentials, config);
    let seed = seed_of(credentials);
    EnvState {
        operating_system: env_operating_system(identity.system_version),
        working_directory: working_directory(identity.system_version, &seed),
    }
}

/// 就地改写 JSON 里所有 `envState`，返回是否改动过。
///
/// 递归遍历而不是按固定路径定位：`envState` 同时出现在 `currentMessage` 和
/// `history` 里每条带工具结果的用户消息上（见
/// [`crate::kiro::model::requests::conversation::UserInputMessageContext`]），
/// 漏掉任何一处都会在**同一个请求内**出现两个不同的操作系统。
///
/// 为什么不在转换阶段就写对：转换发生在选号之前，那时还不知道会用哪个凭据。
pub fn patch_env_state(json: &mut serde_json::Value, env: &EnvState) -> bool {
    use serde_json::Value;

    match json {
        Value::Object(map) => {
            let mut changed = false;
            if let Some(Value::Object(state)) = map.get_mut("envState") {
                state.insert(
                    "operatingSystem".to_string(),
                    Value::String(env.operating_system.to_string()),
                );
                state.insert(
                    "currentWorkingDirectory".to_string(),
                    Value::String(env.working_directory.clone()),
                );
                changed = true;
            }
            for value in map.values_mut() {
                changed |= patch_env_state(value, env);
            }
            changed
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= patch_env_state(item, env);
            }
            changed
        }
        _ => false,
    }
}

/// `sha256(domain/seed)` 的前 8 字节取模。加 domain 前缀，避免同一个种子在
/// 不同用途上取到相关联的下标（例如用户名和项目名总是同步变化）。
fn hash_index(seed: &str, domain: &str, len: usize) -> usize {
    if len == 0 {
        return 0;
    }
    let digest = Sha256::digest(format!("{domain}/{seed}").as_bytes());
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest[..8]);
    (u64::from_be_bytes(bytes) % len as u64) as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(machine_id: Option<&str>) -> KiroCredentials {
        KiroCredentials {
            id: Some(7),
            machine_id: machine_id.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn whitelist_entries_are_well_formed() {
        assert!(!PROFILES.is_empty());
        for profile in PROFILES {
            assert!(
                matches!(
                    env_operating_system(profile.system_version),
                    "windows" | "linux" | "macos"
                ),
                "无法识别的平台: {}",
                profile.system_version
            );
            assert!(
                profile.node_version.split('.').count() == 3,
                "Node 版本应形如 22.22.0，实际: {}",
                profile.node_version
            );
        }
    }

    #[test]
    fn no_profile_uses_bare_platform_name() {
        // 回归守卫：生产曾长期发送 `os/macos`，真实客户端不存在这种裸平台名。
        for profile in PROFILES {
            assert!(
                profile.system_version.contains('#'),
                "`os/` 段必须是 <平台>#<版本>，实际: {}",
                profile.system_version
            );
        }
    }

    #[test]
    fn assignment_is_stable_for_same_credential() {
        let mut first = cred(Some("a".repeat(64).as_str()));
        let mut second = cred(Some("a".repeat(64).as_str()));
        assert!(assign_missing(&mut first));
        assert!(assign_missing(&mut second));
        assert_eq!(first.system_version, second.system_version);
        assert_eq!(first.node_version, second.node_version);
    }

    #[test]
    fn assignment_differs_across_credentials() {
        // 不要求两两不同（白名单只有 5 组），但一批号不能全落到同一组。
        let mut seen = std::collections::HashSet::new();
        for index in 0..40u64 {
            let mut creds = cred(Some(&format!("{:064x}", index)));
            assign_missing(&mut creds);
            seen.insert(creds.system_version.clone().unwrap());
        }
        assert!(
            seen.len() >= 3,
            "40 个号只落到 {} 组环境，分散度不足",
            seen.len()
        );
    }

    #[test]
    fn assignment_is_idempotent_and_does_not_overwrite() {
        let mut creds = cred(Some("b".repeat(64).as_str()));
        creds.system_version = Some("win32#10.0.19045".to_string());
        creds.node_version = Some("18.20.4".to_string());
        assert!(!assign_missing(&mut creds));
        assert_eq!(creds.system_version.as_deref(), Some("win32#10.0.19045"));
        assert_eq!(creds.node_version.as_deref(), Some("18.20.4"));
    }

    #[test]
    fn half_filled_credential_gets_a_whole_pair() {
        // 只填了一半时必须整组重取：否则会拼出白名单里不存在的组合。
        let mut creds = cred(Some("c".repeat(64).as_str()));
        creds.system_version = Some("darwin#24.5.0".to_string());
        creds.node_version = None;
        assert!(assign_missing(&mut creds));
        let pair = PROFILES.iter().any(|p| {
            Some(p.system_version) == creds.system_version.as_deref()
                && Some(p.node_version) == creds.node_version.as_deref()
        });
        assert!(pair, "补全后的组合必须来自白名单");
    }

    #[test]
    fn seed_falls_back_to_id_when_machine_id_missing() {
        let mut creds = cred(None);
        assert!(assign_missing(&mut creds));
        assert!(creds.system_version.is_some());
    }

    #[test]
    fn resolve_prefers_credential_over_config() {
        let mut config = Config::default();
        config.system_version = "linux#6.8.0".to_string();
        config.node_version = "22.22.0".to_string();

        let mut creds = cred(Some("d".repeat(64).as_str()));
        creds.system_version = Some("win32#10.0.26200".to_string());
        creds.node_version = Some("20.18.1".to_string());

        let identity = resolve(&creds, &config);
        assert_eq!(identity.system_version, "win32#10.0.26200");
        assert_eq!(identity.node_version, "20.18.1");
    }

    #[test]
    fn resolve_falls_back_to_config_when_credential_blank() {
        let mut config = Config::default();
        config.system_version = "darwin#23.6.0".to_string();
        config.node_version = "20.18.1".to_string();

        let mut creds = cred(Some("e".repeat(64).as_str()));
        creds.system_version = Some("   ".to_string());
        creds.node_version = None;

        let identity = resolve(&creds, &config);
        assert_eq!(identity.system_version, "darwin#23.6.0");
        assert_eq!(identity.node_version, "20.18.1");
    }

    #[test]
    fn env_operating_system_matches_ua_family() {
        assert_eq!(env_operating_system("win32#10.0.26200"), "windows");
        assert_eq!(env_operating_system("darwin#24.5.0"), "macos");
        assert_eq!(env_operating_system("linux#6.8.0"), "linux");
        // 未知/畸形值回落 macos，与历史行为一致，不至于发出空字符串
        assert_eq!(env_operating_system(""), "macos");
        assert_eq!(env_operating_system("macos"), "macos");
    }

    #[test]
    fn working_directory_matches_os_style() {
        let win = working_directory("win32#10.0.26200", "seed-1");
        assert!(win.starts_with("C:\\Users\\"), "实际: {win}");
        assert!(win.contains("\\source\\repos\\"));

        let mac = working_directory("darwin#24.5.0", "seed-1");
        assert!(mac.starts_with("/Users/"), "实际: {mac}");

        let linux = working_directory("linux#6.8.0", "seed-1");
        assert!(linux.starts_with("/home/"), "实际: {linux}");
    }

    #[test]
    fn working_directory_is_stable_and_varies_by_seed() {
        assert_eq!(
            working_directory("win32#10.0.26200", "seed-a"),
            working_directory("win32#10.0.26200", "seed-a")
        );
        let mut seen = std::collections::HashSet::new();
        for index in 0..40u64 {
            seen.insert(working_directory("linux#6.8.0", &format!("seed-{index}")));
        }
        assert!(seen.len() >= 10, "工作目录分散度不足: {}", seen.len());
    }

    #[test]
    fn working_directory_never_leaks_process_cwd() {
        // 回归守卫：旧实现用 std::env::current_dir()，把容器工作目录发给上游，
        // 且全池完全相同。
        let actual = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if actual.is_empty() {
            return;
        }
        for seed in ["s1", "s2", "s3"] {
            assert_ne!(working_directory("linux#6.8.0", seed), actual);
        }
    }

    #[test]
    fn hash_index_domains_are_independent() {
        // 同一种子在不同用途上不应总是取到同一下标，否则用户名与项目名会同步变化。
        let mut same = 0;
        for index in 0..50u64 {
            let seed = format!("seed-{index}");
            if hash_index(&seed, "home", 12) == hash_index(&seed, "project", 12) {
                same += 1;
            }
        }
        assert!(same < 30, "两个用途的下标高度相关: {same}/50");
    }
}
