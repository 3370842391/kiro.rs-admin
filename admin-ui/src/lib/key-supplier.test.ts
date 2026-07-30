import { describe, expect, test } from 'bun:test'
import type {
  SupplierConfigUpdate,
  SupplierEntryView,
  SupplierEvent,
  SupplierEventPage,
  SupplierPoolStatus,
} from '@/types/api'
import {
  buildSupplierConfigPayload,
  buildSupplierEntryPayload,
  emptySupplierEntry,
  emptySupplierPool,
  getSupplierEventStatusLabel,
  getSupplierKindLabel,
  hasUnreadSupplierEvents,
  isValidSupplierId,
  suggestSupplierId,
  toSupplierEntryUpdate,
  validateSupplierPool,
} from './key-supplier'
import * as keySupplier from './key-supplier'

const update: SupplierConfigUpdate = {
  baseUrl: 'https://supplier.example',
  publicBaseUrl: 'https://admin.example',
  autoPurchase: true,
  autoDeleteForbidden: true,
  minPurchase: 1,
  maxPurchase: 5,
  apiRegion: 'us-east-1',
  rpmLimit: 100,
  priority: 10,
  groups: ['production'],
  sourceChannel: 'webhook',
  nicknamePrefix: 'supplier-',
  apiKey: '  supplier-secret  ',
  webhookToken: '  webhook-token  ',
}

function event(id: number, readAt: string | null = null): SupplierEvent {
  return {
    id,
    supplierId: 'default',
    eventId: `event-${id}`,
    eventType: 'purchase.requested',
    purchaseOrderId: null,
    supplierBatchId: null,
    message: null,
    quantity: 1,
    receivedAt: '2026-07-24T00:00:00Z',
    status: 'received',
    attempts: 0,
    lastError: null,
    purchasedCount: 0,
    importedCount: 0,
    duplicateCount: 0,
    webhookDuplicateCount: 0,
    failedCount: 0,
    readAt,
    totalDebit: null,
    unitPrice: null,
    supplierOrderId: null,
    replayed: false,
  }
}

function page(items: SupplierEvent[]): SupplierEventPage {
  return { items, unreadCount: items.filter((item) => item.readAt === null).length }
}

describe('key supplier helpers', () => {
  test('omits blank secrets and preserves the stored secret on update', () => {
    const draft = { ...update, apiKey: '   ', webhookToken: '\t' }
    const payload = buildSupplierConfigPayload(draft)

    expect(payload).toEqual({
      baseUrl: 'https://supplier.example',
      publicBaseUrl: 'https://admin.example',
      autoPurchase: true,
      autoDeleteForbidden: true,
      minPurchase: 1,
      maxPurchase: 5,
      apiRegion: 'us-east-1',
      rpmLimit: 100,
      priority: 10,
      groups: ['production'],
      sourceChannel: 'webhook',
      nicknamePrefix: 'supplier-',
    })
  })

  test('trims non-blank secrets without mutating the input', () => {
    const original = structuredClone(update)

    expect(buildSupplierConfigPayload(update)).toEqual({
      ...original,
      apiKey: 'supplier-secret',
      webhookToken: 'webhook-token',
    })
    expect(update).toEqual(original)
  })

  test('labels every supplier event status', () => {
    expect(getSupplierEventStatusLabel('received')).toBe('已接收')
    expect(getSupplierEventStatusLabel('processing')).toBe('处理中')
    expect(getSupplierEventStatusLabel('succeeded')).toBe('成功')
    expect(getSupplierEventStatusLabel('skipped')).toBe('已跳过')
    expect(getSupplierEventStatusLabel('failed')).toBe('失败')
  })

  test('does not treat the initial snapshot as a new unread event', () => {
    expect(hasUnreadSupplierEvents(null, page([event(1)]))).toBe(false)
  })

  test('detects a new unread event but ignores read and duplicate events', () => {
    const previous = page([event(1), event(2)])

    expect(hasUnreadSupplierEvents(previous, page([event(3), event(2, '2026-07-24T00:01:00Z')]))).toBe(
      true,
    )
    expect(hasUnreadSupplierEvents(previous, page([event(2, '2026-07-24T00:01:00Z')]))).toBe(false)
    expect(hasUnreadSupplierEvents(previous, page([event(1)]))).toBe(false)
  })

  test('identifies new unread events by supplier eventId instead of database id', () => {
    const previous = page([event(7)])
    const repeatedDatabaseId = { ...event(7), eventId: 'supplier-event-replayed' }

    expect(hasUnreadSupplierEvents(previous, page([repeatedDatabaseId]))).toBe(true)
  })

  test('parses only finite non-negative integer supplier number drafts', () => {
    const parse = (keySupplier as typeof keySupplier & {
      parseSupplierNumberDraft?: (value: string, minimum: number) => number | null
    }).parseSupplierNumberDraft

    expect(parse).toBeDefined()
    if (!parse) return
    expect(parse('', 0)).toBeNull()
    expect(parse('NaN', 0)).toBeNull()
    expect(parse('Infinity', 0)).toBeNull()
    expect(parse('-1', 0)).toBeNull()
    expect(parse('1.5', 0)).toBeNull()
    expect(parse('0', 0)).toBe(0)
    expect(parse('3', 1)).toBe(3)
  })
})

