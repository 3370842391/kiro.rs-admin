import { describe, expect, test } from 'bun:test'
import type { SupplierConfigUpdate, SupplierEvent, SupplierEventPage } from '@/types/api'
import {
  buildSupplierConfigPayload,
  getSupplierEventStatusLabel,
  hasUnreadSupplierEvents,
} from './key-supplier'

const update: SupplierConfigUpdate = {
  baseUrl: 'https://supplier.example',
  publicBaseUrl: 'https://admin.example',
  autoPurchase: true,
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
})
