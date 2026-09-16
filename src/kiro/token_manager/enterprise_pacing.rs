//! 企业数据面的账号共享发送节奏；仅驻留内存，随 CredentialEntry 回收。
use super::MultiTokenManager;
use std::time::Duration;
use tokio::time::{Instant, sleep};

#[derive(Clone, Copy, Debug)]
pub(crate) struct EnterpriseSendStamp(u64);

#[derive(Debug)]
pub(crate) struct EnterpriseSendWaitTimeout {
    pub(crate) rate_limited: bool,
    required_wait: Duration,
    remaining: Duration,
}

impl std::fmt::Display for EnterpriseSendWaitTimeout {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "enterprise_send_wait_timeout: required_wait_ms={} remaining_ms={}",
            self.required_wait.as_millis(),
            self.remaining.as_millis()
        )
    }
}

impl std::error::Error for EnterpriseSendWaitTimeout {}

#[derive(Default)]
pub(super) struct EnterprisePacing {
    last_send: Option<Instant>,
    adaptive_started: Option<Instant>,
    adaptive_interval: Duration,
    consecutive_429s: u32,
    throttle_version: u64,
    adaptive_is_rate_limit: bool,
    // 保留起点+时长，不把可能达到 u64::MAX 秒的 Retry-After 加到 Instant。
    retry_after: Option<(Instant, Duration)>,
    retry_after_429_version: Option<u64>,
    retry_after_is_rate_limit: bool,
}

impl EnterprisePacing {
    /// 只记录等待原因，不改变等待时长。503 Retry-After 和基础RPM不能伪装成普通429。
    fn rate_limited_wait(&self, now: Instant, rpm_limit: u32) -> bool {
        let required = self.delay(now, rpm_limit);
        if required.is_zero() {
            return false;
        }
        let retry_after = self
            .retry_after
            .map_or(Duration::ZERO, |(received, delay)| {
                delay.saturating_sub(now.saturating_duration_since(received))
            });
        if retry_after == required {
            return self.retry_after_is_rate_limit;
        }
        let since_send = self.last_send.map_or(Duration::ZERO, |sent| {
            self.adaptive_interval
                .saturating_sub(now.saturating_duration_since(sent))
        });
        let since_429 = self.adaptive_started.map_or(Duration::ZERO, |received| {
            self.adaptive_interval
                .saturating_sub(now.saturating_duration_since(received))
        });
        self.adaptive_is_rate_limit && since_send.max(since_429) == required
    }

    fn classify_429(&mut self, stamp: EnterpriseSendStamp, ordinary_rate_limit: bool) {
        if stamp.0 == self.throttle_version {
            self.adaptive_is_rate_limit = ordinary_rate_limit;
        }
        if self.retry_after_429_version == Some(stamp.0) {
            self.retry_after_is_rate_limit = ordinary_rate_limit;
        }
    }

    fn delay(&self, now: Instant, rpm_limit: u32) -> Duration {
        let base_interval = if rpm_limit == 0 {
            Duration::ZERO
        } else {
            Duration::from_millis(60_000u64.div_ceil(u64::from(rpm_limit)))
        };
        let interval = base_interval.max(self.adaptive_interval);
        let since_send = self.last_send.map_or(Duration::ZERO, |sent| {
            interval.saturating_sub(now.saturating_duration_since(sent))
        });
        let since_429 = self.adaptive_started.map_or(Duration::ZERO, |received| {
            self.adaptive_interval
                .saturating_sub(now.saturating_duration_since(received))
        });
        let retry_after = self
            .retry_after
            .map_or(Duration::ZERO, |(received, delay)| {
                delay.saturating_sub(now.saturating_duration_since(received))
            });
        since_send.max(since_429).max(retry_after)
    }

    fn try_send(&mut self, now: Instant, rpm_limit: u32) -> Result<EnterpriseSendStamp, Duration> {
        let delay = self.delay(now, rpm_limit);
        if !delay.is_zero() {
            return Err(delay);
        }
        self.last_send = Some(now);
        Ok(EnterpriseSendStamp(self.throttle_version))
    }

