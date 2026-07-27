import { describe, expect, test } from 'bun:test'
import {
  connectionLabel,
  formatBalanceFreshness,
  formatCleanupCountdown,
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
      expect(
        formatCredentialLifespan({ addedAt: '2026-07-22T11:59:30.000Z' }, now),
      ).toEqual({ kind: 'alive', label: '已存活 30 秒' })
      expect(
        formatCredentialLifespan({ addedAt: '2026-07-22T11:02:00.000Z' }, now),
      ).toEqual({ kind: 'alive', label: '已存活 58 分钟' })
      expect(
        formatCredentialLifespan({ addedAt: '2026-07-19T12:00:00.000Z' }, now),
      ).toEqual({ kind: 'alive', label: '已存活 3 天' })
    })

    test('freezes at the moment of death once the account is banned', () => {
      // 线上观察到的典型寿命：约 58 分钟后被封
      expect(
        formatCredentialLifespan(
          { addedAt: '2026-07-22T10:00:00.000Z', diedAt: '2026-07-22T10:58:00.000Z' },
          now,
        ),
      ).toEqual({ kind: 'dead', label: '存活 58 分钟后死亡' })
    })

    test('dead label does not drift as time passes', () => {
      const input = {
        addedAt: '2026-07-22T10:00:00.000Z',
        diedAt: '2026-07-22T10:58:00.000Z',
      }
      expect(formatCredentialLifespan(input, now + 86_400_000)).toEqual(
        formatCredentialLifespan(input, now),
      )
    })

    /**
     * 线上 bug：被手动禁用/额度耗尽禁用的账号仍显示「已存活 N 小时」并持续增长。
     * 号早就不服务了，计时必须停。
     */
    test('a disabled account stops counting even without a death time', () => {
      expect(
        formatCredentialLifespan(
          { addedAt: '2026-07-22T10:00:00.000Z', disabled: true },
          now,
        ),
      ).toEqual({ kind: 'stopped', label: '已停用' })
    })

    /**
     * 线上 bug：本功能上线时所有存量凭据拿到同一个回填时间戳，界面直接算差值，
     * 于是 40 个账号全部显示「已存活 10 小时」—— 那是升级后经过的时长，不是账号寿命。
     */
    test('a backfilled join time is never used to compute a duration', () => {
      expect(
        formatCredentialLifespan(
          { addedAt: '2026-07-22T02:00:00.000Z', addedAtBackfilled: true },
          now,
        ),
      ).toEqual({ kind: 'unknown', label: '加入时间未知' })

      // 判死事实仍要说出来，只是不给具体时长
      expect(
        formatCredentialLifespan(
          {
            addedAt: '2026-07-22T02:00:00.000Z',
            addedAtBackfilled: true,
            diedAt: '2026-07-22T11:00:00.000Z',
          },
          now,
        ),
      ).toEqual({ kind: 'dead', label: '已封号' })
    })

    test('reports unknown instead of inventing a number', () => {
      expect(formatCredentialLifespan({}, now).kind).toBe('unknown')
      expect(formatCredentialLifespan({ addedAt: 'not-a-date' }, now).kind).toBe('unknown')
    })

    test('clock skew does not produce a negative duration', () => {
      // 死亡时间早于加入时间（时钟回拨 / 数据异常）时按 0 处理，不显示负数
      expect(
        formatCredentialLifespan(
          { addedAt: '2026-07-22T11:00:00.000Z', diedAt: '2026-07-22T10:00:00.000Z' },
          now,
        ),
      ).toEqual({ kind: 'dead', label: '存活 0 秒后死亡' })
    })
  })

  describe('cleanup countdown', () => {
    const now = Date.parse('2026-07-22T12:00:00.000Z')

    test('counts down from death time plus retention window', () => {
      // 10:00 判死 + 24h 保留 → 距清理还有 22 小时
      expect(formatCleanupCountdown('2026-07-22T10:00:00.000Z', 24, true, now)).toBe(
        '22 小时后清理',
      )
    })

    test('shows pending once the window has elapsed', () => {
      expect(formatCleanupCountdown('2026-07-20T10:00:00.000Z', 24, true, now)).toBe('待清理')
    })

    /** 手工添加的账号只禁用不删（通常是唯一一份），不该显示倒计时。 */
    test('returns null when the account is not subject to auto deletion', () => {
      expect(formatCleanupCountdown('2026-07-22T10:00:00.000Z', 24, false, now)).toBeNull()
    })

    test('returns null for accounts that are not dead', () => {
      expect(formatCleanupCountdown(undefined, 24, true, now)).toBeNull()
    })

    test('returns null when retention is unknown or invalid', () => {
      expect(formatCleanupCountdown('2026-07-22T10:00:00.000Z', undefined, true, now)).toBeNull()
      expect(formatCleanupCountdown('2026-07-22T10:00:00.000Z', 0, true, now)).toBeNull()
    })
  })
})
