import axios from 'axios'
import { storage } from '@/lib/storage'
import { buildSnapshotParams } from '@/lib/error-snapshot-utils'
import type {
  ErrorSnapshotDetail,
  ErrorSnapshotPage,
  ErrorSnapshotPayload,
  ErrorSnapshotQuery,
  ErrorSnapshotStorageStatus,
  SuccessResponse,
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

export async function listErrorSnapshots(query: ErrorSnapshotQuery): Promise<ErrorSnapshotPage> {
  const { data } = await api.get<ErrorSnapshotPage>('/error-snapshots', {
    params: buildSnapshotParams(query),
  })
  return data
}

export async function getErrorSnapshot(id: string): Promise<ErrorSnapshotDetail> {
  const { data } = await api.get<ErrorSnapshotDetail>(`/error-snapshots/${encodeURIComponent(id)}`)
  return data
}

export async function getErrorSnapshotPayload(id: string, seq: number): Promise<ErrorSnapshotPayload> {
  const { data } = await api.get<ErrorSnapshotPayload>(
    `/error-snapshots/${encodeURIComponent(id)}/payload/${seq}`,
  )
  return data
}

export async function getErrorSnapshotStorage(): Promise<ErrorSnapshotStorageStatus> {
  const { data } = await api.get<ErrorSnapshotStorageStatus>('/error-snapshots/storage')
  return data
}

export async function downloadErrorSnapshot(id: string): Promise<Blob> {
  const { data } = await api.get<Blob>(`/error-snapshots/${encodeURIComponent(id)}/download`, {
    responseType: 'blob',
  })
  return data
}

export async function pinErrorSnapshot(id: string): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/error-snapshots/${encodeURIComponent(id)}/pin`)
  return data
}

export async function unpinErrorSnapshot(id: string): Promise<SuccessResponse> {
  const { data } = await api.post<SuccessResponse>(`/error-snapshots/${encodeURIComponent(id)}/unpin`)
  return data
}

export async function deleteErrorSnapshot(id: string): Promise<SuccessResponse> {
  const { data } = await api.delete<SuccessResponse>(`/error-snapshots/${encodeURIComponent(id)}`)
  return data
}

export async function cleanupErrorSnapshots(): Promise<Record<string, unknown>> {
  const { data } = await api.post<Record<string, unknown>>('/error-snapshots/cleanup')
  return data
}
