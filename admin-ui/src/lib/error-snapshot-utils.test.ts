import { describe, expect, test } from 'bun:test'
import {
  buildSnapshotParams,
  dispositionLabel,
  formatBytes,
  severityLabel,
  snapshotDisposition,
} from './error-snapshot-utils'
import type { ErrorSnapshotSummary } from '@/types/api'

function summary(overrides: Partial<ErrorSnapshotSummary>): ErrorSnapshotSummary {
  return {
    snapshotId: 'snap_test',
    traceId: 'trace_test',
    ts: '2026-07-15T00:00:00Z',
    model: 'claude-opus-4-8',
    isStream: true,
    keyId: 1,
    keySource: 'clientKey',
    responseMode: 'detection',
    finalCredentialId: 1,
    endpoint: 'ide',
    httpStatus: 200,
    finalStatus: 'error',
    errorType: 'bad_request',
    severity: 'error',
    errorMessage: null,
    recovered: false,
    pinned: false,
    retentionExempt: false,
    omittedDueToDiskPressure: false,
    payloadCount: 1,
    originalBytes: 10,
    compressedBytes: 5,
    createdAt: 1,
    updatedAt: 1,
    ...overrides,
  }
}

describe('error snapshot helpers', () => {
  test('omits empty filters and keeps booleans', () => {
    expect(buildSnapshotParams({ severity: '', recovered: false, pinned: true, limit: 50, offset: 0 }))
      .toEqual({ recovered: 'false', pinned: 'true', limit: '50', offset: '0' })
  })

  test('formats storage and severity consistently', () => {
    expect(formatBytes(1024 ** 3)).toBe('1.00 GB')
    expect(severityLabel('critical')).toBe('严重')
    expect(severityLabel('warning')).toBe('警告')
  })

  test('separates recovered, client disconnects, and final errors', () => {
    expect(snapshotDisposition(summary({ recovered: true, errorType: 'transient' }))).toBe('recovered')
    expect(snapshotDisposition(summary({ errorType: 'client_disconnected', finalStatus: 'interrupted' })))
      .toBe('client_disconnected')
    expect(snapshotDisposition(summary({ errorType: 'stream_read_error', finalStatus: 'interrupted' })))
      .toBe('final_error')
    expect(dispositionLabel('recovered')).toBe('已恢复告警')
    expect(dispositionLabel('client_disconnected')).toBe('客户端断开')
    expect(dispositionLabel('final_error')).toBe('最终失败')
  })
})
