import type {
  SupplierConfigPayload,
  SupplierConfigUpdate,
  SupplierEvent,
  SupplierEventPage,
  SupplierEventStatus,
} from '@/types/api'

export function buildSupplierConfigPayload(update: SupplierConfigUpdate): SupplierConfigPayload {
  const payload: SupplierConfigPayload = {
    baseUrl: update.baseUrl,
    publicBaseUrl: update.publicBaseUrl,
    autoPurchase: update.autoPurchase,
    minPurchase: update.minPurchase,
    maxPurchase: update.maxPurchase,
    apiRegion: update.apiRegion,
    rpmLimit: update.rpmLimit,
    priority: update.priority,
    groups: [...update.groups],
    sourceChannel: update.sourceChannel,
    nicknamePrefix: update.nicknamePrefix,
  }

  const apiKey = update.apiKey?.trim()
  if (apiKey) payload.apiKey = apiKey

  const webhookToken = update.webhookToken?.trim()
  if (webhookToken) payload.webhookToken = webhookToken

  return payload
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

  const previousIds = new Set(getSupplierEvents(previous).map((event) => event.id))
  return getSupplierEvents(current).some(
    (event) => !previousIds.has(event.id) && event.readAt === null,
  )
}
