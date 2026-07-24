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

  test('logout clears the shared React Query cache before another administrator logs in', async () => {
    const app = await readSource('App.tsx')

    expect(app).toContain('useQueryClient')
    expect(app).toContain('queryClient.clear()')
    expect(app).toContain('storage.removeApiKey()')
    expect(app).toContain('["supplier-events", "header-unread"]')
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
    expect(page).toContain('生成中')
    expect(page).toContain('空闲')
  })

  test('page keeps supplier secrets write-only and never renders purchased key material', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('type="password"')
    expect(page).toContain('只写入')
    expect(page).not.toContain('result.keys')
    expect(page).not.toContain('item.keys')
    expect(page).not.toMatch(/purchased(?:Keys|Key|_keys)\s*[:.[]/)
  })

  test('page exposes a retryable configuration error and safe supplier summary', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('configQuery.isError')
    expect(page).toContain('extractErrorMessage(configQuery.error)')
    expect(page).toContain('configQuery.refetch')
    expect(page).toContain('apiKeyConfigured')
    expect(page).toContain('webhookTokenConfigured')
    expect(page).toContain('purchaseResultSummary')
  })

  test('page uses supplier eventId for notification baselines and displays safe event metadata', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('seenEventIds')
    expect(page).toContain('previousEvents.current === null')
    expect(page).toContain('seenEventIds.current.add(event.eventId)')
    expect(page).toContain('event.eventId')
    expect(page).toContain('event.purchaseOrderId')
    expect(page).not.toContain('#${event.id}')
  })

  test('page keeps numeric inputs as validated drafts and blocks invalid submits', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('numericDrafts')
    expect(page).toContain('parseSupplierNumberDraft')
    expect(page).toContain('purchaseCountDraft')
    expect(page).toContain('parsedPurchaseCount === null')
    expect(page).toContain('configNumbersValid')
    expect(page).not.toContain("updateField('minPurchase', Number(event.target.value))")
    expect(page).not.toContain('purchase.mutate(purchaseCount)')
  })
})
