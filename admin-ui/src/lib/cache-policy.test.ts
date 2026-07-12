import { describe, expect, test } from 'bun:test'
import { formatTtl, validateCachePolicyDraft } from './cache-policy'

describe('cache policy', () => {
  test('formats supported TTLs', () => {
    expect(formatTtl(300)).toBe('5 分钟')
    expect(formatTtl(1800)).toBe('30 分钟')
    expect(formatTtl(3600)).toBe('1 小时')
  })

  test('validates capacity, interval and bounds', () => {
    expect(
      validateCachePolicyDraft({
        capacity: 4096,
        flushIntervalSecs: 60,
        minPct: 0,
        maxPct: 95,
      }),
    ).toBeNull()
    expect(
      validateCachePolicyDraft({
        capacity: 100,
        flushIntervalSecs: 60,
        minPct: 0,
        maxPct: 95,
      }),
    ).toContain('256')
    expect(
      validateCachePolicyDraft({
        capacity: 4096,
        flushIntervalSecs: 60,
        minPct: 99,
        maxPct: 90,
      }),
    ).toContain('下界')
  })
})
