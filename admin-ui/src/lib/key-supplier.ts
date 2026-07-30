import type {
  SupplierConfigPayload,
  SupplierConfigUpdate,
  SupplierEntryPayload,
  SupplierEntryUpdate,
  SupplierEntryView,
  SupplierEvent,
  SupplierEventPage,
  SupplierEventStatus,
  SupplierPoolConfig,
  SupplierKind,
} from '@/types/api'

export function buildSupplierConfigPayload(update: SupplierConfigUpdate): SupplierConfigPayload {
  const payload: SupplierConfigPayload = {
    baseUrl: update.baseUrl,
    publicBaseUrl: update.publicBaseUrl,
    autoPurchase: update.autoPurchase,
    autoDeleteForbidden: update.autoDeleteForbidden,
    minPurchase: update.minPurchase,
    maxPurchase: update.maxPurchase,
    apiRegion: update.apiRegion,
    rpmLimit: update.rpmLimit,
    priority: update.priority,
    groups: [...update.groups],
    sourceChannel: update.sourceChannel,
    nicknamePrefix: update.nicknamePrefix,
    restockOnlyWhenExhausted: update.restockOnlyWhenExhausted,
    restockUsableThreshold: update.restockUsableThreshold,
    lowQuotaThreshold: update.lowQuotaThreshold,
  }

  const apiKey = update.apiKey?.trim()
  if (apiKey) payload.apiKey = apiKey

  const webhookToken = update.webhookToken?.trim()
  if (webhookToken) payload.webhookToken = webhookToken

  const webhookSecret = update.webhookSecret?.trim()
  if (webhookSecret) payload.webhookSecret = webhookSecret

  return payload
}

/** Wraps the shared config payload so secrets keep their "blank means unchanged" semantics. */
export function buildSupplierEntryPayload(update: SupplierEntryUpdate): SupplierEntryPayload {
  const payload: SupplierEntryPayload = {
    ...buildSupplierConfigPayload(update),
    name: update.name.trim(),
    kind: update.kind,
    enabled: update.enabled,
  }

  const id = update.id?.trim()
  if (id) payload.id = id

  return payload
}

const supplierKindLabels: Record<SupplierKind, string> = {
  'kiro-rs': '号商（kiro.rs 协议）',
  'kiro-app': 'kiroapp.cc',
  'kiroapp-io': 'kiroapp.io',
}

/** Default base URL per protocol, so operators rarely have to type it. */
const supplierKindBaseUrls: Record<SupplierKind, string> = {
  'kiro-rs': '',
  'kiro-app': 'https://kiroapp.cc',
  // Their docs say `http://`, but the token and key travel in the clear over it.
  // Default to https and let the operator downgrade if the host really lacks TLS.
  'kiroapp-io': 'https://kiroapp.io',
}

export function getSupplierKindLabel(kind: SupplierKind): string {
  return supplierKindLabels[kind] ?? kind
}

/** Server rule: lowercase letters, digits, `-` and `_`; immutable once created. */
export function isValidSupplierId(value: string): boolean {
  const normalized = value.trim()
  return normalized.length > 0 && normalized.length <= 64 && /^[a-zA-Z0-9_-]+$/.test(normalized)
}

/** Turns a display name into a usable id so operators rarely have to type one. */
export function suggestSupplierId(name: string, taken: readonly string[]): string {
  const base =
    name
      .trim()
      .toLowerCase()
      .replace(/[^a-z0-9_-]+/g, '-')
      .replace(/^-+|-+$/g, '')
      .slice(0, 48) || 'supplier'
  if (!taken.includes(base)) return base

  for (let suffix = 2; suffix < 100; suffix += 1) {
    const candidate = `${base}-${suffix}`
    if (!taken.includes(candidate)) return candidate
  }
  return `${base}-${Date.now()}`
}

/** Blank config that the "add supplier" form starts from. */
export function emptySupplierEntry(kind: SupplierKind): SupplierEntryUpdate {
  return {
    id: '',
    name: '',
    kind,
    enabled: true,
    baseUrl: supplierKindBaseUrls[kind] ?? '',
    publicBaseUrl: '',
    autoPurchase: true,
    autoDeleteForbidden: false,
    minPurchase: 1,
    maxPurchase: 1,
    apiRegion: 'us-east-1',
    rpmLimit: 10,
    priority: 0,
    groups: [],
    sourceChannel: 'Webhook 自动采购',
    nicknamePrefix: '自动采购',
    // 新建默认开启补货闸：供货商不停推到货时，不加闸就是每次都掏钱。
    restockOnlyWhenExhausted: true,
    restockUsableThreshold: 0,
    lowQuotaThreshold: 0,
  }
}

