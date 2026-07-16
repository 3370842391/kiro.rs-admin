import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('admin credential observability UI wiring', () => {
  test('dashboard derives credit summary from all credentials and fresh balance overrides', async () => {
    const dashboard = await readSource('src/components/dashboard.tsx')

    expect(dashboard).toContain('summarizeAvailableCredits')
    expect(dashboard).toMatch(
      /summarizeAvailableCredits\(\s*data\?\.credentials\s*\?\?\s*\[\],\s*balanceMap,?\s*\)/s,
    )
    expect(dashboard).toContain('availableCreditSummary={availableCreditSummary}')
  })

  test('RPM status bar renders available credits and coverage', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    expect(status).toContain('AvailableCreditSummary')
    expect(status).toContain('formatAvailableCreditSummary')
    expect(status).toContain('label="可用积分"')
    expect(status).toContain('value={creditDisplay.value}')
    expect(status).toContain('detail={creditDisplay.detail}')
    expect(status).toContain('sm:grid-cols-3')
    expect(status).toContain('xl:grid-cols-6')
  })
})
