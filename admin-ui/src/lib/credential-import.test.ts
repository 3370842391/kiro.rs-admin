import { describe, expect, test } from 'bun:test'
import { unwrapCredentialImportPayload } from './credential-import'


describe('unwrapCredentialImportPayload', () => {
  test('unwraps internal credentials array instead of treating it as one account', () => {
    const items = [{ email: 'user', refreshToken: 'refresh' }]

    expect(
      unwrapCredentialImportPayload({ version: 1, credentials: items }),
    ).toEqual(items)
  })

  test('keeps arrays and Kiro Account Manager accounts compatible', () => {
    const flat = [{ refreshToken: 'flat' }]
    const accounts = [{ credentials: { refreshToken: 'nested' } }]

    expect(unwrapCredentialImportPayload(flat)).toEqual(flat)
    expect(unwrapCredentialImportPayload({ accounts })).toEqual(accounts)
  })

  test('keeps flat and nested single credential objects compatible', () => {
    const flat = { refreshToken: 'flat' }
    const nested = { email: 'user', credentials: { refreshToken: 'nested' } }

    expect(unwrapCredentialImportPayload(flat)).toEqual([flat])
    expect(unwrapCredentialImportPayload(nested)).toEqual([nested])
  })

  test('rejects unrelated objects and scalar JSON', () => {
    expect(() => unwrapCredentialImportPayload({ version: 1 })).toThrow(
      '无法识别',
    )
    expect(() => unwrapCredentialImportPayload('invalid')).toThrow('无法识别')
  })
})
