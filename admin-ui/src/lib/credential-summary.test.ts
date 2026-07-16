import { describe, expect, test } from 'bun:test'
import {
  formatAvailableCreditSummary,
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

  test('格式化美元总额和启用账号覆盖率', () => {
    expect(
      formatAvailableCreditSummary({
        availableCredits: 1234.5,
        enabledCount: 15,
        observedCount: 12,
      }),
    ).toEqual({
      value: '$1,234.50',
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
      value: '$0.00',
      detail: '已统计 2/2 个启用账号',
    })

    expect(
      formatAvailableCreditSummary({
        availableCredits: 0,
        enabledCount: 0,
        observedCount: 0,
      }),
    ).toEqual({
      value: '$0.00',
      detail: '无启用账号',
    })
  })
})