describe('global key pool helpers', () => {
  test('new pool drafts are off so upgrading changes nothing', () => {
    const draft = emptySupplierPool()

    expect(draft.enabled).toBe(false)
    // 0 是「未配置」哨兵，不是业务默认值。
    expect(draft.targetCount).toBe(0)
    expect(draft.lowQuotaThreshold).toBe(0)
    expect(validateSupplierPool(draft)).toBeNull()
  })

  test('enabling the pool requires an explicit target count', () => {
    // 放过 0 会让人以为限住了采购，实际上每次到货都被跳过。
    expect(validateSupplierPool({ enabled: true, targetCount: 0, lowQuotaThreshold: 0 })).toContain(
      '1..=10000',
    )
    expect(validateSupplierPool({ enabled: true, targetCount: 1, lowQuotaThreshold: 0 })).toBeNull()
    expect(
      validateSupplierPool({ enabled: true, targetCount: 10000, lowQuotaThreshold: 0 }),
    ).toBeNull()
    expect(
      validateSupplierPool({ enabled: true, targetCount: 10001, lowQuotaThreshold: 0 }),
    ).toContain('1..=10000')
  })

  test('a disabled pool tolerates a stale target so the feature can be turned off', () => {
    // 关闭状态下的旧值不参与任何判定，拦住保存会把人困在「想关都关不掉」。
    expect(validateSupplierPool({ enabled: false, targetCount: 0, lowQuotaThreshold: 0 })).toBeNull()
    expect(validateSupplierPool({ enabled: false, targetCount: 7, lowQuotaThreshold: 0 })).toBeNull()
    // 但越界仍然拒绝：那是手滑，不是「未配置」。
    expect(
      validateSupplierPool({ enabled: false, targetCount: 10001, lowQuotaThreshold: 0 }),
    ).toContain('0..=10000')
  })

  test('low quota threshold is range checked independently of the enable switch', () => {
    expect(
      validateSupplierPool({ enabled: false, targetCount: 0, lowQuotaThreshold: 100000 }),
    ).toBeNull()
    expect(
      validateSupplierPool({ enabled: false, targetCount: 0, lowQuotaThreshold: 100001 }),
    ).toContain('0..=100000')
    expect(
      validateSupplierPool({ enabled: true, targetCount: 3, lowQuotaThreshold: -1 }),
    ).toContain('0..=100000')
  })

  test('pool status carries everything needed to explain a low usable count', () => {
    // 界面要能回答三个问题，缺一个就得去翻日志。
    const status: SupplierPoolStatus = {
      enabled: true,
      targetCount: 5,
      lowQuotaThreshold: 0,
      globalUsable: 2,
      deficit: 3,
      health: { total: 6, usable: 2, dead: 3, quotaExhausted: 1, lowQuota: 0 },
      bySupplierId: 4,
      byLegacyChannel: 2,
      matchedChannels: ['Webhook 自动采购'],
    }

    // 「池里 6 个号怎么可用数只有 2」
    expect(status.health.dead + status.health.quotaExhausted + status.health.lowQuota).toBe(
      status.health.total - status.health.usable,
    )
    // 「还差几个」
    expect(status.deficit).toBe(status.targetCount - status.globalUsable)
    // 「备注匹配还生效吗」——两类识别计数之和等于池里的号数
    expect(status.bySupplierId + status.byLegacyChannel).toBe(status.health.total)
    // 「我买的号怎么没算进去」
    expect(status.matchedChannels).toContain('Webhook 自动采购')
  })
})

