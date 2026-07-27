export interface CreditBalance {
  remaining: number
}

export interface CreditCredential {
  id: number
  disabled: boolean
  balance?: CreditBalance
}

export interface AvailableCreditSummary {
  availableCredits: number
  enabledCount: number
  observedCount: number
}

export interface AvailableCreditDisplay {
  value: string
  detail: string
}

/**
 * 积分格式化。
 *
 * 这里曾用 `style: 'currency', currency: 'USD'` 显示成 `$49,327.01`，但数据源是
 * Kiro `getUsageLimits` 的 `usageLimitWithPrecision` / `currentUsageWithPrecision`
 * —— 上游响应里没有任何货币字段，那就是纯额度计数。加美元符号是无依据的，而且同一
 * 个概念在概览页和消耗速率上是纯数字，两处口径不一致，也让「余量 ÷ 每分钟消耗 =
 * 还能撑多久」这种换算显得不合逻辑。
 *
 * 改为普通千分位数字，单位「积分」由展示层作为独立文案给出。
 */
const CREDIT_FORMATTER = new Intl.NumberFormat('en-US', {
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
})

/** 把积分数值格式化成带千分位的字符串（不含单位）。 */
export function formatCreditAmount(value: number): string {
  if (!Number.isFinite(value)) return '0'
  return CREDIT_FORMATTER.format(value)
}

export function summarizeAvailableCredits(
  credentials: ReadonlyArray<CreditCredential>,
  balanceOverrides: ReadonlyMap<number, CreditBalance>,
): AvailableCreditSummary {
  let availableCredits = 0
  let enabledCount = 0
  let observedCount = 0

  for (const credential of credentials) {
    if (credential.disabled) continue

    enabledCount += 1
    const remaining = (
      balanceOverrides.get(credential.id) ?? credential.balance
    )?.remaining

    if (remaining === undefined || !Number.isFinite(remaining)) continue

    observedCount += 1
    if (remaining > 0) availableCredits += remaining
  }

  return { availableCredits, enabledCount, observedCount }
}

export function formatAvailableCreditSummary(
  summary: AvailableCreditSummary,
): AvailableCreditDisplay {
  if (summary.enabledCount === 0) {
    return {
      value: formatCreditAmount(0),
      detail: '无启用账号',
    }
  }

  const detail = `已统计 ${summary.observedCount}/${summary.enabledCount} 个启用账号`

  if (summary.observedCount === 0) {
    return { value: '待查询', detail }
  }

  return {
    value: formatCreditAmount(summary.availableCredits),
    detail,
  }
}
