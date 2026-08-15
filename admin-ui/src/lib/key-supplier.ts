import type {
  PurchaseRegionMode,
  SupplierConfigPayload,
  SupplierConfigUpdate,
  SupplierCapabilities,
  SupplierCommonConfig,
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
    purchaseRegionMode: update.purchaseRegionMode,
    purchaseRegion: update.purchaseRegion,
    credentialApiRegionFallback: update.credentialApiRegionFallback,
    rpmLimit: update.rpmLimit,
    maxConcurrency: update.maxConcurrency,
    priority: update.priority,
    groups: [...update.groups],
    sourceChannel: update.sourceChannel,
    nicknamePrefix: update.nicknamePrefix,
    restockOnlyWhenExhausted: update.restockOnlyWhenExhausted,
    targetUsable: update.targetUsable,
    lowQuotaThreshold: update.lowQuotaThreshold,
    maxUnitPrice: update.maxUnitPrice,
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
    importOverrides: {
      ...update.importOverrides,
      ...(update.importOverrides.groups
        ? { groups: [...update.importOverrides.groups] }
        : {}),
    },
  }

  const id = update.id?.trim()
  if (id) payload.id = id

  return payload
}

const supplierKindLabels: Record<SupplierKind, string> = {
  'kiro-rs': '号商（kiro.rs 协议）',
  'kiro-app': 'kiroapp.cc',
  'kiroapp-io': 'kiroapp.io',
  'kiro-drop': 'Kiro Drop',
  'kiro-ceo': 'kiro.ceo',
}

/** Default base URL per protocol, so operators rarely have to type it. */
const supplierKindBaseUrls: Record<SupplierKind, string> = {
  'kiro-rs': '',
  'kiro-app': 'https://kiroapp.cc',
  // Their docs say `http://`, but the token and key travel in the clear over it.
  // Default to https and let the operator downgrade if the host really lacks TLS.
  'kiroapp-io': 'https://kiroapp.io',
  // Kiro Drop's docs only document the `/api` path, never the host — it differs per
  // deployment, so there is nothing safe to prefill.
  'kiro-drop': '',
  'kiro-ceo': 'https://kiro.ceo',
}

const supplierKindCapabilities: Record<SupplierKind, SupplierCapabilities> = {
  'kiro-rs': {
    regionModes: ['omit'],
    supportsWebhookRegistration: true,
    purchaseIsIdempotent: true,
    supportsPrice: false,
  },
  'kiro-app': {
    regionModes: ['omit'],
    supportsWebhookRegistration: false,
    purchaseIsIdempotent: false,
    supportsPrice: true,
  },
  'kiroapp-io': {
    regionModes: ['fixed', 'batch'],
    supportsWebhookRegistration: false,
    purchaseIsIdempotent: true,
    supportsPrice: true,
  },
  'kiro-drop': {
    // 购买接口接受 region（us / eu / us-east-1 / eu-central-1），webhook 也带区域，
    // 且缺货时客户端会自动改打另一个区。与 src/admin/key_supplier/capabilities.rs
    // 的 DROP_REGION_MODES 保持一致——两边不一致的话界面上就不给选区。
    regionModes: ['fixed', 'webhook', 'bestAvailable'],
    supportsWebhookRegistration: true,
    purchaseIsIdempotent: true,
    supportsPrice: true,
  },
  'kiro-ceo': {
    regionModes: ['fixed', 'webhook', 'bestAvailable'],
    supportsWebhookRegistration: true,
    purchaseIsIdempotent: true,
    supportsPrice: true,
  },
}

const supplierKindRegionDefaults: Record<
  SupplierKind,
  Pick<SupplierEntryUpdate, 'purchaseRegionMode' | 'purchaseRegion'>
> = {
  'kiro-rs': { purchaseRegionMode: 'omit', purchaseRegion: null },
  'kiro-app': { purchaseRegionMode: 'omit', purchaseRegion: null },
  'kiroapp-io': { purchaseRegionMode: 'batch', purchaseRegion: null },
  // 默认 bestAvailable：先打对方默认区（美区），明确判定缺货再自动改打欧区。
  // 与后端 default_purchase_region_mode 一致。
  'kiro-drop': { purchaseRegionMode: 'bestAvailable', purchaseRegion: null },
  'kiro-ceo': { purchaseRegionMode: 'fixed', purchaseRegion: 'us' },
}

const defaultSupplierCommon: SupplierCommonConfig = {
  sourceChannel: 'Webhook 自动采购',
  nicknameLabel: '',
  rpmLimit: 10,
  maxConcurrency: 0,
  priority: 0,
  groups: [],
  autoDeleteForbidden: false,
}

export function getSupplierKindLabel(kind: SupplierKind): string {
  return supplierKindLabels[kind] ?? kind
}

