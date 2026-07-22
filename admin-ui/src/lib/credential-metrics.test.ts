import { describe, expect, test } from 'bun:test'
import {
  connectionLabel,
  formatBalanceFreshness,
  formatRpmMetric,
  formatRpmUtilization,
  formatSuccessRate,
  formatTokenState,
} from './credential-metrics'

describe('credential metrics formatting', () => {
  test('shows the one-minute RPM window and limit', () => {
    expect(formatRpmMetric(7, 10)).toBe('7 / 10 次/分钟')
    expect(formatRpmMetric(7, 0)).toBe('7 次/分钟')
  })

  test('formats RPM utilization and handles unlimited or invalid input', () => {
    expect(formatRpmUtilization(7, 10)).toBe('70%')
    expect(formatRpmUtilization(11, 10)).toBe('100%')
    expect(formatRpmUtilization(7, 0)).toBe('不限速')
    expect(formatRpmUtilization(Number.NaN, 10)).toBe('未知')
    expect(formatRpmUtilization(1, -1)).toBe('未知')
  })

  test('formats success rate without showing a false zero for unused accounts', () => {
    expect(formatSuccessRate(8, 2)).toBe('80.0%')
    expect(formatSuccessRate(0, 0)).toBe('暂无数据')
    expect(formatSuccessRate(-1, 2)).toBe('未知')
  })

  test('formats token expiry relative to a supplied clock', () => {
    const now = Date.parse('2026-07-22T00:00:00.000Z')
    expect(formatTokenState('2026-07-23T00:00:00.000Z', now)).toBe('剩余 1天')
    expect(formatTokenState('2026-07-22T00:30:00.000Z', now)).toBe('剩余 30分钟')
    expect(formatTokenState('2026-07-21T23:59:00.000Z', now)).toBe('已过期')
    expect(formatTokenState('not-a-date', now)).toBe('未知')
  })

  test('formats balance cache freshness and missing cache', () => {
    const now = Date.parse('2026-07-22T00:00:00.000Z')
    const nowSeconds = Math.floor(now / 1000)
    expect(formatBalanceFreshness(nowSeconds - 30, now)).toBe('30秒前')
    expect(formatBalanceFreshness(nowSeconds - 90, now)).toBe('1分钟前')
    expect(formatBalanceFreshness(undefined, now)).toBe('未查询')
    expect(formatBalanceFreshness(Number.NaN, now)).toBe('未查询')
  })

  test('labels the connection path', () => {
    expect(connectionLabel(true)).toBe('代理')
    expect(connectionLabel(false)).toBe('直连')
  })
})
