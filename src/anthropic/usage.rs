use super::cache_metering::CacheUsage;
use crate::admin::client_keys::CacheBillingMode;

#[derive(Debug, Clone, Copy)]
pub(crate) struct InputTokenUsage {
    client_visible_tokens: i32,
    upstream_context_tokens: Option<i32>,
}

impl InputTokenUsage {
    pub(crate) fn new(client_visible_tokens: i32) -> Self {
        Self {
            client_visible_tokens: client_visible_tokens.max(0),
            upstream_context_tokens: None,
        }
    }

    pub(crate) fn observe_upstream_context(&mut self, tokens: i32) {
        self.upstream_context_tokens = Some(tokens.max(0));
    }

    pub(crate) fn client_visible_tokens(&self) -> i32 {
        self.client_visible_tokens
    }

    pub(crate) fn upstream_context_tokens(&self) -> Option<i32> {
        self.upstream_context_tokens
    }

    /// 上游 `contextUsageEvent` 的真实上下文占用优先，拿不到才退回客户端侧估算。
    ///
    /// 客户端（Claude Code 等）按累计 usage 占模型窗口的比例决定何时自动 compact。
    /// 只回报 `client_visible_tokens`（本轮客户端自己发了多少）会让它完全看不到上游
    /// 已堆积的历史：缓存命中率越高，被算进缓存的部分越多，回报值就越小。实测上游
    /// 报 5417 时客户端只收到 72，于是它以为窗口才用了几个百分点，压缩永不触发，
    /// 一路撞到上游请求体字节上限的 400 才停。
    pub(crate) fn split_api(&self, cache: &CacheUsage) -> (i32, i32, i32) {
        let total = self.report_total();
        // 口径随 CacheUsage 一路传下来（与 hit_rate_* 同处），不再单独接一条线。
        match cache.billing_mode {
            // 优质客户：互斥分摊，三者之和 == total。
            CacheBillingMode::Exclusive => cache.split_against_total(total),
            // 普通客户：同行口径，覆盖前缀重复计入 creation，三者之和 > total。
            CacheBillingMode::Legacy => cache.split_legacy_overlapping(total),
        }
    }

    /// **真实（互斥）口径**，恒定不受 per-key 计费模式影响。
    ///
    /// 专供 `traces.db` 与利润报表使用。若这里也跟着 legacy 走，被缓存覆盖的前缀会
    /// 被重复计一次，我们**自己的**报表就会虚高——而真实成本走 `credits`
    /// （来自上游 `metering.usage`），和这三桶没有对应关系。
    /// 对外账单可以按 legacy 收，内部账必须看得准。
    pub(crate) fn split_internal(&self, cache: &CacheUsage) -> (i32, i32, i32) {
        cache.split_against_total(self.report_total())
    }

