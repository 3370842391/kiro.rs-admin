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

  test('credential identity badges wrap without clipping', async () => {
    const card = await readSource('src/components/credential-card.tsx')
    const badgeRow = card.match(
      /<div className="([^"]*\[&>\*\]:shrink-0[^"]*)">\s*\{badges\}/,
    )?.[1]

    expect(badgeRow).toBeDefined()
    expect(badgeRow).toContain('flex-wrap')
    expect(badgeRow).toContain('gap-x-1')
    expect(badgeRow).toContain('gap-y-1')
    expect(badgeRow).not.toContain('overflow-hidden')
    expect(card).toContain('truncate text-sm font-medium leading-5')
  })

  test('failure dialog groups records and exposes the complete endpoint chain', async () => {
    const dialog = await readSource('src/components/credential-failures-dialog.tsx')

    expect(dialog).toContain('records.map((rec)')
    expect(dialog).toContain('sortTraceAttempts')
    expect(dialog).toContain('failureDisposition')
    expect(dialog).toContain('failureDispositionLabel')
    expect(dialog).toContain('compactAttemptLabel')
    expect(dialog).toContain('客户端 Key：')
    expect(dialog).toContain('端点：')
    expect(dialog).toContain('耗时')
    expect(dialog).not.toContain('failedHops')
    expect(dialog).not.toContain('本次请求最终由其他凭据成功')
  })
})
