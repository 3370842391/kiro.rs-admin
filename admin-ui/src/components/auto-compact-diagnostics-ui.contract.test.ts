import { describe, expect, test } from 'bun:test'

describe('auto compact diagnostics UI wiring', () => {
  test('exposes an independent governance switch', async () => {
    const api = await Bun.file('src/api/credentials.ts').text()
    const page = await Bun.file('src/components/trace-log-page.tsx').text()

    expect(api).toContain('autoCompactDiagnosticsEnabled')
    expect(page).toContain('autoCompactDiagnosticsEnabled')
    expect(page).toContain('自动压缩诊断')
    expect(page).toContain('独立于链路追踪')
  })

  test('sends diagnosis, session and high pressure filters', async () => {
    const api = await Bun.file('src/api/traces.ts').text()
    const types = await Bun.file('src/types/api.ts').text()
    const page = await Bun.file('src/components/trace-log-page.tsx').text()

    for (const field of ['compactionDiagnosis', 'sessionHash', 'highPressureOnly']) {
      expect(api).toContain(field)
      expect(types).toContain(field)
      expect(page).toContain(field)
    }
    expect(page).toContain('只看高压力')
    expect(page).toContain('查看同会话')
  })

  test('renders only the safe diagnostics contract', async () => {
    const types = await Bun.file('src/types/api.ts').text()
    const page = await Bun.file('src/components/trace-log-page.tsx').text()

    expect(types).toContain('CompactionTrace')
    expect(types).toContain('CompactionDiagnostics')
    for (const field of [
      'requestBodyBytes',
      'upstreamContextTokens',
      'upstreamContextPercentage',
      'clientReportedTokens',
      'requestShape',
      'meteringEventCount',
      'messageStartEnqueued',
      'contextWindowExceededEnqueued',
    ]) {
      expect(page).toContain(field)
    }
    expect(page).not.toContain('user_id')
    expect(page).not.toContain('requestBodyText')
    expect(page).not.toContain('toolParameters')
    expect(page).not.toContain('authorization')
  })
})
