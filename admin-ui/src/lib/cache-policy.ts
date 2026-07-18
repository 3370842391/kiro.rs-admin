export const CACHE_TTL_OPTIONS = [300, 1800, 3600] as const

export type CacheTtlSeconds = (typeof CACHE_TTL_OPTIONS)[number]

export function formatTtl(seconds: number): string {
  if (seconds === 300) return '5 分钟'
  if (seconds === 1800) return '30 分钟'
  if (seconds === 3600) return '1 小时'
  return `${seconds} 秒`
}

export function validateCachePolicyDraft(value: {
  capacity: number
  flushIntervalSecs: number
  rollingPrefixLimit: number
  minPct: number
  maxPct: number
}): string | null {
  if (!Number.isInteger(value.capacity) || value.capacity < 256 || value.capacity > 65_536) {
    return '最大条目必须是 256–65536 内的整数'
  }
  if (
    !Number.isInteger(value.rollingPrefixLimit) ||
    value.rollingPrefixLimit < 2 ||
    value.rollingPrefixLimit > 64
  ) {
    return '滚动前缀数必须是 2–64 内的整数'
  }
  if (
    !Number.isInteger(value.flushIntervalSecs) ||
    value.flushIntervalSecs < 10 ||
    value.flushIntervalSecs > 600
  ) {
    return '落盘周期必须是 10–600 秒内的整数'
  }
  if (
    !Number.isInteger(value.minPct) ||
    !Number.isInteger(value.maxPct) ||
    value.minPct < 0 ||
    value.minPct > 100 ||
    value.maxPct < 0 ||
    value.maxPct > 100
  ) {
    return '命中率必须是 0–100% 内的整数'
  }
  if (value.minPct > 0 && value.maxPct > 0 && value.minPct > value.maxPct) {
    return '命中率下界不能大于上界'
  }
  return null
}
