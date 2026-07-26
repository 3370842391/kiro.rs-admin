import { describe, expect, test } from 'bun:test'
import {
  connectionLabel,
  formatBalanceFreshness,
  formatCredentialLifespan,
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

  describe('credential lifespan', () => {
    const now = Date.parse('2026-07-22T12:00:00.000Z')

    test('counts up from the join time while the account is alive', () => {
      expect(formatCredentialLifespan('2026-07-22T11:59:30.000Z', undefined, now)).toEqual({
        kind: 'alive',
        label: '已存活 30 秒',
      })
      expect(formatCredentialLifespan('2026-07-22T11:02:00.000Z', undefined, now)).toEqual({
        kind: 'alive',
        label: '已存活 58 分钟',
      })
      expect(formatCredentialLifespan('2026-07-19T12:00:00.000Z', undefined, now)).toEqual({
        kind: 'alive',
        label: '已存活 3 天',
      })
    })

    test('freezes at the moment of death once the account is banned', () => {
      // 线上观察到的典型寿命：约 58 分钟后被封
      const result = formatCredentialLifespan(
        '2026-07-22T10:00:00.000Z',
        '2026-07-22T10:58:00.000Z',
        now,
      )
      expect(result).toEqual({ kind: 'dead', label: '存活 58 分钟后死亡' })
    })

    test('dead label does not drift as time passes', () => {
      const args = ['2026-07-22T10:00:00.000Z', '2026-07-22T10:58:00.000Z'] as const
      const early = formatCredentialLifespan(args[0], args[1], now)
      const later = formatCredentialLifespan(args[0], args[1], now + 86_400_000)
      expect(later).toEqual(early)
    })

    test('reports unknown instead of inventing a number', () => {
      expect(formatCredentialLifespan(undefined, undefined, now).kind).toBe('unknown')
      expect(formatCredentialLifespan('not-a-date', undefined, now).kind).toBe('unknown')
    })

    test('clock skew does not produce a negative duration', () => {
      // 死亡时间早于加入时间（时钟回拨 / 数据异常）时按 0 处理，不显示负数
      const result = formatCredentialLifespan(
        '2026-07-22T11:00:00.000Z',
        '2026-07-22T10:00:00.000Z',
        now,
      )
      expect(result).toEqual({ kind: 'dead', label: '存活 0 秒后死亡' })
    })
  })
})
