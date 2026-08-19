//! 进价测算器：给定「多少钱买的、多少额度」，回答该把 NewAPI 倍率设到多少才赚钱。
//!
//! # 为什么需要它
//!
//! 号池的定价此前靠拍脑袋：进价看供货商报价，卖价看 NewAPI 倍率，两边没有换算桥。
//! 结果是线上实测出现过「一个 ¥800/10000 credits 的号，跑满也只卖得出 ¥550」——
//! 每个号结构性亏 ¥250 且**永远回不了本**，而这件事在买之前完全可以算出来。
//!
//! # 换算桥
//!
//! ```text
//! 成本侧: 成本/credit = 买入价 ÷ 额度积分
//! 收入侧: 卖价/credit = k × 分组倍率        （k 由实测得出，见 PricingCoefficients）
//! 回本条件: 卖价/credit ≥ 成本/credit
//! ```
//!
//! `k` 是**每 credit、每单位分组倍率能卖出多少人民币**。把倍率单独提出来做因子，是因为
//! NewAPI 的计费公式里倍率是纯线性乘数（`quota = tokens × model_ratio × group_ratio`），
//! 所以 `收入 ∝ 倍率` 严格成立——这让「反算该设多少倍率」有闭式解，不需要迭代试。
//!
//! # 为什么系数必须实测而不能写死
//!
//! `k` 随模型结构漂移：同样 1 个 credit，opus 与 sonnet 能卖的钱差 1.5 倍以上
//! （实测归一化后 opus-4-8 ≈0.19、sonnet-5 ≈0.13）。写死一个常数会让测算在模型
//! 结构变化后悄悄失真，而定价算错的代价远高于多做一次实测。

use serde::{Deserialize, Serialize};

/// 单个模型的实测系数。
///
/// 两个系数来源不同，所以都是 `Option`：`tokens_per_credit` 只需要 RS 本地 trace 就能算；
/// `rmb_per_credit_ratio` 还需要 NewAPI 流水能按 `trace_id` 对上，没对上就是 `None`。
/// 缺失时保持 `None` 而不是填 0——0 会让测算结果看起来"算出来了"，实际是假的。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCoefficient {
    pub model: String,
    /// ¥ /（credit × 分组倍率）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rmb_per_credit_ratio: Option<f64>,
    /// 每 credit 能产出多少 token（含缓存读，即真实吞吐口径）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_credit: Option<f64>,
    /// 该模型在实测窗口内消耗的 credits，用于判断样本够不够
    pub credits: f64,
}

/// 实测系数集合。混合口径是按 credits 加权的全池平均。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingCoefficients {
    /// 混合口径 ¥ /（credit × 倍率）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rmb_per_credit_ratio: Option<f64>,
    /// 混合口径 token / credit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokens_per_credit: Option<f64>,
    pub by_model: Vec<ModelCoefficient>,
    /// 参与实测的请求数。太小则系数不可信，前端据此提示。
    pub samples: u64,
    /// 实测窗口（分钟）
    pub window_minutes: u64,
    /// 实测时间（RFC3339）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub measured_at: Option<String>,
}

impl PricingCoefficients {
    /// 取某模型的系数，模型未指定或该模型没实测到就回退到混合口径。
    ///
    /// 回退是刻意的：新加的模型一开始必然没有样本，此时给混合口径的估算
    /// 比直接拒绝回答有用得多。
    fn resolve(&self, model: Option<&str>) -> (Option<f64>, Option<f64>, bool) {
        let picked = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|name| {
                self.by_model
                    .iter()
                    .find(|entry| entry.model.eq_ignore_ascii_case(name))
            });
        match picked {
            Some(entry) => {
                let rmb = entry.rmb_per_credit_ratio.or(self.rmb_per_credit_ratio);
                let tokens = entry.tokens_per_credit.or(self.tokens_per_credit);
                let exact = entry.rmb_per_credit_ratio.is_some();
                (rmb, tokens, exact)
            }
            None => (self.rmb_per_credit_ratio, self.tokens_per_credit, false),
        }
    }
}

