import type { TraceAttempt, TraceRecord } from '@/types/api'

export type FailureDisposition =
  | 'switched_endpoint'
  | 'retried_same_endpoint'
  | 'switched_credential'
  | 'interrupted'
  | 'failed'
  | 'not_sent'

export function sortTraceAttempts(
  attempts: ReadonlyArray<TraceAttempt>,
): TraceAttempt[] {
  return [...attempts].sort((left, right) => left.attempt - right.attempt)
}

export function failureDisposition(
  record: TraceRecord,
  inspectedCredentialId: number,
): FailureDisposition {
  const attempts = sortTraceAttempts(record.attempts)
  if (attempts.length === 0) return 'not_sent'
  if (record.finalStatus === 'interrupted') return 'interrupted'
  if (record.finalStatus !== 'success') return 'failed'
  if (record.finalCredentialId !== inspectedCredentialId) return 'switched_credential'

  const successAttempt = [...attempts]
    .reverse()
    .find(
      (item) =>
        item.credentialId === inspectedCredentialId && item.outcome === 'success',
    )
  const failedAttempts = attempts.filter(
    (item) =>
      item.credentialId === inspectedCredentialId && item.outcome !== 'success',
  )
  const switchedEndpoint =
    successAttempt != null &&
    failedAttempts.some(
      (item) => item.endpoint.trim() !== successAttempt.endpoint.trim(),
    )

  return switchedEndpoint ? 'switched_endpoint' : 'retried_same_endpoint'
}

export function failureDispositionLabel(disposition: FailureDisposition): string {
  switch (disposition) {
    case 'switched_endpoint':
      return '同账号切换端点后成功'
    case 'retried_same_endpoint':
      return '同账号重试后成功'
    case 'switched_credential':
      return '切换其他账号后成功'
    case 'interrupted':
      return '流式响应中断'
    case 'failed':
      return '最终失败'
    case 'not_sent':
      return '请求未到达上游'
  }
}

export function compactAttemptLabel(attempt: TraceAttempt): string {
  const endpoint = attempt.endpoint.trim() || '未知端点'
  const status = attempt.httpStatus == null ? '网络错误' : String(attempt.httpStatus)
  return `#${attempt.credentialId} / ${endpoint} ${status}`
}