/**
 * 区域模式下拉要显示的选项：本家支持的模式 + 当前已持久化的模式（若已不在支持列表里）。
 *
 * 原生 `<select>` 的 `value` 找不到对应 `<option>` 时，浏览器会显示第一项，
 * 而 React state 仍是原值——界面显示「固定区域」，实际配置却是 `omit`，
 * 于是「采购区域」子选择器（只在 mode==='fixed' 时出现）也不显示，用户直接卡住。
 * 线上 kiro-drop 就正处于这个状态：Drop 的能力从「仅 omit」改成
 * fixed/webhook/bestAvailable 后，旧配置里持久化的 `omit` 落在了列表之外。
 *
 * 保留旧值而不是在加载时静默改写配置：改写等于替用户做了一次他没点过的变更。
 */
export function regionModeOptions(
  supported: readonly PurchaseRegionMode[],
  current: PurchaseRegionMode,
): PurchaseRegionMode[] {
  return supported.includes(current) ? [...supported] : [current, ...supported]
}

export function getSupplierCapabilities(kind: SupplierKind): SupplierCapabilities {
  const capabilities = supplierKindCapabilities[kind]
  return { ...capabilities, regionModes: [...capabilities.regionModes] }
}

export function buildSupplierNicknamePreview(
  supplierName: string,
  supplierId: string | undefined,
  nicknameLabel: string,
): string {
  const supplier = supplierName.trim() || supplierId?.trim() || 'supplier'
  const label = nicknameLabel.trim()
  const segments = label && label.toLocaleLowerCase() !== supplier.toLocaleLowerCase()
    ? [supplier, label]
    : [supplier]
  return [...segments, '1df694d5', '1'].join('-')
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
export function emptySupplierEntry(
  kind: SupplierKind,
  common: SupplierCommonConfig = defaultSupplierCommon,
): SupplierEntryUpdate {
  const regionDefaults = supplierKindRegionDefaults[kind]
  return {
    id: '',
    name: '',
    kind,
    enabled: true,
    baseUrl: supplierKindBaseUrls[kind] ?? '',
    publicBaseUrl: '',
    autoPurchase: true,
    autoDeleteForbidden: common.autoDeleteForbidden,
    minPurchase: 1,
    maxPurchase: 1,
    apiRegion: 'us-east-1',
    ...regionDefaults,
    credentialApiRegionFallback: 'us-east-1',
    rpmLimit: common.rpmLimit,
    maxConcurrency: common.maxConcurrency,
    priority: common.priority,
    groups: [...common.groups],
    sourceChannel: common.sourceChannel,
    nicknamePrefix: common.nicknameLabel,
    importOverrides: {},
    // 新建默认开启补货闸：供货商不停推到货时，不加闸就是每次都掏钱。
    restockOnlyWhenExhausted: true,
    // 目标存量：每家常备 1 个。填 0 是失效保护（不买），所以默认给 1。
    targetUsable: 1,
    lowQuotaThreshold: 0,
    // 0 = 不限价。默认不限，避免新建的供货商因为对方不报价而一个都买不到。
    maxUnitPrice: 0,
  }
}

export function toSupplierEntryUpdate(entry: SupplierEntryView): SupplierEntryUpdate {
  const importOverrides = entry.importOverrides ?? {}
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
    purchaseRegionMode: entry.purchaseRegionMode,
    purchaseRegion: entry.purchaseRegion,
    credentialApiRegionFallback: entry.credentialApiRegionFallback,
    rpmLimit: entry.rpmLimit,
    maxConcurrency: entry.maxConcurrency,
    priority: entry.priority,
    groups: [...entry.groups],
    sourceChannel: entry.sourceChannel,
    nicknamePrefix: entry.nicknamePrefix,
    importOverrides: {
      ...importOverrides,
      ...(importOverrides.groups ? { groups: [...importOverrides.groups] } : {}),
    },
    restockOnlyWhenExhausted: entry.restockOnlyWhenExhausted,
    targetUsable: entry.targetUsable,
    lowQuotaThreshold: entry.lowQuotaThreshold,
    maxUnitPrice: entry.maxUnitPrice,
  }
}

export function parseSupplierNumberDraft(value: string, minimum: number): number | null {
  const normalized = value.trim()
  if (!normalized) return null

  const parsed = Number(normalized)
  return Number.isSafeInteger(parsed) && parsed >= minimum ? parsed : null
}

/**
 * Same as {@link parseSupplierNumberDraft} but accepts fractions, for money fields.
 *
 * Prices are not integers (Drop quotes `"2.20"`), so the integer parser would reject every
 * realistic cap. `NaN` and `Infinity` are rejected: a comparison against `NaN` is always
 * false, which would silently disable the very gate this value configures.
 */
export function parseSupplierDecimalDraft(value: string, minimum: number): number | null {
  const normalized = value.trim()
  if (!normalized) return null

  const parsed = Number(normalized)
  return Number.isFinite(parsed) && parsed >= minimum ? parsed : null
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