    fn limited(
        &mut self,
        now: Instant,
        status: u16,
        retry_after: Option<Duration>,
        jitter_ms: u64,
    ) {
        if let Some(delay) = retry_after {
            let remaining = self
                .retry_after
                .map_or(Duration::ZERO, |(received, previous)| {
                    previous.saturating_sub(now.saturating_duration_since(received))
                });
            if delay > remaining {
                self.retry_after = Some((now, delay));
                self.retry_after_429_version =
                    (status == 429).then(|| self.throttle_version.wrapping_add(1));
                self.retry_after_is_rate_limit = false;
            }
        }
        if status == 429 {
            self.throttle_version = self.throttle_version.wrapping_add(1);
            self.consecutive_429s = self.consecutive_429s.saturating_add(1).min(6);
            let base_ms = (100u64 << (self.consecutive_429s - 1)).min(2_000);
            self.adaptive_interval = Duration::from_millis(base_ms + jitter_ms.min(base_ms / 8));
            self.adaptive_started = Some(now);
            self.adaptive_is_rate_limit = false;
        }
    }

    fn succeeded(&mut self, stamp: EnterpriseSendStamp) {
        // 旧在途请求的成功不能撤销其发送之后才收到的新 429。
        if stamp.0 == self.throttle_version {
            self.adaptive_started = None;
            self.adaptive_interval = Duration::ZERO;
            self.consecutive_429s = 0;
        }
    }
}

impl MultiTokenManager {
    /// 等待者不预约时隙；只有账号仍可用、节奏允许且取得请求预算时才登记发送。
    /// 等待期间保留调用方已有的 InFlightGuard，取消 future 即按原路径释放。
    pub(crate) async fn wait_enterprise_send(
        &self,
        id: u64,
        deadline: Instant,
        take_call: impl Fn() -> Option<usize>,
    ) -> anyhow::Result<Option<(usize, EnterpriseSendStamp)>> {
        let mut previous_wait = (Duration::ZERO, false);
        loop {
            let (delay, rpm_limit, rate_limited) = {
                let mut entries = self.entries.lock();
                let now = Instant::now();
                if now >= deadline {
                    return Err(EnterpriseSendWaitTimeout {
                        rate_limited: previous_wait.1,
                        required_wait: previous_wait.0,
                        remaining: Duration::ZERO,
                    }
                    .into());
                }
                let entry = entries
                    .iter_mut()
                    .find(|entry| entry.id == id)
                    .ok_or_else(|| anyhow::anyhow!("凭据不存在: {id}"))?;
                Self::validate_retry_entry(entry)?;
                let delay = entry
                    .enterprise_pacing
                    .delay(now, entry.credentials.rpm_limit);
                if delay.is_zero() {
                    let Some(sequence) = take_call() else {
                        return Ok(None);
                    };
                    let stamp = entry
                        .enterprise_pacing
                        .try_send(now, entry.credentials.rpm_limit)
                        .expect("同一锁内已验证发送节奏");
                    return Ok(Some((sequence, stamp)));
                }
                (
                    delay,
                    entry.credentials.rpm_limit,
                    entry
                        .enterprise_pacing
                        .rate_limited_wait(now, entry.credentials.rpm_limit),
                )
            };
            previous_wait = (delay, rate_limited);
            // 超大 Retry-After 直接耗尽企业阶段，留出既有个人池预算，且不构造溢出时刻。
            let remaining = deadline.saturating_duration_since(Instant::now());
            tracing::debug!(
                credential_id = id,
                wait_ms = %delay.as_millis(),
                remaining_ms = %remaining.as_millis(),
                rpm_limit,
                "企业号等待共享发送时隙"
            );
            if delay >= remaining {
                return Err(EnterpriseSendWaitTimeout {
                    rate_limited,
                    required_wait: delay,
                    remaining,
                }
                .into());
            }
            sleep(delay).await;
            // 睡眠期间其他响应可能延长退避；醒后重新检查，而非直接发送。
        }
    }

