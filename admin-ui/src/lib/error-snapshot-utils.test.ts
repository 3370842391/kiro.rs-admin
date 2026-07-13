import { describe, expect, test } from 'bun:test'
import { buildSnapshotParams, formatBytes, severityLabel } from './error-snapshot-utils'

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
})
