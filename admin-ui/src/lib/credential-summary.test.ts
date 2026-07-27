import { describe, expect, test } from 'bun:test'
import {
  formatAvailableCreditSummary,
  formatCreditAmount,
  summarizeAvailableCredits,
} from './credential-summary'

describe('credential summary', () => {
  test('仅汇总启用账号的有限正余额，并统计有效观测', () => {
    const credentials = [
      { id: 1, disabled: false, balance: { remaining: 10 } },
      { id: 2, disabled: false, balance: { remaining: 0 } },
      { id: 3, disabled: false, balance: { remaining: -5 } },
      { id: 4, disabled: false },
      { id: 5, disabled: false, balance: { remaining: Infinity } },
      {
        id: 6,
        disabled: false,
        balance: { remaining: 2.5 },
        throttledRemainingSecs: 60,
      },
      { id: 7, disabled: true, balance: { remaining: 999 } },
    ]

    expect(summarizeAvailableCredits(credentials, new Map())).toEqual({
      availableCredits: 12.5,
      enabledCount: 6,
      observedCount: 4,
    })
  })

  test('优先使用余额覆盖值', () => {
    expect(
      summarizeAvailableCredits(
        [{ id: 1, disabled: false, balance: { remaining: 10 } }],
        new Map([[1, { remaining: 30 }]]),
      ),
    ).toEqual({
      availableCredits: 30,
      enabledCount: 1,
      observedCount: 1,
    })
  })

  // 数据源是 Kiro getUsageLimits 的额度计数，上游响应里没有货币字段 ——
  // 因此格式化为纯数字，单位「积分」由展示层给出，不再带 $。
  test('格式化积分总额和启用账号覆盖率', () => {
    expect(
      formatAvailableCreditSummary({
        availableCredits: 1234.5,
        enabledCount: 15,
        observedCount: 12,
      }),
    ).toEqual({
      value: '1,234.50',
      detail: '已统计 12/15 个启用账号',
    })
  })

  test('区分待查询、已观测零余额和无启用账号', () => {
    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 3,
        observedCount: 0,
      }),
    ).toEqual({
      value: '待查询',
      detail: '已统计 0/3 个启用账号',
    })

    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 2,
        observedCount: 2,
      }),
    ).toEqual({
      value: '0.00',
      detail: '已统计 2/2 个启用账号',
    })

    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 0,
        observedCount: 0,
      }),
    ).toEqual({
      value: '0.00',
      detail: '无启用账号',
    })
  })

  test('积分金额格式化保留千分位与两位小数，且不含货币符号', () => {
    expect(formatCreditAmount(49327.008)).toBe('49,327.01')
    expect(formatCreditAmount(0)).toBe('0.00')
    expect(formatCreditAmount(-12.5)).toBe('-12.50')
    // 非有限值不能渲染成 NaN
    expect(formatCreditAmount(Number.NaN)).toBe('0')
    expect(formatCreditAmount(Number.POSITIVE_INFINITY)).toBe('0')
  })

  /**
   * 回归守卫：额度来自 Kiro getUsageLimits 的 usageLimitWithPrecision /
   * currentUsageWithPrecision，上游响应里没有任何货币字段。此前误按美元展示
   * （$49,327.01），与概览页、消耗速率的纯数字口径冲突，也让「余量 ÷ 每分钟消耗
   * = 还能撑多久」这个换算读起来不成立。
   */
  test('不得把积分当成货币格式化', async () => {
    const { readFile } = await import('node:fs/promises')
    for (const file of ['credential-summary.ts', 'credential-metrics.ts']) {
      const source = await readFile(new URL(`./${file}`, import.meta.url), 'utf8')
      // 匹配「未被注释掉的实际配置行」而非裸字符串：解释性注释里会引用旧写法，
      // 扫源码文本区分不了「用到」和「提到」（本轮已在别处踩过两次）。
      const activeLines = source
        .split('\n')
        .filter((line) => !line.trim().startsWith('*') && !line.trim().startsWith('//'))
        .join('\n')
      expect(activeLines).not.toContain("style: 'currency'")
      expect(activeLines).not.toContain("currency: 'USD'")
    }
    const card = await readFile(
      new URL('../components/credential-card.tsx', import.meta.url),
      'utf8',
    )
    // 卡片曾用模板串硬编码 `$${...}` 拼美元符号
    expect(card).not.toContain('`$${')
    expect(card).not.toContain('`-$${')
  })
})
