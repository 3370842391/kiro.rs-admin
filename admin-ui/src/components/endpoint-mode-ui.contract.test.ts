import { expect, test } from 'bun:test'

test('endpoint mode UI exposes best-mode API and the four-endpoint chain', async () => {
  const api = await Bun.file('src/api/credentials.ts').text()
  const hooks = await Bun.file('src/hooks/use-credentials.ts').text()
  const dialog = await Bun.file('src/components/endpoint-chains-dialog.tsx').text()

  expect(api).toContain("'/config/endpoint-mode'")
  expect(api).toContain('EndpointModeConfig')
  expect(hooks).toContain("queryKey: ['endpointMode']")
  expect(dialog).toContain('默认最好模式')
  expect(dialog).toContain('Kiro Runtime')
  expect(dialog).toContain('codewhisperer')
  expect(dialog).toContain('amazonq')
})
