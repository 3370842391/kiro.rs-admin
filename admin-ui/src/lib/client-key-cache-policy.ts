import type { CacheBillingMode, ClientCachePolicy } from '@/types/api'

/** 表单里的"继承全局"哨兵值。Select 不能用空字符串做 value。 */
export const INHERIT = 'inherit' as const

export type BillingModeChoice = typeof INHERIT | CacheBillingMode
export type TtlChoice = typeof INHERIT | '300' | '1800' | '3600'

/**
 * 允许的 TTL 取值，必须与 Rust `ALLOWED_TTL_SECS` 逐项一致。
 * 后端对非法值会整次拒绝更新，前端多给一个选项就是一个必然报错的按钮。
 * 由 `client-key-cache-policy.contract.test.ts` 直接读 Rust 源码守住。
 */
export const ALLOWED_TTL_SECS = [300, 1800, 3600] as const

/**
 * 计费口径取值，必须与 Rust `CacheBillingMode` 的 serde 变体一致。
 * 同上，由契约测试守住。
 */
export const BILLING_MODES = ['exclusive', 'legacy'] as const

export function ttlLabel(secs: number): string {
  return secs % 3600 === 0 ? `${secs / 3600} 小时` : `${secs / 60} 分钟`
}

export function billingModeLabel(mode: CacheBillingMode): string {
  return mode === 'legacy' ? '同行口径（普通客户）' : '优化互斥（优质客户）'
}

export function billingModeDescription(choice: BillingModeChoice): string {
  switch (choice) {
    case 'exclusive':
      return '三桶之和 == input 总量，被缓存覆盖的前缀只计一次。给优质客户。'
    case 'legacy':
      // 量级必须写清楚：只说"账单更高"会让人以为差个零头，实际是倍数级。
      return '覆盖前缀同时计进 input 与 cache_creation（互斥修复前的行为）。按 Anthropic 单价权重实算，账单约为优化口径的 1.8×（首轮）到 5.4×（长会话高命中）——命中率越高差得越多。命中率整形与写创建照常生效。'
    default:
      return '跟随全局配置（当前全局为优化互斥口径）。'
  }
}

/** 列表列展示：把策略压成一行。空策略显示"继承全局"。 */
export function cachePolicyLabel(policy: ClientCachePolicy | undefined): string {
  if (!policy || (policy.billingMode === undefined && policy.defaultTtlSecs === undefined)) {
    return '继承全局'
  }
  const parts: string[] = []
  if (policy.billingMode !== undefined) {
    parts.push(policy.billingMode === 'legacy' ? '同行口径' : '优化互斥')
  }
  if (policy.defaultTtlSecs !== undefined) {
    parts.push(`TTL ${ttlLabel(policy.defaultTtlSecs)}`)
  }
  return parts.join(' · ')
}

/**
 * 表单 → 请求体。整体替换语义：返回 `{}` 表示两项都恢复继承全局。
 *
 * 刻意不省略成 `undefined`：`undefined` 在后端是"不动"，
 * 会让用户把自定义改回"继承全局"后保存却毫无变化。
 */
export function buildClientCachePolicy(form: {
  billingMode: BillingModeChoice
  ttl: TtlChoice
}): ClientCachePolicy {
  const policy: ClientCachePolicy = {}
  if (form.billingMode !== INHERIT) {
    policy.billingMode = form.billingMode
  }
  if (form.ttl !== INHERIT) {
    policy.defaultTtlSecs = Number(form.ttl)
  }
  return policy
}

/** 现有策略 → 表单初值。 */
export function cachePolicyToForm(policy: ClientCachePolicy | undefined): {
  billingMode: BillingModeChoice
  ttl: TtlChoice
} {
  return {
    billingMode: policy?.billingMode ?? INHERIT,
    ttl: policy?.defaultTtlSecs === undefined ? INHERIT : (String(policy.defaultTtlSecs) as TtlChoice),
  }
}
