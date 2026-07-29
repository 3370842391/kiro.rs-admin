import { describe, expect, test } from 'bun:test'
import type {
  SupplierConfigUpdate,
  SupplierEntryView,
  SupplierEvent,
  SupplierEventPage,
} from '@/types/api'
import {
  buildSupplierConfigPayload,
  buildSupplierEntryPayload,
  emptySupplierEntry,
  getSupplierEventStatusLabel,
  getSupplierKindLabel,
  hasUnreadSupplierEvents,
  isValidSupplierId,
  suggestSupplierId,
  toSupplierEntryUpdate,
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
    for (const kind of ['kiro-rs', 'kiro-app'] as const) {
      const draft = emptySupplierEntry(kind)
      expect(draft.kind).toBe(kind)
      expect(draft.enabled).toBe(true)
      expect(draft.autoPurchase).toBe(true)
      expect(draft.minPurchase).toBeLessThanOrEqual(draft.maxPurchase)
    }
  })

  test('protocol labels name both vendors', () => {
    expect(getSupplierKindLabel('kiro-app')).toContain('kiroapp')
    expect(getSupplierKindLabel('kiro-rs')).toContain('kiro.rs')
  })
})