    pub(crate) fn report_enterprise_response_headers(
        &self,
        id: u64,
        status: u16,
        retry_after: Option<Duration>,
    ) -> Option<EnterpriseSendStamp> {
        if !matches!(status, 429 | 503) {
            return None;
        }
        if let Some(entry) = self.entries.lock().iter_mut().find(|entry| entry.id == id) {
            entry.enterprise_pacing.limited(
                Instant::now(),
                status,
                retry_after,
                fastrand::u64(0..=250),
            );
            return (status == 429).then_some(EnterpriseSendStamp(
                entry.enterprise_pacing.throttle_version,
            ));
        }
        None
    }

    /// 正文分类完成后才确认普通429；过时响应不能改写较新的等待原因。
    pub(crate) fn classify_enterprise_429(
        &self,
        id: u64,
        stamp: EnterpriseSendStamp,
        ordinary_rate_limit: bool,
    ) {
        if let Some(entry) = self.entries.lock().iter_mut().find(|entry| entry.id == id) {
            entry
                .enterprise_pacing
                .classify_429(stamp, ordinary_rate_limit);
        }
    }

    pub(crate) fn report_enterprise_effective_response(&self, id: u64, stamp: EnterpriseSendStamp) {
        if let Some(entry) = self.entries.lock().iter_mut().find(|entry| entry.id == id) {
            entry.enterprise_pacing.succeeded(stamp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enterprise_rate_limit_error_metadata_requires_classified_429_and_keeps_same_delay() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.limited(now, 429, Some(Duration::from_secs(1)), 0);
        let delay = pacing.delay(now, 0);
        assert!(
            !pacing.rate_limited_wait(now, 0),
            "headers alone cannot distinguish quota or auth429"
        );
        pacing.classify_429(EnterpriseSendStamp(1), true);
        assert!(pacing.rate_limited_wait(now, 0));
        assert_eq!(pacing.delay(now, 0), delay);
        pacing.classify_429(EnterpriseSendStamp(1), false);
        assert!(!pacing.rate_limited_wait(now, 0));
        assert_eq!(
            pacing.delay(now, 0),
            delay,
            "classification must not change pacing"
        );
    }

    #[test]
    fn enterprise_rate_limit_error_metadata_ignores_stale_response_classification() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.limited(now, 429, Some(Duration::from_secs(1)), 0);
        pacing.limited(now, 429, Some(Duration::from_secs(2)), 0);
        pacing.classify_429(EnterpriseSendStamp(2), true);
        pacing.classify_429(EnterpriseSendStamp(1), false);
        assert!(pacing.rate_limited_wait(now, 0));
        assert_eq!(pacing.delay(now, 0), Duration::from_secs(2));
    }

    #[test]
    fn enterprise_rate_limit_error_metadata_uses_the_binding_retry_after_cause() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.limited(now, 429, Some(Duration::from_secs(2)), 0);
        pacing.classify_429(EnterpriseSendStamp(1), true);
        pacing.limited(now, 503, Some(Duration::from_secs(1)), 0);
        assert!(
            pacing.rate_limited_wait(now, 0),
            "shorter503 cannot replace existing429 wait"
        );
        pacing.limited(now, 503, Some(Duration::from_secs(3)), 0);
        assert!(
            !pacing.rate_limited_wait(now, 0),
            "binding503 Retry-After is not429"
        );
        pacing.limited(now, 429, None, 0);
        pacing.classify_429(EnterpriseSendStamp(2), true);
        assert!(
            !pacing.rate_limited_wait(now, 0),
            "shorter adaptive429 cannot override503 Retry-After"
        );
        assert_eq!(pacing.delay(now, 0), Duration::from_secs(3));
    }

