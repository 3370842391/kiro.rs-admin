import axios from 'axios'
import { buildSupplierConfigPayload, buildSupplierEntryPayload } from '@/lib/key-supplier'
import { storage } from '@/lib/storage'
import type {
  PurchaseResponse,
  SupplierCallbackUrlResponse,
  SupplierConfigUpdate,
  SupplierConfigView,
  SupplierDeleteResponse,
  SupplierEntryUpdate,
  SupplierEntryView,
  SupplierEventPage,
  SupplierEventQuery,
  SupplierListResponse,
  SupplierMarkEventsReadRequest,
  SupplierMarkEventsReadResponse,
  SupplierOverview,
  SupplierPoolConfig,
  SupplierPoolStatus,
  SupplierRetryEventResponse,
  SupplierWebhookRegisterResponse,
  SupplierWebhookTestResponse,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 15000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export async function getSupplierConfig(): Promise<SupplierConfigView> {
  const { data } = await api.get<SupplierConfigView>('/config/key-supplier')
  return data
}

export async function updateSupplierConfig(
  update: SupplierConfigUpdate,
): Promise<SupplierConfigView> {
  const { data } = await api.put<SupplierConfigView>(
    '/config/key-supplier',
    buildSupplierConfigPayload(update),
  )
  return data
}

export async function getSupplierOverview(): Promise<SupplierOverview> {
  const { data } = await api.get<SupplierOverview>('/key-supplier/overview')
  return data
}

export async function manualPurchaseSupplier(count: number): Promise<PurchaseResponse> {
  const { data } = await api.post<PurchaseResponse>('/key-supplier/purchase', { count })
  return data
}

export async function registerSupplierWebhook(): Promise<SupplierWebhookRegisterResponse> {
  const { data } = await api.post<SupplierWebhookRegisterResponse>('/key-supplier/webhook/register')
  return data
}

export async function testSupplierWebhook(): Promise<SupplierWebhookTestResponse> {
  const { data } = await api.post<SupplierWebhookTestResponse>('/key-supplier/webhook/test')
  return data
}

export async function listSupplierEvents(
  query: SupplierEventQuery = {},
): Promise<SupplierEventPage> {
  const { data } = await api.get<SupplierEventPage>('/key-supplier/events', { params: query })
  return data
}

export async function markSupplierEventsRead(
  request: SupplierMarkEventsReadRequest,
): Promise<SupplierMarkEventsReadResponse> {
  const { data } = await api.post<SupplierMarkEventsReadResponse>(
    '/key-supplier/events/read',
    request,
  )
  return data
}

export async function retrySupplierEvent(id: number): Promise<SupplierRetryEventResponse> {
  const { data } = await api.post<SupplierRetryEventResponse>(`/key-supplier/events/${id}/retry`)
  return data
}

// ============ Global key pool ============

export async function getSupplierPool(): Promise<SupplierPoolConfig> {
  const { data } = await api.get<SupplierPoolConfig>('/key-supplier/pool')
  return data
}

export async function updateSupplierPool(
  update: SupplierPoolConfig,
): Promise<SupplierPoolConfig> {
  const { data } = await api.put<SupplierPoolConfig>('/key-supplier/pool', update)
  return data
}

/** Read-only. Never triggers a purchase. */
export async function getSupplierPoolStatus(): Promise<SupplierPoolStatus> {
  const { data } = await api.get<SupplierPoolStatus>('/key-supplier/pool/status')
  return data
}

// ============ Multi-supplier ============

function supplierPath(id: string, suffix = ''): string {
  return `/key-suppliers/${encodeURIComponent(id)}${suffix}`
}

export async function listSuppliers(): Promise<SupplierListResponse> {
  const { data } = await api.get<SupplierListResponse>('/key-suppliers')
  return data
}

export async function createSupplier(update: SupplierEntryUpdate): Promise<SupplierEntryView> {
  const { data } = await api.post<SupplierEntryView>(
    '/key-suppliers',
    buildSupplierEntryPayload(update),
  )
  return data
}

export async function updateSupplier(
  id: string,
  update: SupplierEntryUpdate,
): Promise<SupplierEntryView> {
  const { data } = await api.put<SupplierEntryView>(
    supplierPath(id),
    buildSupplierEntryPayload(update),
  )
  return data
}

export async function deleteSupplier(id: string): Promise<SupplierDeleteResponse> {
  const { data } = await api.delete<SupplierDeleteResponse>(supplierPath(id))
  return data
}

export async function getSupplierEntryOverview(id: string): Promise<SupplierOverview> {
  const { data } = await api.get<SupplierOverview>(supplierPath(id, '/overview'))
  return data
}

export async function purchaseFromSupplier(
  id: string,
  count: number,
): Promise<PurchaseResponse> {
  const { data } = await api.post<PurchaseResponse>(supplierPath(id, '/purchase'), { count })
  return data
}

export async function registerSupplierEntryWebhook(
  id: string,
): Promise<SupplierWebhookRegisterResponse> {
  const { data } = await api.post<SupplierWebhookRegisterResponse>(
    supplierPath(id, '/webhook/register'),
  )
  return data
}

export async function testSupplierEntryWebhook(
  id: string,
): Promise<SupplierWebhookTestResponse> {
  const { data } = await api.post<SupplierWebhookTestResponse>(supplierPath(id, '/webhook/test'))
  return data
}

/** For both kiroapp protocols, this URL has to be pasted into the vendor's own webhook field. */
export async function getSupplierCallbackUrl(id: string): Promise<SupplierCallbackUrlResponse> {
  const { data } = await api.get<SupplierCallbackUrlResponse>(supplierPath(id, '/callback-url'))
  return data
}
