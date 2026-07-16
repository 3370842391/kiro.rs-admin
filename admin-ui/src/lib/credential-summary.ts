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

const USD_FORMATTER = new Intl.NumberFormat('en-US', {
  style: 'currency',
  currency: 'USD',
  minimumFractionDigits: 2,
  maximumFractionDigits: 2,
})

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
      value: USD_FORMATTER.format(0),
      detail: '无启用账号',
    }
  }

  const detail = `已统计 ${summary.observedCount}/${summary.enabledCount} 个启用账号`

  if (summary.observedCount === 0) {
    return { value: '待查询', detail }
  }

  return {
    value: USD_FORMATTER.format(summary.availableCredits),
    detail,
  }
}