/// 测算输入。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingInput {
    /// 买入价（¥），例：800
    pub cost_rmb: f64,
    /// 额度积分，必须是 Kiro 计量口径的 credit，例：10000
    pub quota_credits: f64,
    /// 正算：打算把 NewAPI 分组倍率设成多少
    #[serde(default)]
    pub group_ratio: Option<f64>,
    /// 反算：想要多少毛利率（百分比，0..100）
    #[serde(default)]
    pub target_margin_pct: Option<f64>,
    /// 只算某个模型；留空用混合口径
    #[serde(default)]
    pub model: Option<String>,
    /// 号平均能把额度跑到百分之多少就死/停用。默认 100。
    ///
    /// 这个字段不是可有可无的装饰：线上实测号常在 87% 左右就被封，
    /// 按 100% 估收入会系统性高估、把亏本的批次算成赚钱。
    #[serde(default)]
    pub consumed_pct: Option<f64>,
}

/// 测算结果。金额类字段在系数缺失时保持 `None`。
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PricingResult {
    /// 成本 / credit
    pub cost_per_credit: f64,
    /// 实际参与计算的额度（已按 consumed_pct 打折）
    pub effective_credits: f64,
    /// 用到的系数是不是该模型精确实测值（false = 回退到混合口径）
    pub model_exact: bool,

    /// 正算：卖价 / credit
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sell_rate_per_credit: Option<f64>,
    /// 正算：单号终身收入
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revenue_rmb: Option<f64>,
    /// 正算：单号终身利润
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_rmb: Option<f64>,
    /// 正算：毛利率（%）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub margin_pct: Option<f64>,

    /// 回本所需的最低分组倍率
    #[serde(skip_serializing_if = "Option::is_none")]
    pub breakeven_group_ratio: Option<f64>,
    /// 反算：达到目标毛利需要的分组倍率
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required_group_ratio: Option<f64>,

    /// 这个号总共能产出多少 token
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producible_tokens: Option<f64>,

    /// 结构性亏损警告：即使倍率拉满也回不了本时给出的提示
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

/// 输入非法（负数 / 零额度）时的错误。
///
/// 用 `Result` 而不是静默返回空结果：定价算错是要真金白银付代价的，
/// 悄悄给一个空壳比直接报错更危险。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PricingError {
    InvalidCost,
    InvalidQuota,
    InvalidGroupRatio,
    InvalidTargetMargin,
    InvalidConsumedPct,
}

impl std::fmt::Display for PricingError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::InvalidCost => "买入价必须大于 0",
            Self::InvalidQuota => "额度积分必须大于 0",
            Self::InvalidGroupRatio => "分组倍率必须大于 0",
            Self::InvalidTargetMargin => "目标毛利率必须在 0（含）到 100 之间",
            Self::InvalidConsumedPct => "额度消耗比例必须在 0 到 100 之间",
        };
        f.write_str(text)
    }
}

impl std::error::Error for PricingError {}

/// 倍率上限。超过它的"回本倍率"没有实操意义——没有客户会接受这个价，
/// 与其给一个数学上成立的天文数字，不如明确告诉运营这批号不该买。
const SANE_MAX_GROUP_RATIO: f64 = 10.0;

