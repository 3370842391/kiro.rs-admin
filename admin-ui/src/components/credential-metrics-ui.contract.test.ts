import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8')
}

describe('credential metrics UI wiring', () => {
  test('renders all seven account metrics and uses shared formatters', async () => {
    const source = await readSource('components/credential-card.tsx')
    for (const label of ['近1分钟 RPM', 'RPM 使用率', '成功率', '进行中', 'Token', '余额更新', '连接']) {
      expect(source).toContain(label)
    }
    expect(source).toContain('formatRpmMetric')
    expect(source).toContain('formatSuccessRate')
    expect(source).toContain('CredentialMetricsStrip')
    expect(source).toContain('view === "list"')
    expect(source).toContain('view = "card"')
  })

  test('refreshes account state frequently enough for the one-minute RPM view', async () => {
    const source = await readSource('hooks/use-credentials.ts')
    expect(source).toContain('refetchInterval: 10000')
  })
})
