import axios from 'axios'
import { buildSupplierConfigPayload } from '@/lib/key-supplier'
import { storage } from '@/lib/storage'
import type {
  PurchaseResponse,
  SupplierConfigUpdate,
  SupplierConfigView,
  SupplierEventPage,
  SupplierEventQuery,
  SupplierMarkEventsReadRequest,
  SupplierMarkEventsReadResponse,
  SupplierOverview,
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
