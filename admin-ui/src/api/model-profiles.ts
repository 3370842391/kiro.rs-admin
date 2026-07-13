import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  ApplyModelProfilesRequest,
  DeleteModelProfileRequest,
  FetchModelProfileRequest,
  ModelProfilePreviewResponse,
  ModelProfileSettingsResponse,
  ModelProfilesResponse,
  ModelProfileSyncResponse,
  PatchModelProfileRequest,
  PreviewModelProfilesRequest,
  SetModelProfileSettingsRequest,
  SyncModelProfilesRequest,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 120_000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export async function listModelProfiles(): Promise<ModelProfilesResponse> {
  const { data } = await api.get<ModelProfilesResponse>('/model-profiles')
  return data
}

export async function patchModelProfile(
  modelId: string,
  request: PatchModelProfileRequest,
): Promise<ModelProfilesResponse> {
  const { data } = await api.patch<ModelProfilesResponse>(
    `/model-profiles/${encodeURIComponent(modelId)}`,
    request,
  )
  return data
}

export async function deleteModelProfile(
  modelId: string,
  request: DeleteModelProfileRequest,
): Promise<ModelProfilesResponse> {
  const { data } = await api.delete<ModelProfilesResponse>(
    `/model-profiles/${encodeURIComponent(modelId)}`,
    { data: request },
  )
  return data
}

export async function fetchModelProfile(
  modelId: string,
  request: FetchModelProfileRequest,
): Promise<ModelProfileSyncResponse> {
  const { data } = await api.post<ModelProfileSyncResponse>(
    `/model-profiles/${encodeURIComponent(modelId)}/fetch`,
    request,
  )
  return data
}

export async function syncModelProfiles(
  request: SyncModelProfilesRequest,
): Promise<ModelProfileSyncResponse> {
  const { data } = await api.post<ModelProfileSyncResponse>('/model-profiles/sync', request)
  return data
}

export async function previewModelProfiles(
  request: PreviewModelProfilesRequest,
): Promise<ModelProfilePreviewResponse> {
  const { data } = await api.post<ModelProfilePreviewResponse>(
    '/model-profiles/preview',
    request,
  )
  return data
}

export async function applyModelProfilePreview(
  request: ApplyModelProfilesRequest,
): Promise<ModelProfilesResponse> {
  const { data } = await api.post<ModelProfilesResponse>('/model-profiles/apply', request)
  return data
}

export async function setModelProfileSettings(
  request: SetModelProfileSettingsRequest,
): Promise<ModelProfileSettingsResponse> {
  const { data } = await api.put<ModelProfileSettingsResponse>(
    '/model-profiles/settings',
    request,
  )
  return data
}
