import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('empty user message compatibility UI wiring', () => {
  test('settings dialog loads and toggles the persisted compatibility flag', async () => {
    const dialog = await readSource('src/components/compatibility-settings-dialog.tsx')
    const api = await readSource('src/api/credentials.ts')
    const hooks = await readSource('src/hooks/use-credentials.ts')
    const topbar = await readSource('src/components/topbar-tools.tsx')

    expect(dialog).toContain('useCompatibilityConfig')
    expect(dialog).toContain('useSetCompatibilityConfig')
    expect(dialog).toContain('emptyUserMessageCompat')
    expect(dialog).toContain('Switch')
    expect(api).toContain("'/config/compatibility'")
    expect(hooks).toContain("queryKey: ['compatibilityConfig']")
    expect(topbar).toContain('CompatibilitySettingsDialog')
    expect(topbar).toContain('openCompatibilitySettings')
  })
})