pub fn simulate(
    input: &PricingInput,
    coefficients: &PricingCoefficients,
) -> Result<PricingResult, PricingError> {
    if !(input.cost_rmb.is_finite() && input.cost_rmb > 0.0) {
        return Err(PricingError::InvalidCost);
    }
    if !(input.quota_credits.is_finite() && input.quota_credits > 0.0) {
        return Err(PricingError::InvalidQuota);
    }
    let consumed_pct = input.consumed_pct.unwrap_or(100.0);
    if !(consumed_pct.is_finite() && consumed_pct > 0.0 && consumed_pct <= 100.0) {
        return Err(PricingError::InvalidConsumedPct);
    }
    if let Some(ratio) = input.group_ratio
        && !(ratio.is_finite() && ratio > 0.0)
    {
        return Err(PricingError::InvalidGroupRatio);
    }
    if let Some(margin) = input.target_margin_pct
        && !(margin.is_finite() && (0.0..100.0).contains(&margin))
    {
        return Err(PricingError::InvalidTargetMargin);
    }

    let (k, tokens_per_credit, model_exact) = coefficients.resolve(input.model.as_deref());
    let effective_credits = input.quota_credits * consumed_pct / 100.0;
    // 成本按整号计（买了就付了），所以成本/credit 用的是**实际能跑出来的**额度做分母。
    // 用标称额度当分母会低估成本——跑不满的那部分是纯浪费。
    let cost_per_credit = input.cost_rmb / effective_credits;

    let mut result = PricingResult {
        cost_per_credit,
        effective_credits,
        model_exact,
        ..Default::default()
    };

    let Some(k) = k.filter(|value| value.is_finite() && *value > 0.0) else {
        // 没有实测卖价系数：token 产出仍可给（它只依赖 RS 本地数据），金额一律留空。
        result.producible_tokens = tokens_per_credit
            .filter(|value| value.is_finite() && *value > 0.0)
            .map(|per| effective_credits * per);
        result.warning = Some("尚未实测出卖价系数，无法给出金额；请先跑一次利润报表".into());
        return Ok(result);
    };

    result.producible_tokens = tokens_per_credit
        .filter(|value| value.is_finite() && *value > 0.0)
        .map(|per| effective_credits * per);

    let breakeven = cost_per_credit / k;
    result.breakeven_group_ratio = Some(breakeven);

    if let Some(ratio) = input.group_ratio {
        let sell_rate = k * ratio;
        let revenue = effective_credits * sell_rate;
        let profit = revenue - input.cost_rmb;
        result.sell_rate_per_credit = Some(sell_rate);
        result.revenue_rmb = Some(revenue);
        result.profit_rmb = Some(profit);
        result.margin_pct = (revenue > 0.0).then(|| profit / revenue * 100.0);
    }

    if let Some(margin) = input.target_margin_pct {
        // revenue × (1 − m) = cost  →  credits × k × G × (1 − m) = cost
        let required = input.cost_rmb / ((1.0 - margin / 100.0) * effective_credits * k);
        result.required_group_ratio = Some(required);
    }

    if breakeven > SANE_MAX_GROUP_RATIO {
        result.warning = Some(format!(
            "回本需要分组倍率 {breakeven:.2}，已超出可实操范围，这批号按此进价不该买"
        ));
    }

    Ok(result)
}

// ============ 系数实测 ============

/// 一条「收入 ↔ 成本」已对齐的样本。
///
/// 只有 NewAPI 流水能按 `trace_id` 对上 RS trace 时才构造得出来——对不上的流水
/// 不知道消耗了多少 credits，拿来算系数会污染分母。
#[derive(Debug, Clone)]
pub struct RevenueSample {
    pub model: String,
    /// 该请求的收入（¥）
    pub revenue_rmb: f64,
    /// 该请求消耗的 credits
    pub credits: f64,
    /// 该请求计费时用的分组倍率
    pub group_ratio: f64,
}

/// token 吞吐样本，纯 RS 侧数据，不依赖 NewAPI。
#[derive(Debug, Clone)]
pub struct TokenSample {
    pub model: String,
    pub tokens: f64,
    pub credits: f64,
}

