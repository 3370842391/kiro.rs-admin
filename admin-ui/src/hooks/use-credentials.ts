import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getCredentials,
  batchUpdateCredentials,
  setCredentialDisabled,
  setCredentialPriority,
  resetCredentialFailure,
  forceRefreshToken,
  clearThrottle,
  getCredentialBalance,
  getCredentialModels,
  addCredential,
  deleteCredential,
  updateCredential,
  updateRefreshToken,
  getLoadBalancingMode,
  setLoadBalancingMode,
  getAccountThrottleConfig,
  setAccountThrottleConfig,
  getCompatibilityConfig,
  setCompatibilityConfig,
  getRetryPolicy,
  setRetryPolicy,
  getEndpointChains,
  setEndpointChains,
  getEndpointMode,
  setEndpointMode,
  getCacheHitRate,
  setCacheHitRate,
  getCachePolicy,
  setCachePolicy,
  clearCachePolicyEntries,
  getLogGovernanceConfig,
  setLogGovernanceConfig,
  resetSuccessCount,
  resetAllSuccessCount,
} from '@/api/credentials'
import type { AddCredentialRequest, UpdateCredentialRequest, UpdateRefreshTokenRequest } from '@/types/api'

// 查询凭据列表
export function useCredentials() {
  return useQuery({
    queryKey: ['credentials'],
    queryFn: getCredentials,
    refetchInterval: 10000, // 每 10 秒刷新一次，保持 RPM / in-flight 指标新鲜
  })
}

export function useBatchUpdateCredentials() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: batchUpdateCredentials,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 查询凭据余额
export function useCredentialBalance(id: number | null) {
  return useQuery({
    queryKey: ['credential-balance', id],
    queryFn: () => getCredentialBalance(id!),
    enabled: id !== null,
    retry: false, // 余额查询失败时不重试（避免重复请求被封禁的账号）
  })
}

// 查询凭据当前可用的模型列表（按需实时查询上游）
export function useCredentialModels(id: number | null) {
  return useQuery({
    queryKey: ['credential-models', id],
    queryFn: () => getCredentialModels(id!),
    enabled: id !== null,
    retry: false, // 失败不重试，避免对被封禁/异常账号反复请求
  })
}

// 设置禁用状态
export function useSetDisabled() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, disabled }: { id: number; disabled: boolean }) =>
      setCredentialDisabled(id, disabled),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 设置优先级
export function useSetPriority() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, priority }: { id: number; priority: number }) =>
      setCredentialPriority(id, priority),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置失败计数
export function useResetFailure() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetCredentialFailure(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 强制刷新 Token
export function useForceRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => forceRefreshToken(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 解除账号级风控冷却
export function useClearThrottle() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => clearThrottle(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 添加新凭据
export function useAddCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (req: AddCredentialRequest) => addCredential(req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 删除凭据
export function useDeleteCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => deleteCredential(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置单个凭据的成功次数
export function useResetSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: number) => resetSuccessCount(id),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 重置所有凭据的成功次数
export function useResetAllSuccessCount() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: () => resetAllSuccessCount(),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新已禁用凭据的 refreshToken
export function useUpdateRefreshToken() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateRefreshTokenRequest }) =>
      updateRefreshToken(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 更新凭据可编辑字段
export function useUpdateCredential() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ id, req }: { id: number; req: UpdateCredentialRequest }) =>
      updateCredential(id, req),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取负载均衡模式
export function useLoadBalancingMode() {
  return useQuery({
    queryKey: ['loadBalancingMode'],
    queryFn: getLoadBalancingMode,
  })
}

// 设置负载均衡模式
export function useSetLoadBalancingMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLoadBalancingMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['loadBalancingMode'] })
    },
  })
}

// 获取账号级风控故障转移配置
export function useAccountThrottleConfig() {
  return useQuery({
    queryKey: ['accountThrottleConfig'],
    queryFn: getAccountThrottleConfig,
  })
}

// 更新账号级风控故障转移配置
export function useSetAccountThrottleConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setAccountThrottleConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['accountThrottleConfig'] })
    },
  })
}

export function useCompatibilityConfig() {
  return useQuery({
    queryKey: ['compatibilityConfig'],
    queryFn: getCompatibilityConfig,
  })
}

export function useSetCompatibilityConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCompatibilityConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['compatibilityConfig'] })
    },
  })
}

// 获取普通 429 重试策略
export function useRetryPolicy() {
  return useQuery({
    queryKey: ['retryPolicy'],
    queryFn: getRetryPolicy,
  })
}

// 更新普通 429 重试策略
export function useSetRetryPolicy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setRetryPolicy,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['retryPolicy'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取 429 降级桶链配置
export function useEndpointChains() {
  return useQuery({
    queryKey: ['endpointChains'],
    queryFn: getEndpointChains,
  })
}

// 设置 429 降级桶链配置
export function useSetEndpointChains() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setEndpointChains,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['endpointChains'] })
    },
  })
}

export function useEndpointMode() {
  return useQuery({
    queryKey: ['endpointMode'],
    queryFn: getEndpointMode,
  })
}

export function useSetEndpointMode() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setEndpointMode,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['endpointMode'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
  })
}

// 获取缓存命中率整形区间
export function useCacheHitRate() {
  return useQuery({
    queryKey: ['cacheHitRate'],
    queryFn: getCacheHitRate,
  })
}

// 设置缓存命中率整形区间
export function useSetCacheHitRate() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCacheHitRate,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cacheHitRate'] })
    },
  })
}

// 获取完整缓存策略和运行时状态
export function useCachePolicy() {
  return useQuery({
    queryKey: ['cachePolicy'],
    queryFn: getCachePolicy,
  })
}

// 更新缓存策略；同步刷新旧命中率查询以保持兼容页面一致
export function useSetCachePolicy() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setCachePolicy,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cachePolicy'] })
      queryClient.invalidateQueries({ queryKey: ['cacheHitRate'] })
    },
  })
}

export function useClearCachePolicyEntries() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: clearCachePolicyEntries,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['cachePolicy'] })
    },
  })
}

// 获取日志治理配置
export function useLogGovernanceConfig() {
  return useQuery({
    queryKey: ['logGovernanceConfig'],
    queryFn: getLogGovernanceConfig,
  })
}

// 更新日志治理配置
export function useSetLogGovernanceConfig() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setLogGovernanceConfig,
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['logGovernanceConfig'] })
    },
  })
}
