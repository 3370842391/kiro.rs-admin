//! 批发号池交付系统（wholesale）
//!
//! 号商场景：客户注册拿 uid + wsk_ key，设定常驻号池目标数 `target`，
//! 每次调 `/wholesale/sync` 由服务端把名下正常号补齐到 target；号死了下次自动补，
//! 余额不足则停、CDK 充值后继续。交付物是 `ksk_`（Kiro 门户原生 key，直连 Kiro）。
//!
//! 封号靠服务端后台探活（`ListAvailableModels` 返回 403 即死号），命中质保期退余额。
//!
//! 方案见 `docs/TASK_号池监控与CDK交付.md`。
//!
//! 子模块：
//! - `health`：封号 / 额度 分类器（P0，纯函数 + 单测）
//! - `store`：SQLite 数据层（P1，mothers/customers/holdings/cdks/wallet）
//! - `probe`：后台探活轮询 + 母号死亡联动 + 5 分钟清理（P2）
//! - `service`：号池同步 / 质保 / CDK 业务逻辑（P3/P4）
//! - `router`：客户 + 管理端 HTTP 接口（P5/P6）

pub mod health;
pub mod router;
pub mod service;
pub mod store;

pub use health::{classify_account_health, AccountHealth};
pub use router::{create_wholesale_router, WholesaleState};
pub use service::{WholesaleConfig, WholesaleService};
pub use store::{SharedWholesaleStore, WholesaleStore};
