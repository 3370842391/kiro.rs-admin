import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8').catch(() => '')
}

describe('key supplier management UI contract', () => {
  test('App lazy-loads the key supplier tab and shows its unread event badge', async () => {
    const app = await readSource('App.tsx')

    expect(app).toContain('key-supplier-page')
    expect(app).toContain('KeySupplierPage')
    expect(app).toContain('key: "supplier"')
    expect(app).toContain('h === "supplier"')
    expect(app).toContain('<KeySupplierPage')
    expect(app).toContain('listSupplierEvents')
    expect(app).toContain('refetchInterval: 5000')
    expect(app).toContain('unreadCount')
  })

  test('page provides configuration, purchase, webhook, event controls and polling', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('自动购买')
    expect(page).toContain('手动购买')
    expect(page).toContain('注册 Webhook')
    expect(page).toContain('测试 Webhook')
    expect(page).toContain('标记所选已读')
    expect(page).toContain('全部标记已读')
    expect(page).toContain('重试')
    expect(page).toContain('refetchInterval: 30000')
    expect(page).toContain('refetchInterval: 5000')
    expect(page).toContain('hasUnreadSupplierEvents')
    expect(page).toContain('profile')
    expect(page).toContain('stockMax')
  })

  test('page keeps supplier secrets write-only and never renders purchased key material', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('type="password"')
    expect(page).toContain('只写入')
    expect(page).not.toContain('result.keys')
    expect(page).not.toContain('item.keys')
    expect(page).not.toMatch(/purchased(?:Keys|Key|_keys)\s*[:.[]/)
  })
})