export function toSupplierEntryUpdate(entry: SupplierEntryView): SupplierEntryUpdate {
  return {
    id: entry.id,
    name: entry.name,
    kind: entry.kind,
    enabled: entry.enabled,
    baseUrl: entry.baseUrl,
    publicBaseUrl: entry.publicBaseUrl,
    autoPurchase: entry.autoPurchase,
    autoDeleteForbidden: entry.autoDeleteForbidden,
    minPurchase: entry.minPurchase,
    maxPurchase: entry.maxPurchase,
    apiRegion: entry.apiRegion,
    rpmLimit: entry.rpmLimit,
    priority: entry.priority,
    groups: [...entry.groups],
    sourceChannel: entry.sourceChannel,
    nicknamePrefix: entry.nicknamePrefix,
    restockOnlyWhenExhausted: entry.restockOnlyWhenExhausted,
    restockUsableThreshold: entry.restockUsableThreshold,
    lowQuotaThreshold: entry.lowQuotaThreshold,
  }
}

export function parseSupplierNumberDraft(value: string, minimum: number): number | null {
  const normalized = value.trim()
  if (!normalized) return null

  const parsed = Number(normalized)
  return Number.isSafeInteger(parsed) && parsed >= minimum ? parsed : null
}

export const MAX_POOL_TARGET = 10_000
export const MAX_POOL_LOW_QUOTA_THRESHOLD = 100_000

/** Blank global pool config. Off by default so upgrading changes nothing. */
export function emptySupplierPool(): SupplierPoolConfig {
  return { enabled: false, targetCount: 0, lowQuotaThreshold: 0 }
}

/**
 * Why the pool form can be saved at all while disabled: a stale `targetCount` on a
 * disabled pool participates in no decision, so blocking the save would strand users
 * who just want to turn the feature off.
 *
 * Enabling, on the other hand, requires an explicit target. `0` is the "not configured"
 * sentinel — accepting it would let someone believe they capped purchasing when in fact
 * every arrival gets skipped (or worse, a default gets guessed and money is spent).
 */
export function validateSupplierPool(draft: SupplierPoolConfig): string | null {
  if (draft.lowQuotaThreshold < 0 || draft.lowQuotaThreshold > MAX_POOL_LOW_QUOTA_THRESHOLD) {
    return `额度水位必须在 0..=${MAX_POOL_LOW_QUOTA_THRESHOLD} 之间`
  }
  if (draft.enabled && (draft.targetCount < 1 || draft.targetCount > MAX_POOL_TARGET)) {
    return `启用号池时目标存量必须在 1..=${MAX_POOL_TARGET} 之间`
  }
  if (!draft.enabled && (draft.targetCount < 0 || draft.targetCount > MAX_POOL_TARGET)) {
    return `目标存量必须在 0..=${MAX_POOL_TARGET} 之间`
  }
  return null
}

const supplierEventStatusLabels: Record<SupplierEventStatus, string> = {
  received: '已接收',
  processing: '处理中',
  succeeded: '成功',
  skipped: '已跳过',
  failed: '失败',
}

export function getSupplierEventStatusLabel(status: SupplierEventStatus): string {
  return supplierEventStatusLabels[status]
}

type SupplierEventSnapshot = SupplierEventPage | readonly SupplierEvent[]

function getSupplierEvents(snapshot: SupplierEventSnapshot): readonly SupplierEvent[] {
  return 'items' in snapshot ? snapshot.items : snapshot
}

export function hasUnreadSupplierEvents(
  previous: SupplierEventSnapshot | null | undefined,
  current: SupplierEventSnapshot,
): boolean {
  if (!previous) return false

  const previousEventIds = new Set(getSupplierEvents(previous).map((event) => event.eventId))
  return getSupplierEvents(current).some(
    (event) => !previousEventIds.has(event.eventId) && event.readAt === null,
  )
}
