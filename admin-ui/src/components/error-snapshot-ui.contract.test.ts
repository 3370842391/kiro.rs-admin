import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

describe('error snapshot UI wiring', () => {
  test('adds a top-level snapshot page and trace drill-down', async () => {
    const app = await readFile('src/App.tsx', 'utf8')
    const trace = await readFile('src/components/trace-log-page.tsx', 'utf8')
    expect(app).toContain('key: "snapshots"')
    expect(app).toContain('<ErrorSnapshotPage />')
    expect(trace).toContain('rec.snapshotId')
    expect(trace).toContain('查看错误快照')
  })

  test('exposes all six governance controls', async () => {
    const trace = await readFile('src/components/trace-log-page.tsx', 'utf8')
    for (const field of [
      'errorSnapshotEnabled', 'errorSnapshotRetentionDays',
      'errorSnapshotMaxStorageGb', 'errorSnapshotCaptureRecovered',
      'errorSnapshotCaptureBodies', 'errorSnapshotMinFreeDiskGb',
    ]) expect(trace).toContain(field)
  })
})
