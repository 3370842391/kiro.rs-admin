import { describe, expect, test } from 'bun:test'
import { validateImageBudget } from './image-budget'

describe('validateImageBudget', () => {
  test('rejects retry settings larger than primary settings', () => {
    expect(
      validateImageBudget({
        enabled: true,
        totalBase64BudgetBytes: 819_200,
        hardBase64LimitBytes: 8 * 1024 * 1024,
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
        hardBase64LimitBytes: 8 * 1024 * 1024,
        historyMaxDimension: 1280,
        historyJpegQuality: 72,
        retryHistoryMaxDimension: 960,
        retryHistoryJpegQuality: 60,
      }),
    ).toBeNull()
  })

  test('rejects a soft target above the hard limit', () => {
    expect(
      validateImageBudget({
        enabled: true,
        totalBase64BudgetBytes: 2 * 1024 * 1024,
        hardBase64LimitBytes: 1024 * 1024,
        historyMaxDimension: 1280,
        historyJpegQuality: 72,
        retryHistoryMaxDimension: 960,
        retryHistoryJpegQuality: 60,
      }),
    ).toContain('硬上限')
  })

  test('rejects a hard limit above 32 MiB', () => {
    expect(
      validateImageBudget({
        enabled: true,
        totalBase64BudgetBytes: 819_200,
        hardBase64LimitBytes: 32 * 1024 * 1024 + 1,
        historyMaxDimension: 1280,
        historyJpegQuality: 72,
        retryHistoryMaxDimension: 960,
        retryHistoryJpegQuality: 60,
      }),
    ).toContain('32 MiB')
  })
})
