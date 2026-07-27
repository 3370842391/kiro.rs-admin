use super::cache_metering::CacheUsage;

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
        let total = self
            .upstream_context_tokens
            .unwrap_or(self.client_visible_tokens);
        cache.split_against_total(total)
    }
}

#[cfg(test)]
mod tests {
    use super::InputTokenUsage;
    use crate::anthropic::cache_metering::CacheUsage;

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
