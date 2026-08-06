import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8').catch(() => '')
}

/** 读 Rust 侧源码，用于跨语言比对能力表。 */
async function readRustSource(path: string): Promise<string> {
  return readFile(new URL(`../../../${path}`, import.meta.url), 'utf8').catch(() => '')
}

/** 从 capabilities.rs 解析出某个供货商的 region_modes（后端是权威）。 */
function backendRegionModes(rust: string, kind: string): string[] {
  // const XXX_REGION_MODES: &[PurchaseRegionMode] = &[ Fixed, Webhook, ... ];
  const constants = new Map<string, string[]>()
  for (const match of rust.matchAll(
    /const\s+(\w+_REGION_MODES|OMIT_ONLY)\s*:\s*&\[PurchaseRegionMode\]\s*=\s*&\[([^\]]*)\]/g,
  )) {
    const modes = [...match[2].matchAll(/PurchaseRegionMode::(\w+)/g)].map(
      (m) => m[1][0].toLowerCase() + m[1].slice(1),
    )
    constants.set(match[1], modes)
  }
  // SupplierKind::KiroDrop => Self { region_modes: DROP_REGION_MODES,
  // 显式映射：'kiroapp-io' 的 Rust 变体是 KiroAppIo，机械大小写转换得不出来。
  const variants: Record<string, string> = {
    'kiro-rs': 'KiroRs',
    'kiro-app': 'KiroApp',
    'kiroapp-io': 'KiroAppIo',
    'kiro-drop': 'KiroDrop',
    'kiro-ceo': 'KiroCeo',
  }
  const variant = variants[kind]
  if (!variant) return []
  const arm = new RegExp(
    `SupplierKind::${variant}\\s*=>\\s*Self\\s*\\{[^}]*?region_modes:\\s*(\\w+)`,
  ).exec(rust)
  if (!arm) return []
  return constants.get(arm[1]) ?? []
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
    expect(page).toContain('capabilities.supportsWebhookRegistration')
    expect(page).not.toContain("config?.kind === 'kiro-rs'")
    expect(page).toContain('getSupplierCallbackUrl')
    expect(page).toContain('获取回调地址')
    expect(page).toContain('需手动填写')
    expect(page).toContain('到货通知')
    // kiro-app 的价格与积分要能看到，否则无从判断该不该采购。
    expect(page).toContain('keyPrice')
    expect(page).toContain('balance')
    expect(page).toContain('剩余积分')
  })

  test('page cannot run webhook actions against the previous supplier while creating', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('registerWebhook.isPending || creating || selectedId === null')
    expect(page).toContain('testWebhook.isPending || creating || selectedId === null')
    expect(page).toContain('callbackUrlQuery.isPending || creating || selectedId === null')
  })

  test('page offers every protocol and shows the tiered price range', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain("'kiroapp-io'")
    // 漏掉一个协议就等于那家供货商在界面上加不进来。
    expect(page).toContain("'kiro-drop'")
    expect(page).toContain("'kiro-ceo'")
    // 阶梯定价必须显示区间：只显示一个数会让人以为总价能预估。
    expect(page).toContain('keyPriceMax')
    expect(page).toContain('单价区间')
    expect(page).toContain('formatKeyPrice')
    // kiroapp.io 没有签名头，别把用户送去找一个不存在的密钥。
    expect(page).toContain('Webhook 配置')
  })

  test('page exposes the restock gate and the pool health that explains its decision', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('restockOnlyWhenExhausted')
    expect(page).toContain('targetUsable')
    expect(page).toContain('lowQuotaThreshold')
    expect(page).toContain('仅在号不够用时补货')
    // 「为什么没买」必须能在界面上看出来，而不是只能翻日志：
    // 额度明显快干了但「额度低」一栏还是 0，就说明水位没配或余额源没接上。
    expect(page).toContain('credentialHealth')
    expect(page).toContain('quotaExhausted')
    expect(page).toContain('不可用构成')
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

  test('pool card explains the stock-target semantics rather than implying a per-arrival cap', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('全局号池')
    // 语义必须写清是「存量」而不是「每次买几个」，否则用户会按后者理解并配错。
    expect(page).toContain('目标存量')
    expect(page).toContain('缺口')
    expect(page).toContain('谁先推来谁先拿到缺口')
    expect(page).toContain('getSupplierPoolStatus')
    expect(page).toContain('validateSupplierPool')
  })

  test('pool card surfaces the four-way health split so a low usable count is explainable', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    // 「池里 10 个号怎么可用数只有 3」——答案必须在界面上，不能只在日志里。
    expect(page).toContain('poolStatusQuery.data.health.dead')
    expect(page).toContain('poolStatusQuery.data.health.quotaExhausted')
    expect(page).toContain('poolStatusQuery.data.health.lowQuota')
    expect(page).toContain('已判死的号仍留在池子里')
  })

  test('pool card warns that editing sourceChannel drops legacy purchases from the watermark', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    // 备注匹配的已知弱点：改掉 sourceChannel 会让旧号静默不计入，缺口变大后重复采购。
    expect(page).toContain('byLegacyChannel')
    expect(page).toContain('只能靠「来源渠道」备注认出来')
    expect(page).toContain('不再计入水位')
    expect(page).toContain('matchedChannels')
  })

  test('per-supplier restock fields say they stop applying once the pool is on', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    // 两处水位并存最容易让人误判「为什么没买」，界面必须直说哪一处在生效。
    expect(page).toContain('不再参与判定')
    expect(page).toContain('仅作安全上限')
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

  test('page separates common import settings from per-supplier overrides', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('公共导入设置')
    expect(page).toContain('getSupplierCommon')
    expect(page).toContain('updateSupplierCommon')
    expect(page).toContain('importOverrides')
    expect(page).toContain('继承公共设置')
    expect(page).toContain('Nickname 预览')
  })

  // 前端有一份写死的能力表副本。它和 Rust 侧 capabilities.rs 走偏过一次：
  // 后端把 kiro-drop 从「仅 omit」改成支持三种模式，前端没跟着改，
  // 结果界面上压根不给选采购区域——配置能力有了却用不上。
  test('frontend capability table matches the Rust protocol matrix for every supplier', async () => {
    const helpers = await readSource('lib/key-supplier.ts')
    const rust = await readRustSource('src/admin/key_supplier/capabilities.rs')
    expect(rust).not.toBe('')

    for (const kind of ['kiro-rs', 'kiro-app', 'kiroapp-io', 'kiro-drop', 'kiro-ceo']) {
      const expected = backendRegionModes(rust, kind)
      expect(expected.length).toBeGreaterThan(0)
      // 从前端表里取该 kind 的 regionModes
      const block = new RegExp(
        `'${kind}':\\s*\\{[\\s\\S]*?regionModes:\\s*\\[([^\\]]*)\\]`,
      ).exec(helpers)
      expect(block).not.toBeNull()
      const actual = [...block![1].matchAll(/'([^']+)'/g)].map((m) => m[1])
      expect(actual.sort()).toEqual(expected.sort())
    }
  })

  test('page uses protocol capabilities for purchase region controls and defaults CEO to US', async () => {
    const page = await readSource('components/key-supplier-page.tsx')
    const helpers = await readSource('lib/key-supplier.ts')

    expect(page).toContain('purchaseRegionMode')
    expect(page).toContain('capabilities.regionModes')
    expect(page).toContain('采购区域')
    expect(page).toContain('凭据 API 区域兜底')
    expect(helpers).toContain("purchaseRegionMode: 'fixed'")
    expect(helpers).toContain("purchaseRegion: 'us'")
  })

  test('event rows expose the structured decision snapshot', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('decisionSnapshot')
    expect(page).toContain('判定详情')
    expect(page).toContain('creditedAtDecision')
    expect(page).toContain('requestedRegionSource')
  })

  test('callback URLs are scoped to the supplier that produced them', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('callbackUrlQuery.data?.supplierId === selectedId')
    expect(page).toContain('registerWebhook.data?.supplierId === selectedId')
    expect(page).not.toContain(
      'callbackUrlQuery.data?.callbackUrl ?? registerWebhook.data?.callbackUrl',
    )
  })

  test('editing a supplier derives region capabilities from the draft kind', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('config ? getSupplierCapabilities(config.kind)')
    expect(page).not.toContain('selectedEntry.capabilities\n    : config')
  })

  test('webhook actions keep using the saved supplier capabilities while editing', async () => {
    const page = await readSource('components/key-supplier-page.tsx')

    expect(page).toContain('selectedEntry?.capabilities.supportsWebhookRegistration')
    expect(page).not.toContain(
      'supportsWebhookRegistration = capabilities',
    )
  })
})
