import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  ModelMapping,
  ModelMappingsResponse,
  UpsertModelMappingRequest,
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

export async function listModelMappings(): Promise<ModelMappingsResponse> {
  const { data } = await api.get<ModelMappingsResponse>('/model-mappings')
  return data
}

export async function upsertModelMapping(
  req: UpsertModelMappingRequest,
): Promise<ModelMapping> {
  const { data } = await api.post<ModelMapping>('/model-mappings', req)
  return data
}

/** 整表替换（一次性保存全部映射） */
export async function replaceModelMappings(
  mappings: ModelMapping[],
): Promise<ModelMappingsResponse> {
  const { data } = await api.put<ModelMappingsResponse>('/model-mappings', { mappings })
  return data
}

export async function deleteModelMapping(source: string): Promise<void> {
  // 源名可能含 . 或特殊字符，encodeURIComponent 保证路径安全
  await api.delete(`/model-mappings/${encodeURIComponent(source)}`)
}
