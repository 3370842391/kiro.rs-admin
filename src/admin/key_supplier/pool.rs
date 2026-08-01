//! 全局号池水位闸的判定逻辑。
//!
//! 这里只放不依赖 I/O 的纯函数，目的是让「不买超」这条核心不变式能被测试直接锁住，
//! 而不必搭一整套 mock。语义是**目标存量**而非「每次买几个」：所有自动采购来的可用
//! 凭据合计不得超过目标存量，任一供货商推来到货通知时按缺口补齐，只向推送方下单。

use super::service::{CountDecision, select_purchase_count};

/// 号池闸的判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolDecision {
    /// 按这个数量向推送方下单。
    Purchase(u32),
    /// 跳过，附机器可判定的原因。
    Skip(PoolSkipReason),
}

/// 跳过原因。做成枚举而不是直接给字符串，是为了让「为什么没买」在事件表里可区分、
/// 在测试里可断言——只有一句笼统的「skipped」等于没记录。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolSkipReason {
    /// 目标存量未配置或为 0。失效保护：宁可不买。
    TargetUnavailable,
    /// 全局可用数已达或超过目标存量。
    TargetReached,
    /// 缺口经夹逼后低于该家 `minPurchase`。
    BelowSupplierMinimum,
    /// 该家库存不足（仅 `kiro-rs` 会先查库存）。
    SupplierOutOfStock,
    /// 对方现在的单价高于该家配置的上限。
    UnitPriceTooHigh,
    /// 配了单价上限，但这家在下单前报不出单价。按「宁可少买」不买。
    UnitPriceUnknown,
}

impl PoolSkipReason {
    /// 落进事件表 message 并直接展示给运维，因此是中文固定串，与现有
    /// `SkipWithReason` 的用法保持一致，不做 i18n。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TargetUnavailable => "号池目标存量不可用，跳过采购",
            Self::TargetReached => "号池已达目标存量",
            Self::BelowSupplierMinimum => "缺口低于该供货商单笔下限",
            Self::SupplierOutOfStock => "供货商库存不足",
            Self::UnitPriceTooHigh => "单价高于配置的上限",
            Self::UnitPriceUnknown => "配了单价上限但供货商不报价，已跳过",
        }
    }
}

/// 缺口 = 目标存量 - 全局可用数，下界截断到 0。
///
/// 可用数已经超过目标存量时返回 0 而不是负数：这种情况只停止采购，绝不去处置
/// 多出来的凭据（那是运营决定，不是水位闸该做的事）。
pub fn deficit(target_count: u32, global_usable: usize) -> u32 {
    let usable = u32::try_from(global_usable).unwrap_or(u32::MAX);
    target_count.saturating_sub(usable)
}

