//! 按账号推算上游能扛多少 RPM。
//!
//! 运营把 `rpmLimit` 一律设成 30 时，Kiro 的 `USER_REQUEST_RATE_EXCEEDED` 会把
//! 重试打成雪崩：同一分钟既有成功又有大量 429。这里只做一件事——根据最近完整
//! 分钟的成功条数和 429 条数，给出「这个号大概能撑多少」。**不改** `rpmLimit`。
//!
//! 口径：
//! - 见过 429 的分钟：账号已经顶到天花板。取这些分钟成功数的中位数；
//!   429 很密时再减 1，避免卡在刚好爆的边上。
//! - 没见过 429：只知道下限，取这些分钟成功数的最大值。
//! - 没有流量：不给数字，避免把「没跑」说成「只能 0」。

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 回看多少个已经走完的分钟。太短会抖，太长会跟不上号被打残。
pub const WINDOW_MINUTES: i64 = 15;

/// 单账号、单分钟的成功 / 429 计数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RpmMinuteBucket {
    pub credential_id: u64,
    pub minute_epoch: i64,
    pub successes: u32,
    pub rate_limited: u32,
}

/// 推算是天花板还是地板。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RpmInferenceKind {
    /// 窗口里见过 429，数字是「别再往上加」。
    Ceiling,
    /// 窗口里没 429，数字是「至少能到这」，还能试着加。
    Floor,
}

/// 单个账号的推算结果，给凭据列表展示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RpmInference {
    pub suggested: u32,
    pub kind: RpmInferenceKind,
    /// 窗口内每分钟成功数的中位数（有 429）或最大值（无 429）。
    pub success_rpm: u32,
    /// 窗口内每分钟 429 次数的平均值，四舍五入。
    pub rate429_rpm: u32,
    pub sample_minutes: u32,
    pub measured_at: String,
}

/// 进程内推算缓存。每分钟整表替换，读的时候按 id 取。
#[derive(Default)]
pub struct RpmInferenceStore {
    by_id: Mutex<HashMap<u64, RpmInference>>,
}

impl RpmInferenceStore {
    pub fn get(&self, id: u64) -> Option<RpmInference> {
        self.by_id.lock().get(&id).cloned()
    }

    pub fn replace(&self, next: HashMap<u64, RpmInference>) {
        *self.by_id.lock() = next;
    }
}

/// 把按分钟的桶收成每个账号一条推算。
pub fn infer_all(
    buckets: &[RpmMinuteBucket],
    measured_at: DateTime<Utc>,
) -> HashMap<u64, RpmInference> {
    let mut grouped: HashMap<u64, Vec<(u32, u32)>> = HashMap::new();
    for bucket in buckets {
        if bucket.successes == 0 && bucket.rate_limited == 0 {
            continue;
        }
        grouped
            .entry(bucket.credential_id)
            .or_default()
            .push((bucket.successes, bucket.rate_limited));
    }

    let measured_at = measured_at.to_rfc3339();
    grouped
        .into_iter()
        .filter_map(|(id, minutes)| {
            infer_one(&minutes).map(|(suggested, kind, success_rpm, rate429_rpm)| {
                (
                    id,
                    RpmInference {
                        suggested,
                        kind,
                        success_rpm,
                        rate429_rpm,
                        sample_minutes: minutes.len() as u32,
                        measured_at: measured_at.clone(),
                    },
                )
            })
        })
        .collect()
}

fn infer_one(minutes: &[(u32, u32)]) -> Option<(u32, RpmInferenceKind, u32, u32)> {
    if minutes.is_empty() {
        return None;
    }
    let dirty: Vec<u32> = minutes
        .iter()
        .filter(|(_, limited)| *limited > 0)
        .map(|(ok, _)| *ok)
        .collect();
    let clean: Vec<u32> = minutes
        .iter()
        .filter(|(_, limited)| *limited == 0)
        .map(|(ok, _)| *ok)
        .collect();

    let total_ok: u32 = minutes.iter().map(|(ok, _)| *ok).sum();
    let total_429: u32 = minutes.iter().map(|(_, limited)| *limited).sum();
    let rate429_rpm = ((total_429 as u64 + minutes.len() as u64 / 2) / minutes.len() as u64) as u32;

    if !dirty.is_empty() {
        let success_rpm = median_u32(&dirty);
        let heavy = total_429 * 4 >= total_ok.max(1) || rate429_rpm >= 3;
        let suggested = if heavy {
            success_rpm.saturating_sub(1).max(1)
        } else {
            success_rpm.max(1)
        };
        Some((suggested, RpmInferenceKind::Ceiling, success_rpm, rate429_rpm))
    } else if !clean.is_empty() {
        let success_rpm = *clean.iter().max().unwrap_or(&0);
        if success_rpm == 0 {
            return None;
        }
        Some((success_rpm, RpmInferenceKind::Floor, success_rpm, 0))
    } else {
        None
    }
}

fn median_u32(values: &[u32]) -> u32 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 22, 6, 0, 0).unwrap()
    }

    fn buckets(id: u64, minutes: &[(u32, u32)]) -> Vec<RpmMinuteBucket> {
        minutes
            .iter()
            .enumerate()
            .map(|(i, (ok, limited))| RpmMinuteBucket {
                credential_id: id,
                minute_epoch: 1_787_000_000 + i as i64 * 60,
                successes: *ok,
                rate_limited: *limited,
            })
            .collect()
    }

    #[test]
    fn saturated_heavy_429_shaves_one_off_median() {
        // 线上 2411 近 15 分钟：每分钟都有 429，成功中位数 12，429 比成功还密。
        let minutes: Vec<(u32, u32)> = (0..14).map(|_| (12, 13)).collect();
        let out = infer_all(&buckets(2411, &minutes), at());
        let got = out.get(&2411).expect("should infer");
        assert_eq!(got.suggested, 11);
        assert_eq!(got.kind, RpmInferenceKind::Ceiling);
        assert_eq!(got.success_rpm, 12);
        assert_eq!(got.sample_minutes, 14);
    }

    #[test]
    fn light_429_keeps_median_as_ceiling() {
        let minutes = [(5, 1), (5, 1), (6, 0), (5, 1)];
        let out = infer_all(&buckets(2408, &minutes), at());
        let got = out.get(&2408).unwrap();
        assert_eq!(got.suggested, 5);
        assert_eq!(got.kind, RpmInferenceKind::Ceiling);
        assert_eq!(got.success_rpm, 5);
    }

    #[test]
    fn clean_window_is_a_floor_not_a_cap() {
        let minutes = [(4, 0), (7, 0), (6, 0)];
        let out = infer_all(&buckets(1, &minutes), at());
        let got = out.get(&1).unwrap();
        assert_eq!(got.suggested, 7);
        assert_eq!(got.kind, RpmInferenceKind::Floor);
        assert_eq!(got.rate429_rpm, 0);
    }

    #[test]
    fn idle_account_is_omitted() {
        let out = infer_all(&[], at());
        assert!(out.is_empty());
        let empty = [RpmMinuteBucket {
            credential_id: 9,
            minute_epoch: 0,
            successes: 0,
            rate_limited: 0,
        }];
        assert!(infer_all(&empty, at()).is_empty());
    }

    #[test]
    fn store_replaces_snapshot() {
        let store = RpmInferenceStore::default();
        assert!(store.get(1).is_none());
        let inferred = infer_all(&buckets(1, &[(8, 4), (9, 5)]), at());
        store.replace(inferred);
        assert_eq!(store.get(1).unwrap().suggested, 8);
        store.replace(HashMap::new());
        assert!(store.get(1).is_none());
    }
}
