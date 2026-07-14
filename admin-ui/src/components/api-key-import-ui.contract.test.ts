import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('API Key import UI wiring', () => {
  test('batch dialog provides JSON/KAM and API Key text modes', async () => {
    const dialog = await readSource('src/components/batch-import-dialog.tsx')

    expect(dialog).toContain("type ImportMode = 'json' | 'api-key'")
    expect(dialog).toContain('JSON / KAM')
    expect(dialog).toContain('API Key 文本')
    expect(dialog).toContain('parseApiKeyLines(apiKeyInput, batchApiRegion || undefined)')
    expect(dialog).toContain("authRegion: 'us-east-1'")
    expect(dialog).toContain('nickname: entry.nickname')
    expect(dialog).toContain('apiRegion: entry.apiRegion')
  })

  test('batch text preview exposes only masked keys and accessible row errors', async () => {
    const dialog = await readSource('src/components/batch-import-dialog.tsx')

    expect(dialog).toContain('entry.maskedApiKey')
    expect(dialog).toContain('error.maskedApiKey')
    expect(dialog).toContain('aria-live="polite"')
    expect(dialog).toContain('行号')
    expect(dialog).toContain('Nickname')
    expect(dialog).toContain('API Region')
    expect(dialog).not.toMatch(/预览[\s\S]{0,200}entry\.kiroApiKey/)
  })

  test('single add fixes API Key auth region and requires a supported API region', async () => {
    const dialog = await readSource('src/components/add-credential-dialog.tsx')

    expect(dialog).toContain("const [nickname, setNickname] = useState('')")
    expect(dialog).toContain("authRegion: isApiKey ? 'us-east-1'")
    expect(dialog).toContain('nickname: isApiKey ? nickname.trim()')
    expect(dialog).toContain("if (!apiRegion) {")
    expect(dialog).toContain('<SelectItem value="us-east-1">')
    expect(dialog).toContain('<SelectItem value="eu-central-1">')
    expect(dialog).toMatch(/id="authRegion"[\s\S]*value="us-east-1"[\s\S]*readOnly/)
    expect(dialog).toContain('redactApiKeys(data.message)')
    expect(dialog).toContain('redactApiKeys(extractErrorMessage(error))')
    expect(dialog).toMatch(/id="nickname"[\s\S]*maxLength=\{128\}/)
  })

  test('single API Key add routes directly to batch text import mode', async () => {
    const addDialog = await readSource('src/components/add-credential-dialog.tsx')
    const batchDialog = await readSource('src/components/batch-import-dialog.tsx')
    const dashboard = await readSource('src/components/dashboard.tsx')

    expect(addDialog).toContain('onBatchApiKeyImport')
    expect(addDialog).toContain('批量添加 API Key')
    expect(batchDialog).toContain('initialMode?: ImportMode')
    expect(batchDialog).toContain("initialMode = 'json'")
    expect(dashboard).toContain('initialMode={batchImportInitialMode}')
    expect(dashboard).toContain('openBatchImport("api-key")')
    expect(dashboard).toContain('批量导入凭据 / API Key / KAM')
  })

  test('API Key edit can repair region and edit a bounded nickname', async () => {
    const dialog = await readSource('src/components/edit-credential-dialog.tsx')
    const types = await readSource('src/types/api.ts')

    expect(dialog).toContain("credential.authMethod === 'api_key'")
    expect(dialog).toMatch(/id="nickname"[\s\S]*maxLength=\{128\}/)
    expect(dialog).toMatch(/id="editAuthRegion"[\s\S]*value="us-east-1"[\s\S]*readOnly/)
    expect(dialog).toContain('<SelectItem value="us-east-1">')
    expect(dialog).toContain('<SelectItem value="eu-central-1">')
    expect(dialog).toContain('apiRegion: isApiKey ? apiRegion')
    expect(dialog).toContain('nickname: nickname.trim()')
    expect(types).toContain('nickname?: string')
    expect(types).toContain('apiRegion?: string')
  })

  test('credential cards prefer nickname and show auth/API regions', async () => {
    const card = await readSource('src/components/credential-card.tsx')

    expect(card).toContain('credential.nickname?.trim() ||')
    expect(card).toContain('credential.authRegion')
    expect(card).toContain('credential.apiRegion')
    expect(card).toContain('Auth Region')
    expect(card).toContain('API Region')
  })

  test('available models dialog supports optional routing diagnostics', async () => {
    const types = await readSource('src/types/api.ts')
    const dialog = await readSource('src/components/available-models-dialog.tsx')

    expect(types).toContain('nickname?: string')
    expect(types).toContain('authRegion?: string')
    expect(types).toContain('apiRegion?: string')
    expect(types).toContain('resolvedApiRegion?: string')
    expect(types).toContain('resolvedHost?: string')
    expect(types).toContain('kiroVersion?: string')
    expect(dialog).toContain('data.resolvedApiRegion')
    expect(dialog).toContain('data.resolvedHost')
    expect(dialog).toContain('data.kiroVersion')
  })
})
