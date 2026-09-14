import axios from 'axios'
import { storage } from '@/lib/storage'
import type {
  BanPostmortem,
  FailureStatsMap,
  PoolHealth,
  RecentActivityResponse,
  TracePage,
  TraceQuery,
} from '@/types/api'

const api = axios.create({
  baseURL: '/api/admin',
  timeout: 30000,
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((config) => {
  const apiKey = storage.getApiKey()
  if (apiKey) config.headers['x-api-key'] = apiKey
  return config
})

export async function getTraces(query: TraceQuery): Promise<TracePage> {
  const params: Record<string, string> = {}
  if (query.status) params.status = query.status
  if (query.errorType) params.errorType = query.errorType
  if (query.credentialId != null) params.credentialId = String(query.credentialId)
  if (query.keyId != null) params.keyId = String(query.keyId)
  if (query.failedAttemptCredentialId != null)
    params.failedAttemptCredentialId = String(query.failedAttemptCredentialId)
  if (query.model) params.model = query.model
  if (query.group) params.group = query.group
  if (query.compactionDiagnosis) params.compactionDiagnosis = query.compactionDiagnosis
  if (query.sessionHash) params.sessionHash = query.sessionHash
  if (query.highPressureOnly) params.highPressureOnly = 'true'
  if (query.onlyFailed) params.onlyFailed = 'true'
  if (query.limit != null) params.limit = String(query.limit)
  if (query.offset != null) params.offset = String(query.offset)
  const { data } = await api.get<TracePage>('/traces', { params })
  return data
}

export async function getFailureStats(): Promise<FailureStatsMap> {
  const { data } = await api.get<FailureStatsMap>('/traces/failure-stats')
  return data
}

/**
 * 按凭据的近期请求形态（成功 / 429 / 其它失败）。
 *
 * 与 failure-stats 的区别是带时间窗且含成功数：排查封号时「这个号最近一小时
 * 打了多少、其中多少被限流」比历史累计失败有用——累计值只会单调增长。
 */
export async function getRecentActivity(windowMinutes = 60): Promise<RecentActivityResponse> {
  const { data } = await api.get<RecentActivityResponse>('/traces/recent-activity', {
    params: { windowMinutes: String(windowMinutes) },
  })
  return data
}

/**
 * 号池风险体检：限流形态、出口集中度、批量清扫特征 + 「该做什么」的结论。
 */
export async function getPoolHealth(windowMinutes = 60): Promise<PoolHealth> {
  const { data } = await api.get<PoolHealth>('/pool-health', {
    params: { windowMinutes: String(windowMinutes) },
  })
  return data
}

/** 封号复盘：把台账切成一波一波，每波给出出口分布与归因结论 */
export async function getBanPostmortem(limit = 200): Promise<BanPostmortem> {
  const { data } = await api.get<BanPostmortem>('/ban-postmortem', {
    params: { limit: String(limit) },
  })
  return data
}

/** 清空全部请求链路记录，返回删除条数 */
export async function clearTraces(): Promise<number> {
  const { data } = await api.delete<{ cleared: number }>('/traces')
  return data.cleared
}