    #[test]
    fn enterprise_rate_limit_error_metadata_does_not_reclassify_base_rpm() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.try_send(now, 100).unwrap();
        pacing.limited(now, 429, None, 0);
        pacing.classify_429(EnterpriseSendStamp(1), true);
        assert!(pacing.rate_limited_wait(now, 0));
        assert!(!pacing.rate_limited_wait(now, 100));
        assert_eq!(pacing.delay(now, 100), Duration::from_millis(600));
    }

    #[test]
    fn rpm_interval_is_rounded_up_and_zero_is_unlimited() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        assert!(pacing.try_send(now, 0).is_ok());
        assert!(pacing.try_send(now, 0).is_ok());
        assert_eq!(pacing.delay(now, 7), Duration::from_millis(8_572));
        assert_eq!(pacing.delay(now, u32::MAX), Duration::from_millis(1));
    }

    #[test]
    fn competing_waiters_do_not_reserve_future_slots() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.try_send(now, 100).unwrap();
        for _ in 0..20 {
            assert_eq!(
                pacing.try_send(now, 100).unwrap_err(),
                Duration::from_millis(600)
            );
        }
        let next = now + Duration::from_millis(600);
        assert!(pacing.try_send(next, 100).is_ok());
        assert_eq!(
            pacing.try_send(next, 100).unwrap_err(),
            Duration::from_millis(600)
        );
    }

    #[test]
    fn throttle_backoff_grows_caps_and_paces_recovering_sends() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        for expected in [100, 200, 400, 800, 1_600, 2_000, 2_000] {
            pacing.limited(now, 429, None, 0);
            assert_eq!(pacing.delay(now, 0), Duration::from_millis(expected));
        }
        let later = now + Duration::from_secs(2);
        pacing.try_send(later, 0).unwrap();
        assert_eq!(
            pacing.delay(later, 0),
            Duration::from_secs(2),
            "并发等待者不能在同一退避结束时一起发送"
        );
        pacing.limited(later, 429, None, u64::MAX);
        assert_eq!(pacing.delay(later, 0), Duration::from_millis(2_250));
    }

    #[test]
    fn current_success_restores_only_base_interval() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.limited(now, 429, None, 0);
        let later = now + Duration::from_millis(100);
        let stamp = pacing.try_send(later, 600).unwrap();
        pacing.succeeded(stamp);
        assert_eq!(pacing.delay(later, 600), Duration::from_millis(100));
        assert_eq!(pacing.delay(later, 0), Duration::ZERO);
        pacing.limited(later, 429, None, 0);
        assert_eq!(pacing.delay(later, 0), Duration::from_millis(100));
    }

    #[test]
    fn stale_success_cannot_clear_a_newer_429() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        let old = pacing.try_send(now, 0).unwrap();
        pacing.limited(now, 429, None, 0);
        pacing.succeeded(old);
        assert_eq!(pacing.delay(now, 0), Duration::from_millis(100));
    }

    #[test]
    fn success_and_shorter_retry_after_cannot_shorten_upstream_wait() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        let sent = pacing.try_send(now, 0).unwrap();
        pacing.limited(now, 503, Some(Duration::from_secs(60)), 0);
        pacing.succeeded(sent);
        let later = now + Duration::from_secs(10);
        pacing.limited(later, 429, Some(Duration::from_secs(1)), 0);
        assert_eq!(pacing.delay(later, 0), Duration::from_secs(50));
        pacing.limited(later, 503, Some(Duration::from_secs(70)), 0);
        assert_eq!(pacing.delay(later, 0), Duration::from_secs(70));
    }

    #[test]
    fn enormous_retry_after_never_adds_to_instant_or_overflows() {
        let now = Instant::now();
        let mut pacing = EnterprisePacing::default();
        pacing.limited(now, 429, Some(Duration::from_secs(u64::MAX)), 0);
        assert_eq!(
            pacing.delay(now + Duration::from_secs(1), 0),
            Duration::from_secs(u64::MAX - 1)
        );
    }

    #[test]
    fn independent_credentials_do_not_share_pacing() {
        let now = Instant::now();
        let mut first = EnterprisePacing::default();
        let mut second = EnterprisePacing::default();
        first.limited(now, 429, Some(Duration::from_secs(60)), 0);
        assert!(second.try_send(now, 100).is_ok());
        assert_eq!(first.delay(now, 100), Duration::from_secs(60));
    }
}
