import type { ImageBudgetConfig } from '@/types/api'

export function validateImageBudget(value: ImageBudgetConfig): string | null {
  if (
    !Number.isInteger(value.totalBase64BudgetBytes) ||
    value.totalBase64BudgetBytes < 256 * 1024 ||
    value.totalBase64BudgetBytes > 8 * 1024 * 1024
  ) {
    return '图片总预算必须在 256 KiB–8 MiB 之间'
  }
  if (
    !Number.isInteger(value.historyMaxDimension) ||
    value.historyMaxDimension < 640 ||
    value.historyMaxDimension > 4096
  ) {
    return '历史图片最大边长必须在 640–4096 之间'
  }
  if (
    !Number.isInteger(value.historyJpegQuality) ||
    value.historyJpegQuality < 40 ||
    value.historyJpegQuality > 95
  ) {
    return '历史图片 JPEG 质量必须在 40–95 之间'
  }
  if (
    !Number.isInteger(value.retryHistoryMaxDimension) ||
    value.retryHistoryMaxDimension < 480 ||
    value.retryHistoryMaxDimension > value.historyMaxDimension
  ) {
    return '重试最大边长必须在 480–普通历史图片最大边长之间'
  }
  if (
    !Number.isInteger(value.retryHistoryJpegQuality) ||
    value.retryHistoryJpegQuality < 30 ||
    value.retryHistoryJpegQuality > value.historyJpegQuality
  ) {
    return '重试 JPEG 质量必须在 30–普通历史图片质量之间'
  }
  return null
}
