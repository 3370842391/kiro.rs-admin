import { describe, expect, test } from 'bun:test'
import { validateImageBudget } from './image-budget'

describe('validateImageBudget', () => {
  test('rejects retry settings larger than primary settings', () => {
    expect(
      validateImageBudget({
        enabled: true,
        totalBase64BudgetBytes: 819_200,
        historyMaxDimension: 1280,
        historyJpegQuality: 72,
        retryHistoryMaxDimension: 1600,
        retryHistoryJpegQuality: 80,
      }),
    ).toContain('重试')
  })

  test('accepts documented defaults', () => {
    expect(
      validateImageBudget({
        enabled: true,
        totalBase64BudgetBytes: 819_200,
        historyMaxDimension: 1280,
        historyJpegQuality: 72,
        retryHistoryMaxDimension: 960,
        retryHistoryJpegQuality: 60,
      }),
    ).toBeNull()
  })
})
