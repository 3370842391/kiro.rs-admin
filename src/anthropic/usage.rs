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

    pub(crate) fn split_api(&self, cache: &CacheUsage) -> (i32, i32, i32) {
        cache.split_against_total(self.client_visible_tokens)
    }
}

#[cfg(test)]
mod tests {
    use super::InputTokenUsage;
    use crate::anthropic::cache_metering::CacheUsage;

    #[test]
    fn api_usage_keeps_client_visible_total_when_upstream_is_larger() {
        let mut usage = InputTokenUsage::new(72);
        usage.observe_upstream_context(5_417);

        let (input, creation, read) = usage.split_api(&CacheUsage::default());
        assert_eq!((input, creation, read), (72, 0, 0));
        assert_eq!(usage.client_visible_tokens(), 72);
        assert_eq!(usage.upstream_context_tokens(), Some(5_417));
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
    fn api_usage_grows_only_with_client_visible_prompt() {
        let mut short = InputTokenUsage::new(72);
        short.observe_upstream_context(5_417);
        let mut long = InputTokenUsage::new(182);
        long.observe_upstream_context(6_340);

        assert_eq!(short.split_api(&CacheUsage::default()).0, 72);
        assert_eq!(long.split_api(&CacheUsage::default()).0, 182);
        assert_eq!(182 - 72, 110);
    }
}