/// 由缺口推出本次实际采购量。
///
/// 夹逼顺序：缺口 → 该家可用库存 → 该家 `maxPurchase`，再与 `minPurchase` 比。
///
/// 低于 `minPurchase` 时**放弃**而不是放大到 `minPurchase`。放大会买超目标存量，
/// 而且缺口越小超得越多——那正是这道闸要防的事。
///
/// `available_stock` 对不先查库存的协议（两家 kiroapp）传 `max_purchase` 占位，
/// 使该项夹逼成为无操作；它们的文档明确建议不要先查库存，查询与领取不在同一事务，
/// 多一次往返只会把货让给别人。
pub fn select_pool_purchase_count(
    target_count: u32,
    global_usable: usize,
    available_stock: u64,
    max_purchase: u32,
    min_purchase: u32,
) -> PoolDecision {
    if target_count == 0 {
        return PoolDecision::Skip(PoolSkipReason::TargetUnavailable);
    }
    let gap = deficit(target_count, global_usable);
    if gap == 0 {
        return PoolDecision::Skip(PoolSkipReason::TargetReached);
    }
    // 夹逼规则只有一份实现：复用现有的「取三者最小、低于下限则跳过」。两套实现
    // 迟早会在 minPurchase 的边界上漂移。
    match select_purchase_count(gap, available_stock, max_purchase, min_purchase) {
        CountDecision::Purchase(count) => PoolDecision::Purchase(count),
        // 现有函数不区分跳过原因，这里补上：库存耗尽和配置下限拦住是两回事，
        // 前者是正常竞争结果，后者需要人去调配置。
        CountDecision::Skip if available_stock == 0 => {
            PoolDecision::Skip(PoolSkipReason::SupplierOutOfStock)
        }
        CountDecision::Skip => PoolDecision::Skip(PoolSkipReason::BelowSupplierMinimum),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 在有界网格上穷举代替随机采样。这些整数域很小，穷举是确定性的、比随机抽样
    /// 更强，也不必为此引入属性测试依赖。
    const GRID: u32 = 12;

    fn grid_cases() -> impl Iterator<Item = (u32, usize, u64, u32, u32)> {
        (0..=GRID).flat_map(move |target| {
            (0..=GRID as usize).flat_map(move |usable| {
                (0..=u64::from(GRID)).flat_map(move |stock| {
                    (1..=GRID).flat_map(move |max| {
                        (1..=max).map(move |min| (target, usable, stock, max, min))
                    })
                })
            })
        })
    }

    /// Property 1：目标存量不变式。买完之后合计不能超过目标存量。
    /// 这是全组里唯一真正重要的一条——它直接等于「不会花超」。
    #[test]
    fn purchase_never_pushes_the_pool_past_the_target() {
        for (target, usable, stock, max, min) in grid_cases() {
            if let PoolDecision::Purchase(count) =
                select_pool_purchase_count(target, usable, stock, max, min)
            {
                let after = usable as u64 + u64::from(count);
                assert!(
                    after <= u64::from(target),
                    "买超了: target={target} usable={usable} stock={stock} \
                     max={max} min={min} count={count} after={after}"
                );
            }
        }
    }

    /// Property 2：缺口非负。`u32` 已经保证，这条防的是将来有人改成有符号类型。
    #[test]
    fn deficit_is_never_negative_and_saturates_when_the_pool_is_over_target() {
        assert_eq!(deficit(3, 0), 3);
        assert_eq!(deficit(3, 3), 0);
        assert_eq!(deficit(3, 10), 0, "超过目标存量时缺口为 0，不是负数");
        assert_eq!(deficit(0, 0), 0);
        // 可用数大到超出 u32 也不能回绕。
        assert_eq!(deficit(5, usize::MAX), 0);
    }

    /// Property 3 与 Property 5：单家上限与库存上限。
    #[test]
    fn purchase_respects_the_supplier_max_and_available_stock() {
        for (target, usable, stock, max, min) in grid_cases() {
            if let PoolDecision::Purchase(count) =
                select_pool_purchase_count(target, usable, stock, max, min)
            {
                assert!(count <= max, "超过单家上限: count={count} max={max}");
                assert!(
                    u64::from(count) <= stock,
                    "超过可用库存: count={count} stock={stock}"
                );
            }
        }
    }

    /// Property 4：下限二值性。要么不买，要么买够下限——不存在中间值。
    ///
    /// 这条排除了一个很自然但错误的实现：缺口小于 `minPurchase` 时把数量放大到
    /// `minPurchase` 去凑单。那样每次都会买超目标存量。
    #[test]
    fn purchase_is_either_zero_or_at_least_the_supplier_minimum() {
        for (target, usable, stock, max, min) in grid_cases() {
            match select_pool_purchase_count(target, usable, stock, max, min) {
                PoolDecision::Purchase(count) => {
                    assert!(count > 0, "Purchase(0) 不是合法结果");
                    assert!(
                        count >= min,
                        "买了但没到下限: count={count} min={min} target={target} usable={usable}"
                    );
                }
                PoolDecision::Skip(_) => {}
            }
        }
    }

    /// Property 6：确定性。纯函数无隐藏状态，同时锁住「不引入跨触发轮转游标」。
    #[test]
    fn decision_is_deterministic_for_the_same_inputs() {
        for (target, usable, stock, max, min) in grid_cases() {
            let first = select_pool_purchase_count(target, usable, stock, max, min);
            let second = select_pool_purchase_count(target, usable, stock, max, min);
            assert_eq!(first, second);
        }
    }

    /// 跳过原因必须可区分：「已达存量」「低于下限」「库存不足」「存量不可用」
    /// 是四种完全不同的运维动作。
    #[test]
    fn skip_reasons_distinguish_the_four_causes() {
        // 目标存量未配置 → 失效保护，优先级高于一切。
        assert_eq!(
            select_pool_purchase_count(0, 0, 100, 10, 1),
            PoolDecision::Skip(PoolSkipReason::TargetUnavailable)
        );
        // 池子已满。
        assert_eq!(
            select_pool_purchase_count(3, 3, 100, 10, 1),
            PoolDecision::Skip(PoolSkipReason::TargetReached)
        );
        assert_eq!(
            select_pool_purchase_count(3, 5, 100, 10, 1),
            PoolDecision::Skip(PoolSkipReason::TargetReached)
        );
        // 缺口 1 但该家单笔至少买 5。
        assert_eq!(
            select_pool_purchase_count(3, 2, 100, 10, 5),
            PoolDecision::Skip(PoolSkipReason::BelowSupplierMinimum)
        );
        // 对方没货了。
        assert_eq!(
            select_pool_purchase_count(3, 0, 0, 10, 1),
            PoolDecision::Skip(PoolSkipReason::SupplierOutOfStock)
        );

        // 四种原因的文案两两不同，否则事件表里区分不出来。
        let reasons = [
            PoolSkipReason::TargetUnavailable,
            PoolSkipReason::TargetReached,
            PoolSkipReason::BelowSupplierMinimum,
            PoolSkipReason::SupplierOutOfStock,
        ];
        for (index, left) in reasons.iter().enumerate() {
            for right in &reasons[index + 1..] {
                assert_ne!(left.as_str(), right.as_str());
            }
        }
    }

    /// 需求给的三个具体例子，直接钉住。
    #[test]
    fn documented_examples_hold() {
        // 目标 1、池子空 → 买 1。
        assert_eq!(
            select_pool_purchase_count(1, 0, 100, 10, 1),
            PoolDecision::Purchase(1)
        );
        // 目标 3、池里 2 个活号 → 只补 1。
        assert_eq!(
            select_pool_purchase_count(3, 2, 100, 10, 1),
            PoolDecision::Purchase(1)
        );
        // 目标 3、池里 3 个死号（可用数 0）→ 补满 3。
        assert_eq!(
            select_pool_purchase_count(3, 0, 100, 10, 1),
            PoolDecision::Purchase(3)
        );
        // 缺口 5 但该家单笔最多 2 → 这次只买 2，剩下的留给下一次推送。
        assert_eq!(
            select_pool_purchase_count(5, 0, 100, 2, 1),
            PoolDecision::Purchase(2)
        );
    }
}