describe('multi-supplier helpers', () => {
  const entry: SupplierEntryView = {
    id: 'kiroapp',
    name: 'kiroapp.cc',
    kind: 'kiro-app',
    enabled: true,
    supportsWebhookRegistration: false,
    baseUrl: 'https://kiroapp.cc',
    publicBaseUrl: 'https://admin.example',
    autoPurchase: true,
    autoDeleteForbidden: false,
    minPurchase: 1,
    maxPurchase: 5,
    apiRegion: 'us-east-1',
    rpmLimit: 10,
    priority: 0,
    groups: ['production'],
    sourceChannel: 'webhook',
    nicknamePrefix: 'auto-',
    apiKeyConfigured: true,
    webhookTokenConfigured: true,
    webhookSecretConfigured: true,
  }

  test('entry payload keeps secrets write-only and trims identity fields', () => {
    const payload = buildSupplierEntryPayload({
      ...toSupplierEntryUpdate(entry),
      id: '  kiroapp  ',
      name: '  kiroapp.cc  ',
      apiKey: '  fresh-secret  ',
    })

    expect(payload.id).toBe('kiroapp')
    expect(payload.name).toBe('kiroapp.cc')
    expect(payload.kind).toBe('kiro-app')
    expect(payload.enabled).toBe(true)
    expect(payload.apiKey).toBe('fresh-secret')
    // 留空的 secret 必须整个字段不出现，否则会把服务端已存的值清空。
    expect('webhookToken' in payload).toBe(false)
    expect('webhookSecret' in payload).toBe(false)
  })

  test('webhook signing secret is write-only and trimmed', () => {
    const payload = buildSupplierEntryPayload({
      ...toSupplierEntryUpdate(entry),
      webhookSecret: '  hook-secret  ',
    })

    expect(payload.webhookSecret).toBe('hook-secret')
  })

  test('blank secrets and blank id are omitted from the payload', () => {
    const payload = buildSupplierEntryPayload({
      ...toSupplierEntryUpdate(entry),
      id: '   ',
      apiKey: '   ',
      webhookToken: '',
      webhookSecret: '   ',
    })

    expect('apiKey' in payload).toBe(false)
    expect('webhookToken' in payload).toBe(false)
    expect('webhookSecret' in payload).toBe(false)
    expect('id' in payload).toBe(false)
  })

  test('round-trips a view into an editable update without sharing the groups array', () => {
    const update = toSupplierEntryUpdate(entry)

    expect(update).toMatchObject({ id: 'kiroapp', name: 'kiroapp.cc', kind: 'kiro-app', enabled: true })
    expect(update.groups).toEqual(['production'])
    update.groups.push('mutated')
    expect(entry.groups).toEqual(['production'])
  })

  test('supplier ids accept url-safe values only', () => {
    for (const valid of ['default', 'kiroapp', 'kiro-app-2', 'vendor_1', 'A1']) {
      expect(isValidSupplierId(valid)).toBe(true)
    }
    for (const invalid of ['', '   ', 'has space', 'slash/es', 'dots.', '中文', 'a'.repeat(65)]) {
      expect(isValidSupplierId(invalid)).toBe(false)
    }
  })

  test('suggests a url-safe id and avoids collisions', () => {
    expect(suggestSupplierId('KiroApp.cc 主号', [])).toBe('kiroapp-cc')
    expect(suggestSupplierId('kiroapp', ['kiroapp'])).toBe('kiroapp-2')
    expect(suggestSupplierId('kiroapp', ['kiroapp', 'kiroapp-2'])).toBe('kiroapp-3')
    // 纯符号名字也要退化出一个可用 id。
    expect(suggestSupplierId('###', [])).toBe('supplier')
    expect(isValidSupplierId(suggestSupplierId('中文名字', []))).toBe(true)
  })

  test('new supplier drafts prefill the vendor base url per protocol', () => {
    expect(emptySupplierEntry('kiro-app').baseUrl).toBe('https://kiroapp.cc')
    expect(emptySupplierEntry('kiro-rs').baseUrl).toBe('')
    for (const kind of ['kiro-rs', 'kiro-app', 'kiroapp-io'] as const) {
      const draft = emptySupplierEntry(kind)
      expect(draft.kind).toBe(kind)
      expect(draft.enabled).toBe(true)
      expect(draft.autoPurchase).toBe(true)
      expect(draft.minPurchase).toBeLessThanOrEqual(draft.maxPurchase)
    }
  })

  test('kiroapp.io drafts default to https so the km_ token is not sent in the clear', () => {
    // 对方文档写的是 http://kiroapp.io，但明文 HTTP 会把 km_ 令牌和 ksk_ 暴露在链路上。
    expect(emptySupplierEntry('kiroapp-io').baseUrl).toBe('https://kiroapp.io')
  })

  test('protocol labels name every vendor', () => {
    expect(getSupplierKindLabel('kiro-app')).toContain('kiroapp')
    expect(getSupplierKindLabel('kiro-rs')).toContain('kiro.rs')
    expect(getSupplierKindLabel('kiroapp-io')).toContain('kiroapp.io')
    // 两家 kiroapp 的标签必须能区分，否则下拉框里认不出选了哪家。
    expect(getSupplierKindLabel('kiroapp-io')).not.toBe(getSupplierKindLabel('kiro-app'))
  })

  test('new drafts enable the restock gate so a chatty supplier cannot bill on every arrival', () => {
    const draft = emptySupplierEntry('kiroapp-io')

    expect(draft.restockOnlyWhenExhausted).toBe(true)
    // 0 = 一个能用的都没有了才买。
    expect(draft.restockUsableThreshold).toBe(0)
    expect(draft.lowQuotaThreshold).toBe(0)
  })

  test('the restock gate knobs survive the payload round-trip', () => {
    const payload = buildSupplierEntryPayload({
      ...emptySupplierEntry('kiroapp-io'),
      id: 'io',
      name: 'io',
      restockOnlyWhenExhausted: true,
      restockUsableThreshold: 2,
      lowQuotaThreshold: 500,
    })

    // 水位必须真的发出去——丢了就是「额度水位静默失效」。
    expect(payload.restockOnlyWhenExhausted).toBe(true)
    expect(payload.restockUsableThreshold).toBe(2)
    expect(payload.lowQuotaThreshold).toBe(500)
  })

  test('kiroapp.io round-trips through the entry payload builder', () => {
    const payload = buildSupplierEntryPayload({
      ...emptySupplierEntry('kiroapp-io'),
      id: 'kiroapp-io',
      name: ' kiroapp.io ',
    })

    expect(payload.kind).toBe('kiroapp-io')
    expect(payload.id).toBe('kiroapp-io')
    expect(payload.name).toBe('kiroapp.io')
  })
})