    fn report_total(&self) -> i32 {
        self.upstream_context_tokens
            .unwrap_or(self.client_visible_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::InputTokenUsage;
    use crate::admin::client_keys::CacheBillingMode;
    use crate::anthropic::cache_metering::CacheUsage;

    /// 有缓存覆盖时，两种口径的差别：legacy 把覆盖前缀重复计进 input。
    fn covered_usage() -> (InputTokenUsage, CacheUsage) {
        let mut usage = InputTokenUsage::new(0);
        usage.observe_upstream_context(1_000);
        let cache = CacheUsage {
            cache_read: 300,
            cache_covered_est: 800,
            prompt_total_est: 1_000,
            ..CacheUsage::default()
        };
        (usage, cache)
    }

    #[test]
    fn exclusive_mode_keeps_the_three_buckets_mutually_exclusive() {
        let (usage, cache) = covered_usage();
        let (input, creation, read) = usage.split_api(&cache);

        // 覆盖 80% → cache_total=800，内部按 read:creation = 300:500 拆
        assert_eq!((input, creation, read), (200, 500, 300));
        assert_eq!(
            input + creation + read,
            1_000,
            "互斥口径三桶之和必须等于 total"
        );
    }

    /// legacy 口径：覆盖前缀同时计进 input 与 creation，三桶之和 > total。
    ///
    /// **这不是 bug**——它就是同行普遍口径，也是本项目互斥修复之前的行为。
    /// 命中率整形照常生效、写创建照常产生，两者并存即该口径的定义。
    #[test]
    fn legacy_mode_double_counts_the_covered_prefix_on_purpose() {
        let (usage, cache) = covered_usage();
        let cache = cache.with_billing_mode(CacheBillingMode::Legacy);
        let (input, creation, read) = usage.split_api(&cache);

        assert_eq!(input, 1_000, "legacy 的 input 取全量，不扣掉覆盖前缀");
        assert_eq!(creation, 500);
        assert_eq!(read, 300);
        assert!(
            input + creation + read > 1_000,
            "三桶之和大于 total 是该口径的特征，不是缺陷"
        );
    }

    /// 钉住 `Legacy` 相对 `Exclusive` 的真实计价倍数，并证明它**随命中率上升**。
    ///
    /// 这不是防回归，是防误判：直觉容易以为两种口径差个零头，实测是 1.8×–5.4×
    /// （命中率越高差得越多，因为被重复计入的前缀正是命中的那部分）。定价按错一个
    /// 数量级比代码写错更贵，所以把数字放进测试而不是只写在注释里。
    #[test]
    fn legacy_billing_multiple_grows_with_hit_rate() {
        // Anthropic 口径的相对单价：input 1.0 / cache_write(5m) 1.25 / cache_read 0.1
        fn weighted_cost((input, creation, read): (i32, i32, i32)) -> f64 {
            input as f64 + creation as f64 * 1.25 + read as f64 * 0.1
        }
        fn multiple(total: i32, covered: i32, prompt_est: i32, cache_read: i32) -> f64 {
            let cache = CacheUsage {
                cache_read,
                cache_covered_est: covered,
                prompt_total_est: prompt_est,
                ..CacheUsage::default()
            }
            // 用户实际会配的整形区间，确认整形在两种口径下都参与计算。
            .with_hit_rate_bounds(0, 90);
            let mut usage = InputTokenUsage::new(0);
            usage.observe_upstream_context(total);

            let exclusive = weighted_cost(usage.split_api(&cache));
            let legacy =
                weighted_cost(usage.split_api(&cache.with_billing_mode(CacheBillingMode::Legacy)));
            legacy / exclusive
        }

        let first_turn = multiple(10_000, 10_000, 10_000, 0);
        let second_turn = multiple(10_000, 9_500, 10_000, 9_000);
        let long_session = multiple(60_000, 58_000, 60_000, 57_000);

        // 允许实现微调，但量级必须是"倍"而不是"个百分点"。
        assert!(
            (1.7..=1.9).contains(&first_turn),
            "首轮全写入约 1.8×，实测 {first_turn:.2}×"
        );
        assert!(
            (4.5..=5.0).contains(&second_turn),
            "第 2 轮约 4.7×，实测 {second_turn:.2}×"
        );
        assert!(
            (5.2..=5.6).contains(&long_session),
            "长会话约 5.4×，实测 {long_session:.2}×"
        );
        assert!(
            first_turn < second_turn && second_turn < long_session,
            "倍数必须随命中率单调上升：{first_turn:.2} < {second_turn:.2} < {long_session:.2}"
        );
    }

    /// 内部口径恒定为互斥，不随对外计费模式变化——否则我们自己的报表会虚高。
    #[test]
    fn internal_usage_stays_exclusive_even_when_billing_is_legacy() {
        let (usage, cache) = covered_usage();
        let exclusive = usage.split_internal(&cache);
        let legacy = cache.with_billing_mode(CacheBillingMode::Legacy);

        assert_eq!(
            exclusive,
            usage.split_internal(&legacy),
            "切成 legacy 计费后，内部口径必须一字不变"
        );
        let (i, c, r) = usage.split_internal(&legacy);
        assert_eq!(i + c + r, 1_000);
        // 同时确认对外口径确实变了，否则这条测试没有意义
        assert_ne!(usage.split_api(&legacy), usage.split_internal(&legacy));
    }

    /// 全 miss 时两种口径一致：没有覆盖前缀就没有可重复计的部分。
    /// 凭空把 prompt 塞进 creation 会让上报总量翻倍。
    #[test]
    fn full_miss_is_identical_in_both_modes() {
        let mut usage = InputTokenUsage::new(0);
        usage.observe_upstream_context(1_000);
        let cache = CacheUsage::default();

        let exclusive = usage.split_api(&cache);
        let legacy = cache.with_billing_mode(CacheBillingMode::Legacy);
        assert_eq!(exclusive, usage.split_api(&legacy));
        assert_eq!(exclusive, (1_000, 0, 0));
    }

    #[test]
    fn default_billing_mode_is_the_optimised_exclusive_split() {
        // 升级零行为变化：没配过的 Key 必须走互斥口径。
        let (usage, cache) = covered_usage();
        let (i, c, r) = usage.split_api(&cache);
        assert_eq!(i + c + r, 1_000);
        assert_eq!(CacheBillingMode::default(), CacheBillingMode::Exclusive);
    }

    #[test]
    fn api_usage_reports_upstream_context_when_available() {
        let mut usage = InputTokenUsage::new(72);
        usage.observe_upstream_context(5_417);

        // 客户端必须看到上游的真实占用，否则自动压缩永不触发。
        let (input, creation, read) = usage.split_api(&CacheUsage::default());
        assert_eq!((input, creation, read), (5_417, 0, 0));
        // 估算值仍保留：message_start 早于 contextUsageEvent，那时只有它可用。
        assert_eq!(usage.client_visible_tokens(), 72);
        assert_eq!(usage.upstream_context_tokens(), Some(5_417));
    }

    #[test]
    fn api_usage_falls_back_to_client_estimate_without_upstream() {
        let usage = InputTokenUsage::new(72);
        assert_eq!(usage.split_api(&CacheUsage::default()).0, 72);
    }

    #[test]
    fn cache_fields_sum_to_client_visible_total() {
        let usage = InputTokenUsage::new(100);
        let cache = CacheUsage {
            cache_read: 40,
            cache_covered_est: 60,
            prompt_total_est: 100,
            ..CacheUsage::default()
        };

        let (input, creation, read) = usage.split_api(&cache);
        assert_eq!(input + creation + read, 100);
    }

    #[test]
    fn api_usage_grows_with_upstream_context() {
        let mut short = InputTokenUsage::new(72);
        short.observe_upstream_context(5_417);
        let mut long = InputTokenUsage::new(182);
        long.observe_upstream_context(6_340);

        // 回报值须随上游真实占用增长，客户端才能看出窗口在被填满。
        assert_eq!(short.split_api(&CacheUsage::default()).0, 5_417);
        assert_eq!(long.split_api(&CacheUsage::default()).0, 6_340);
    }
}
