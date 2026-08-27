export type OutcomeBadgeVariant =
  | 'default'
  | 'secondary'
  | 'destructive'
  | 'outline'
  | 'success'
  | 'warning'

export const ERROR_TYPE_OPTIONS = [
  { value: '', label: '全部错误类型' },
  { value: 'client_disconnected', label: '客户端断开' },
  { value: 'stream_idle_timeout', label: '上游空闲超时' },
  { value: 'stream_read_error', label: '上游读失败' },
  { value: 'stream_interrupted', label: '上游断流' },
  { value: 'payload_limit_exceeded', label: '请求体超限' },
  { value: 'upstream_tool_schema_error', label: '工具 schema' },
  { value: 'tool_schema_retry_exhausted', label: '工具 schema 重试仍失败' },
  { value: 'proxy_pool_empty', label: '代理候选空' },
  { value: 'quota_exhausted', label: '额度耗尽' },
  { value: 'account_throttled', label: '账号风控' },
  { value: 'auth_failed', label: '鉴权失败' },
  { value: 'transient', label: '瞬态错误' },
  { value: 'network_error', label: '网络错误' },
  { value: 'bad_request', label: '请求错误' },
  { value: 'unknown', label: '未知' },
] as const

/** 真正的上游断流。`interrupted` 状态里大部分是客户端断开，不要混进这一组。 */
export const UPSTREAM_STREAM_BREAK_TYPES = [
  'stream_idle_timeout',
  'stream_read_error',
  'stream_interrupted',
] as const

export function isUpstreamStreamBreak(errorType: string | null | undefined): boolean {
  return UPSTREAM_STREAM_BREAK_TYPES.some((type) => type === errorType)
}

export function isClientDisconnect(errorType: string | null | undefined): boolean {
  return errorType === 'client_disconnected'
}

export function interruptedStatusHint(errorType?: string | null): string {
  if (isClientDisconnect(errorType)) return '客户端断开（不是上游断流）'
  if (isUpstreamStreamBreak(errorType)) return '上游断流'
  return '中断（含客户端断开与上游断流）'
}

export function outcomeStyle(outcome: string): {
  label: string
  variant: OutcomeBadgeVariant
} {
  switch (outcome) {
    case 'success':
      return { label: '成功', variant: 'success' }
    case 'quota_exhausted':
      return { label: '额度耗尽', variant: 'warning' }
    case 'account_throttled':
      return { label: '账号风控', variant: 'warning' }
    case 'auth_failed':
      return { label: '鉴权失败', variant: 'destructive' }
    case 'transient':
      return { label: '瞬态错误', variant: 'outline' }
    case 'network_error':
      return { label: '网络错误', variant: 'destructive' }
    case 'bad_request':
      return { label: '请求错误', variant: 'destructive' }
    case 'payload_limit_exceeded':
      return { label: '请求体超限', variant: 'destructive' }
    case 'client_disconnected':
      return { label: '客户端断开', variant: 'outline' }
    case 'stream_idle_timeout':
      return { label: '上游空闲超时', variant: 'warning' }
    case 'stream_read_error':
      return { label: '上游读失败', variant: 'warning' }
    case 'stream_interrupted':
      return { label: '上游断流', variant: 'warning' }
    case 'upstream_tool_schema_error':
      return { label: '工具 schema', variant: 'destructive' }
    case 'tool_schema_retry_exhausted':
      return { label: '工具 schema 重试仍失败', variant: 'destructive' }
    case 'proxy_pool_empty':
      return { label: '代理候选空', variant: 'destructive' }
    default:
      return { label: outcome || '未知', variant: 'secondary' }
  }
}
