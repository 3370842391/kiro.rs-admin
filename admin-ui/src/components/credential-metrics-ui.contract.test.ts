import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8')
}

describe('credential metrics UI wiring', () => {
  test('promotes live concurrency to a first-class gauge in both views', async () => {
    const source = await readSource('components/credential-card.tsx')

    expect(source).toContain('ConcurrencyGauge')
    expect(source).toContain('concurrencyTone')
    expect(source).toContain('concurrencyHint')
    expect(source).toContain('concurrencyFillRatio')
    // 并发在卡片视图与列表视图各渲染一次
    expect(source.match(/<ConcurrencyGauge\b/g)?.length).toBe(2)
    expect(source).toContain('view === "list"')
    expect(source).toContain('view = "card"')
  })

  test('drops the redundant metric strip in favour of a single quiet meta line', async () => {
    const source = await readSource('components/credential-card.tsx')

    expect(source).not.toContain('CredentialMetricsStrip')
    expect(source).not.toContain('近1分钟 RPM')
    expect(source).not.toContain('RPM 使用率')
    // 区域 / 端点 / 来源不再各占一个徽章，改为压在一行低对比度元信息里
    expect(source).toContain('CredentialMetaLine')
    expect(source).toContain('data-credential-meta')
    expect(source).not.toContain('Auth Region:')
    expect(source).not.toContain('API Region:')
    expect(source).not.toContain('来源: ')
  })

  test('keeps shared formatters for the surviving metrics', async () => {
    const source = await readSource('components/credential-card.tsx')

    expect(source).toContain('formatSuccessRate')
    expect(source).toContain('formatTokenState')
    expect(source).toContain('connectionLabel')
    expect(source).toContain('formatBalanceFreshness')
  })

  test('refreshes account state frequently enough for the one-minute RPM view', async () => {
    const source = await readSource('hooks/use-credentials.ts')
    expect(source).toContain('refetchInterval: 10000')
  })
})
