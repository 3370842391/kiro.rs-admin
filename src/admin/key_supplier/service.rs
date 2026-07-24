#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountDecision {
    Purchase(u32),
    Skip,
}

pub fn select_purchase_count(
    event_count: u32,
    stock_count: u64,
    configured_max: u32,
    configured_min: u32,
) -> CountDecision {
    let count = event_count.min(stock_count.min(u64::from(u32::MAX)) as u32).min(configured_max);
    if count < configured_min {
        CountDecision::Skip
    } else {
        CountDecision::Purchase(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn purchase_count_respects_event_stock_and_configured_bounds() {
        assert_eq!(select_purchase_count(20, 8, 5, 2), CountDecision::Purchase(5));
        assert_eq!(select_purchase_count(3, 1, 10, 2), CountDecision::Skip);
    }
}