/// 从实测样本推导系数。
///
/// `k = Σ收入 / Σ(credits × 倍率)`：分母带上倍率，得到的就是「每 credit 每单位倍率」
/// 的单价，与样本里各客户实际用的倍率无关。直接用 `Σ收入/Σcredits` 会把当期的倍率
/// 结构固化进系数，换个倍率就失真。
pub fn measure(
    revenue: &[RevenueSample],
    tokens: &[TokenSample],
    window_minutes: u64,
) -> PricingCoefficients {
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct Acc {
        revenue: f64,
        weighted_credits: f64,
        tokens: f64,
        credits: f64,
    }

    let mut by_model: BTreeMap<String, Acc> = BTreeMap::new();
    let mut total = Acc::default();
    let mut samples = 0u64;

    for s in revenue {
        if !(s.revenue_rmb.is_finite() && s.credits.is_finite() && s.group_ratio.is_finite()) {
            continue;
        }
        if s.credits <= 0.0 || s.group_ratio <= 0.0 || s.revenue_rmb < 0.0 {
            continue;
        }
        let weighted = s.credits * s.group_ratio;
        let entry = by_model.entry(s.model.clone()).or_default();
        entry.revenue += s.revenue_rmb;
        entry.weighted_credits += weighted;
        total.revenue += s.revenue_rmb;
        total.weighted_credits += weighted;
        samples += 1;
    }

    for s in tokens {
        if !(s.tokens.is_finite() && s.credits.is_finite()) || s.credits <= 0.0 || s.tokens < 0.0 {
            continue;
        }
        let entry = by_model.entry(s.model.clone()).or_default();
        entry.tokens += s.tokens;
        entry.credits += s.credits;
        total.tokens += s.tokens;
        total.credits += s.credits;
    }

    let ratio_of = |acc: &Acc| (acc.weighted_credits > 0.0).then(|| acc.revenue / acc.weighted_credits);
    let tokens_of = |acc: &Acc| (acc.credits > 0.0).then(|| acc.tokens / acc.credits);

    PricingCoefficients {
        rmb_per_credit_ratio: ratio_of(&total),
        tokens_per_credit: tokens_of(&total),
        by_model: by_model
            .into_iter()
            .map(|(model, acc)| ModelCoefficient {
                model,
                rmb_per_credit_ratio: ratio_of(&acc),
                tokens_per_credit: tokens_of(&acc),
                credits: if acc.credits > 0.0 {
                    acc.credits
                } else {
                    acc.weighted_credits
                },
            })
            .collect(),
        samples,
        window_minutes,
        measured_at: Some(chrono::Utc::now().to_rfc3339()),
    }
}

/// 实测系数的缓存。
///
/// 与 [`crate::admin::credential_earnings::SellRateStore`] 同样必须落盘：测算器接口
/// 不能每次都去打 NewAPI（慢且吃配额），所以只在跑利润报表时更新一次。
pub struct PricingCoefficientStore {
    data: parking_lot::Mutex<Option<PricingCoefficients>>,
    path: Option<std::path::PathBuf>,
}

