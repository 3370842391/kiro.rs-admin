import type { ErrorSnapshotQuery, ErrorSnapshotSummary, SnapshotSeverity } from '@/types/api'

export type SnapshotDisposition = 'recovered' | 'client_disconnected' | 'final_error'

export function buildSnapshotParams(query: ErrorSnapshotQuery): Record<string, string> {
  const params: Record<string, string> = {}
  const entries: Array<[string, string | number | boolean | undefined]> = [
    ['traceId', query.traceId],
    ['model', query.model],
    ['errorType', query.errorType],
    ['httpStatus', query.httpStatus],
    ['credentialId', query.credentialId],
    ['severity', query.severity],
    ['recovered', query.recovered],
    ['pinned', query.pinned],
    ['from', query.from],
    ['to', query.to],
    ['limit', query.limit],
    ['offset', query.offset],
  ]
  for (const [key, value] of entries) {
    if (value !== undefined && value !== '') params[key] = String(value)
  }
  return params
}

export function formatBytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  const exponent = Math.min(Math.floor(Math.log(bytes) / Math.log(1024)), units.length - 1)
  return `${(bytes / 1024 ** exponent).toFixed(2)} ${units[exponent]}`
}

export function severityLabel(severity: SnapshotSeverity): string {
  return { critical: '严重', error: '错误', warning: '警告', info: '信息' }[severity]
}

export function snapshotDisposition(record: ErrorSnapshotSummary): SnapshotDisposition {
  if (record.recovered) return 'recovered'
  if (record.errorType === 'client_disconnected') return 'client_disconnected'
  return 'final_error'
}

export function dispositionLabel(disposition: SnapshotDisposition): string {
  return {
    recovered: '已恢复告警',
    client_disconnected: '客户端断开',
    final_error: '最终失败',
  }[disposition]
}
