//! 「客户端请求了 thinking，但 Kiro 未返回 reasoning」的聚合观测。
//!
//! 这条降级**不是错误**：请求正常完成，正文和工具调用都保留了。但它在生产上
//! 4.3 小时内出现 13233 次，占全部 WARN 的 70%，把 docker 日志（3×50 MB 轮转）
//! 的可回溯窗口压到 4 小时——真正的 WARN 全被它冲掉了。
//!
//! 所以逐条不再打 WARN，改成按模型聚合、每分钟最多输出一行 INFO。
//! 趋势（哪个模型降级、降级多少）保住了，个案退到 DEBUG，需要时开
//! `RUST_LOG=kiro_rs::anthropic=debug` 就能捞。

use std::collections::BTreeMap;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

/// 汇总输出间隔。取 60 秒：够把高频降级压成个位数行，又不至于让趋势迟到太久。
const FLUSH_INTERVAL: Duration = Duration::from_secs(60);

struct State {
    /// 用 BTreeMap 而不是 HashMap：输出顺序稳定，日志行可直接对比/diff。
    counts: BTreeMap<String, u64>,
    window_started: Instant,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        counts: BTreeMap::new(),
        window_started: Instant::now(),
    })
});

/// 记一次 thinking 降级。
///
/// 逐条走 DEBUG；累计到一分钟才汇总一行 INFO。调用方不需要再自己打日志。
pub(crate) fn record(model: &str) {
    tracing::debug!(model = %model, "客户端请求了 thinking，但 Kiro 未返回 reasoning；已保留正文或工具调用");

    let flushed = {
        let mut state = STATE.lock();
        *state.counts.entry(model.to_owned()).or_insert(0) += 1;
        if state.window_started.elapsed() < FLUSH_INTERVAL {
            None
        } else {
            let elapsed = state.window_started.elapsed();
            state.window_started = Instant::now();
            Some((std::mem::take(&mut state.counts), elapsed))
        }
    };

    // 在锁外格式化与输出：tracing 的写出可能阻塞，不该压着全局锁做。
    if let Some((counts, elapsed)) = flushed {
        let total: u64 = counts.values().sum();
        let breakdown = counts
            .iter()
            .map(|(model, count)| format!("{model}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        tracing::info!(
            window_secs = elapsed.as_secs(),
            total,
            "thinking 降级汇总（客户端要 thinking、上游未返回 reasoning；请求均已正常完成）：{}",
            breakdown
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关键契约：`record` 不再逐条产出 WARN。
    ///
    /// 这里不便断言 tracing 输出，退一步钉住「调用不 panic 且状态可累加」，
    /// 真正的降噪效果由 §3.3 的日志窗口观测验证。
    #[test]
    fn record_accumulates_without_panicking() {
        for _ in 0..10 {
            record("claude-opus-5");
            record("claude-sonnet-5");
        }
        let state = STATE.lock();
        // 尚未到一分钟，计数应当仍在窗口里累积而不是被清空。
        let total: u64 = state.counts.values().sum();
        assert!(total >= 20, "窗口内计数应累积，实际 {total}");
    }

    #[test]
    fn flush_interval_is_one_minute() {
        // 防止有人把间隔调成秒级又把降噪效果抹掉。
        assert_eq!(FLUSH_INTERVAL, Duration::from_secs(60));
    }
}