impl PricingCoefficientStore {
    pub fn new(path: Option<std::path::PathBuf>) -> Self {
        let data = path
            .as_ref()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str::<PricingCoefficients>(&s).ok());
        Self {
            data: parking_lot::Mutex::new(data),
            path,
        }
    }

    pub fn get(&self) -> Option<PricingCoefficients> {
        self.data.lock().clone()
    }

    /// 样本为空时不覆盖：留着上一次的实测值，比换成一个算不出金额的空系数有用。
    pub fn record(&self, measured: PricingCoefficients) -> bool {
        if measured.rmb_per_credit_ratio.is_none() && measured.tokens_per_credit.is_none() {
            return false;
        }
        tracing::info!(
            rmb_per_credit_ratio = ?measured.rmb_per_credit_ratio,
            tokens_per_credit = ?measured.tokens_per_credit,
            samples = measured.samples,
            "已更新进价测算系数"
        );
        *self.data.lock() = Some(measured);
        self.persist();
        true
    }

    fn persist(&self) {
        let Some(path) = &self.path else { return };
        let json = {
            let data = self.data.lock();
            match serde_json::to_string_pretty(&*data) {
                Ok(j) => j,
                Err(error) => {
                    tracing::warn!(%error, "进价测算系数序列化失败");
                    return;
                }
            }
        };
        if let Err(error) =
            atomicwrites::AtomicFile::new(path, atomicwrites::OverwriteBehavior::AllowOverwrite)
                .write(|f| std::io::Write::write_all(f, json.as_bytes()))
        {
            tracing::warn!(%error, path = %path.display(), "进价测算系数落盘失败");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 线上实测量级：混合口径 k≈0.123 ¥/(credit·倍率)，tok/credit≈130000
    fn coeffs() -> PricingCoefficients {
        PricingCoefficients {
            rmb_per_credit_ratio: Some(0.123),
            tokens_per_credit: Some(130_000.0),
            by_model: vec![ModelCoefficient {
                model: "claude-opus-4-8".into(),
                rmb_per_credit_ratio: Some(0.19),
                tokens_per_credit: Some(115_081.0),
                credits: 8339.0,
            }],
            samples: 20_000,
            window_minutes: 1440,
            measured_at: Some("2026-08-19T00:00:00+00:00".into()),
        }
    }

    fn input(cost: f64, credits: f64) -> PricingInput {
        PricingInput {
            cost_rmb: cost,
            quota_credits: credits,
            ..Default::default()
        }
    }

    /// 复现线上那批 ¥800/10000 的号：倍率 0.3 时必亏，且回本倍率远高于 0.3。
    #[test]
    fn reproduces_the_loss_making_batch_measured_in_production() {
        let mut i = input(800.0, 10_000.0);
        i.group_ratio = Some(0.3);
        let r = simulate(&i, &coeffs()).unwrap();

        assert!((r.cost_per_credit - 0.08).abs() < 1e-9);
        // 0.123 × 0.3 = 0.0369 ¥/credit，跑满 10000 只有 ¥369
        assert!(r.revenue_rmb.unwrap() < 800.0, "该批次在 0.3 倍率下必然亏");
        assert!(r.profit_rmb.unwrap() < 0.0);
        // 回本需要 0.08 / 0.123 ≈ 0.65
        let be = r.breakeven_group_ratio.unwrap();
        assert!((be - 0.650).abs() < 0.01, "回本倍率应≈0.65，实际 {be}");
    }

    #[test]
    fn forward_calculation_is_linear_in_group_ratio() {
        let mut low = input(800.0, 10_000.0);
        low.group_ratio = Some(0.3);
        let mut high = input(800.0, 10_000.0);
        high.group_ratio = Some(0.6);

        let a = simulate(&low, &coeffs()).unwrap().revenue_rmb.unwrap();
        let b = simulate(&high, &coeffs()).unwrap().revenue_rmb.unwrap();
        // 倍率翻倍 → 收入翻倍。这条线性关系是反算有闭式解的前提。
        assert!((b / a - 2.0).abs() < 1e-9);
    }

    /// 反算出来的倍率，代回正算必须刚好命中目标毛利——闭环自洽。
    #[test]
    fn reverse_solved_ratio_hits_the_target_margin() {
        let mut i = input(800.0, 10_000.0);
        i.target_margin_pct = Some(40.0);
        let required = simulate(&i, &coeffs())
            .unwrap()
            .required_group_ratio
            .unwrap();

        let mut check = input(800.0, 10_000.0);
        check.group_ratio = Some(required);
        let margin = simulate(&check, &coeffs()).unwrap().margin_pct.unwrap();
        assert!((margin - 40.0).abs() < 1e-6, "实际毛利 {margin}");
    }

    #[test]
    fn breakeven_ratio_yields_exactly_zero_profit() {
        let i = input(800.0, 10_000.0);
        let be = simulate(&i, &coeffs()).unwrap().breakeven_group_ratio.unwrap();

        let mut check = input(800.0, 10_000.0);
        check.group_ratio = Some(be);
        let profit = simulate(&check, &coeffs()).unwrap().profit_rmb.unwrap();
        assert!(profit.abs() < 1e-9, "回本点利润应为 0，实际 {profit}");
    }

    /// 号跑不满额度会同时抬高成本/credit 并压低收入，必须两头都反映。
    #[test]
    fn early_death_raises_unit_cost_and_cuts_revenue() {
        // 取精确回本倍率而不是四舍五入的 0.65：差在小数点后第四位，
        // 却足以让「利润恰好为 0」的断言失败。
        let breakeven = simulate(&input(800.0, 10_000.0), &coeffs())
            .unwrap()
            .breakeven_group_ratio
            .unwrap();

        let mut full = input(800.0, 10_000.0);
        full.group_ratio = Some(breakeven);
        let mut early = input(800.0, 10_000.0);
        early.group_ratio = Some(breakeven);
        early.consumed_pct = Some(87.0);

        let f = simulate(&full, &coeffs()).unwrap();
        let e = simulate(&early, &coeffs()).unwrap();

        assert!(e.cost_per_credit > f.cost_per_credit, "跑不满则单位成本更高");
        assert!(e.revenue_rmb.unwrap() < f.revenue_rmb.unwrap());
        // 刚好回本的倍率，在只能跑到 87% 时就变成亏损
        assert!(f.profit_rmb.unwrap().abs() < 1e-9);
        assert!(e.profit_rmb.unwrap() < 0.0);
    }

    #[test]
    fn model_specific_coefficient_beats_blended_when_available() {
        let mut i = input(800.0, 10_000.0);
        i.group_ratio = Some(0.3);
        i.model = Some("claude-opus-4-8".into());
        let r = simulate(&i, &coeffs()).unwrap();

        assert!(r.model_exact, "该模型有实测值就该用它");
        // 0.19 × 0.3 = 0.057，高于混合口径的 0.0369
        assert!((r.sell_rate_per_credit.unwrap() - 0.057).abs() < 1e-9);
    }

    #[test]
    fn unknown_model_falls_back_to_blended_and_says_so() {
        let mut i = input(800.0, 10_000.0);
        i.group_ratio = Some(0.3);
        i.model = Some("brand-new-model".into());
        let r = simulate(&i, &coeffs()).unwrap();

        assert!(!r.model_exact);
        assert!((r.sell_rate_per_credit.unwrap() - 0.123 * 0.3).abs() < 1e-9);
    }

    #[test]
    fn token_output_is_reported_even_without_sell_rate() {
        // token 产出只依赖 RS 本地数据，不该被"卖价没实测到"连累
        let coefficients = PricingCoefficients {
            rmb_per_credit_ratio: None,
            tokens_per_credit: Some(130_000.0),
            ..Default::default()
        };
        let mut i = input(800.0, 10_000.0);
        i.group_ratio = Some(0.3);
        let r = simulate(&i, &coefficients).unwrap();

        assert_eq!(r.producible_tokens, Some(1_300_000_000.0));
        assert_eq!(r.revenue_rmb, None, "没有卖价系数时金额必须留空而不是填 0");
        assert!(r.warning.is_some());
    }

    #[test]
    fn hopeless_batch_gets_an_explicit_warning() {
        // 进价高到回本倍率超出可实操范围
        let i = input(100_000.0, 10_000.0);
        let r = simulate(&i, &coeffs()).unwrap();
        assert!(r.breakeven_group_ratio.unwrap() > SANE_MAX_GROUP_RATIO);
        assert!(r.warning.is_some(), "结构性亏损必须显式警告");
    }

    fn rev(model: &str, revenue: f64, credits: f64, ratio: f64) -> RevenueSample {
        RevenueSample {
            model: model.into(),
            revenue_rmb: revenue,
            credits,
            group_ratio: ratio,
        }
    }

    /// 核心性质：k 对倍率归一。同样的单价、不同倍率下产生的样本，必须推出同一个 k。
    ///
    /// 这条不成立的话，「反算该设多少倍率」就是错的——系数会把当期倍率结构固化进去。
    #[test]
    fn measured_coefficient_is_invariant_to_the_ratios_in_the_sample() {
        // 真实单价 k=0.12：倍率 0.3 的请求卖 0.036/credit，倍率 0.9 的卖 0.108/credit
        let samples = vec![
            rev("m", 100.0 * 0.12 * 0.3, 100.0, 0.3),
            rev("m", 500.0 * 0.12 * 0.9, 500.0, 0.9),
        ];
        let c = measure(&samples, &[], 60);
        assert!((c.rmb_per_credit_ratio.unwrap() - 0.12).abs() < 1e-12);
    }

    /// 反例护栏：若误用 Σ收入/Σcredits（不带倍率），同一批样本会得出被倍率结构污染的值。
    #[test]
    fn naive_unweighted_rate_would_differ_from_normalised_coefficient() {
        let samples = vec![
            rev("m", 100.0 * 0.12 * 0.3, 100.0, 0.3),
            rev("m", 500.0 * 0.12 * 0.9, 500.0, 0.9),
        ];
        let normalised = measure(&samples, &[], 60).rmb_per_credit_ratio.unwrap();
        let naive: f64 = samples.iter().map(|s| s.revenue_rmb).sum::<f64>()
            / samples.iter().map(|s| s.credits).sum::<f64>();
        assert!(
            (naive - normalised).abs() > 0.01,
            "两种口径必须显著不同，否则这条护栏没意义"
        );
    }

    #[test]
    fn measure_splits_by_model_and_keeps_blended_total() {
        let samples = vec![
            rev("opus", 0.19 * 1000.0 * 0.3, 1000.0, 0.3),
            rev("sonnet", 0.13 * 1000.0 * 0.3, 1000.0, 0.3),
        ];
        let tokens = vec![
            TokenSample { model: "opus".into(), tokens: 115_000.0 * 1000.0, credits: 1000.0 },
            TokenSample { model: "sonnet".into(), tokens: 201_000.0 * 1000.0, credits: 1000.0 },
        ];
        let c = measure(&samples, &tokens, 1440);

        let opus = c.by_model.iter().find(|m| m.model == "opus").unwrap();
        let sonnet = c.by_model.iter().find(|m| m.model == "sonnet").unwrap();
        assert!((opus.rmb_per_credit_ratio.unwrap() - 0.19).abs() < 1e-9);
        assert!((sonnet.rmb_per_credit_ratio.unwrap() - 0.13).abs() < 1e-9);
        assert!((opus.tokens_per_credit.unwrap() - 115_000.0).abs() < 1e-6);
        // 混合口径是加权平均，必须落在两者之间
        let blended = c.rmb_per_credit_ratio.unwrap();
        assert!(blended > 0.13 && blended < 0.19, "混合值 {blended}");
        assert_eq!(c.samples, 2);
    }

    /// token 样本不依赖 NewAPI，所以只有 token 样本时也该产出 tok/credit。
    #[test]
    fn token_only_measurement_still_yields_throughput() {
        let tokens = vec![TokenSample {
            model: "m".into(),
            tokens: 1_300_000.0,
            credits: 10.0,
        }];
        let c = measure(&[], &tokens, 60);
        assert_eq!(c.rmb_per_credit_ratio, None);
        assert_eq!(c.tokens_per_credit, Some(130_000.0));
    }

    #[test]
    fn measure_ignores_garbage_samples() {
        let samples = vec![
            rev("m", 12.0, 100.0, 1.0),
            rev("m", f64::NAN, 100.0, 1.0),
            rev("m", 5.0, 0.0, 1.0),   // 没有 credits，进分母会炸
            rev("m", 5.0, 100.0, 0.0), // 倍率 0，无法归一
        ];
        let c = measure(&samples, &[], 60);
        assert_eq!(c.samples, 1);
        assert!((c.rmb_per_credit_ratio.unwrap() - 0.12).abs() < 1e-12);
    }

    #[test]
    fn store_keeps_previous_value_when_new_measurement_is_empty() {
        let store = PricingCoefficientStore::new(None);
        assert!(store.get().is_none());
        assert!(store.record(coeffs()));
        assert!(store.get().is_some());

        // 空系数不该把已实测值抹掉
        assert!(!store.record(PricingCoefficients::default()));
        assert!(store.get().unwrap().rmb_per_credit_ratio.is_some());
    }

    #[test]
    fn rejects_invalid_input_instead_of_returning_empty_result() {
        assert_eq!(
            simulate(&input(0.0, 10_000.0), &coeffs()).unwrap_err(),
            PricingError::InvalidCost
        );
        assert_eq!(
            simulate(&input(800.0, 0.0), &coeffs()).unwrap_err(),
            PricingError::InvalidQuota
        );
        let mut bad_margin = input(800.0, 10_000.0);
        bad_margin.target_margin_pct = Some(100.0);
        assert_eq!(
            simulate(&bad_margin, &coeffs()).unwrap_err(),
            PricingError::InvalidTargetMargin
        );
        let mut bad_consumed = input(800.0, 10_000.0);
        bad_consumed.consumed_pct = Some(0.0);
        assert_eq!(
            simulate(&bad_consumed, &coeffs()).unwrap_err(),
            PricingError::InvalidConsumedPct
        );
        let mut bad_ratio = input(800.0, 10_000.0);
        bad_ratio.group_ratio = Some(-1.0);
        assert_eq!(
            simulate(&bad_ratio, &coeffs()).unwrap_err(),
            PricingError::InvalidGroupRatio
        );
    }
}
