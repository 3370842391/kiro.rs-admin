import { describe, expect, test } from 'bun:test'

async function readApiTypes(): Promise<string> {
  return Bun.file(new URL('../types/api.ts', import.meta.url)).text()
}

function interfaceBody(source: string, name: string): string {
  const start = source.indexOf(`export interface ${name} {`)
  const nextExport = source.indexOf('\nexport ', start + 1)
  return source.slice(start, nextExport === -1 ? source.length : nextExport)
}

describe('key supplier API security contracts', () => {
  test('supplier response contracts do not expose plaintext key fields', async () => {
    const source = await readApiTypes()
    const responseContracts = ['SupplierConfigView', 'SupplierOverview', 'SupplierEvent', 'SupplierEventPage', 'PurchaseResponse']

    for (const name of responseContracts) {
      const body = interfaceBody(source, name)
      expect(body).toContain(`export interface ${name}`)
      expect(body).not.toMatch(/^\s*(?:keys|purchasedKeys|purchasedKey|keyValues)\s*[?:]/m)
      expect(body).not.toMatch(
        /^\s*(?:apiKey|supplierApiKey|api_key|supplier_api_key|webhookToken|webhook_token)\s*[?:]/m,
      )
    }

    expect(interfaceBody(source, 'SupplierConfigView')).toMatch(/apiKeyConfigured: boolean/)
    expect(interfaceBody(source, 'SupplierConfigView')).toMatch(/webhookTokenConfigured: boolean/)
    expect(interfaceBody(source, 'SupplierOverview')).toMatch(/stockMax: number/)
    expect(interfaceBody(source, 'PurchaseResponse')).toMatch(/purchased: number/)
  })
})
