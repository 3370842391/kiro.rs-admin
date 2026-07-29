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
    expect(page).toContain('收到新 Key 就绪 Webhook 后自动发起一次购买')
    expect(page).toContain('单次最小购买量')
    expect(page).toContain('单次最大购买量')
    expect(page).not.toContain('最小库存')
    expect(page).not.toContain('最大库存')
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
    expect(page).toContain('webhookRegistered')
    expect(page).toContain('Webhook 已注册')
    expect(page).toContain('自动采购 RPM 预设')
    expect(page).toContain('useGroupOptions')
    expect(page).toContain('GroupMultiSelect')
    expect(page).toContain('autoDeleteForbidden')
    expect(page).toContain('403 时自动删除')
    expect(page).not.toContain('Groups（逗号分隔）')
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

  test('page manages multiple suppliers and scopes overview, purchase and events per supplier', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('listSuppliers')
    expect(page).toContain('createSupplier')
    expect(page).toContain('updateSupplier')
    expect(page).toContain('deleteSupplier')
    expect(page).toContain('添加供货商')
    expect(page).toContain('getSupplierKindLabel')
    expect(page).toContain('selectedId')
    // 概览/购买/webhook 都必须打到选中的那家，不能再走单供货商老路由。
    expect(page).toContain('getSupplierEntryOverview')
    expect(page).toContain('purchaseFromSupplier')
    expect(page).toContain('registerSupplierEntryWebhook')
    expect(page).toContain('testSupplierEntryWebhook')
    expect(page).toContain('supplierId: eventSupplierId')
    expect(page).toContain('只看当前供货商')
    expect(page).toContain('event.supplierId')
    // 删除要有确认，避免误删整家配置。
    expect(page).toContain('window.confirm')
  })

  test('page hides remote webhook registration for protocols that cannot register', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('supportsWebhookRegistration')
    expect(page).toContain("config?.kind === 'kiro-rs'")
    expect(page).toContain('getSupplierCallbackUrl')
    expect(page).toContain('获取回调地址')
    expect(page).toContain('需手动填写')
    expect(page).toContain('到货通知')
    // kiro-app 的价格与积分要能看到，否则无从判断该不该采购。
    expect(page).toContain('keyPrice')
    expect(page).toContain('balance')
    expect(page).toContain('剩余积分')
  })

  test('page exposes the webhook signing secret and its verification state', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('webhookSecret')
    expect(page).toContain('Webhook 签名密钥')
    expect(page).toContain('X-Kiro-Signature')
    expect(page).toContain('webhookSecretConfigured')
    expect(page).toContain('留空则不验签')
    expect(page).toContain('验签')
  })

  test('page warns that duplicate pushes never buy twice and surfaces the duplicate counter', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('同一条推送重复到达不会重复购买')
    expect(page).toContain('event.webhookDuplicateCount')
  })

  test('page validates supplier ids before creating and locks them afterwards', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('isValidSupplierId')
    expect(page).toContain('suggestSupplierId')
    expect(page).toContain('创建后不可改')
    expect(page).toContain('readOnly={!creating}')
  })

  test('page keeps numeric inputs as validated drafts and blocks invalid submits', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('numericDrafts')
    expect(page).toContain('parseSupplierNumberDraft')
    expect(page).toContain('purchaseCountDraft')
    expect(page).toContain('setPurchaseCountDraft(String(next.minPurchase))')
    expect(page).toContain('min={config?.minPurchase ?? 1}')
    expect(page).toContain('max={config?.maxPurchase}')
    expect(page).toContain('purchaseCountValid')
    expect(page).toContain('configNumbersValid')
    expect(page).not.toContain("updateField('minPurchase', Number(event.target.value))")
    expect(page).not.toContain('purchase.mutate(purchaseCount)')
  })
})
