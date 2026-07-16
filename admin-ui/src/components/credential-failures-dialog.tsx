import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogDescription,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { useTraces } from '@/hooks/use-traces'
import {
  compactAttemptLabel,
  failureDisposition,
  failureDispositionLabel,
  sortTraceAttempts,
  type FailureDisposition,
} from '@/lib/failure-trace'
import type { TraceAttempt, TraceRecord } from '@/types/api'

interface CredentialFailuresDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credentialId: number
  email?: string
}

function outcomeStyle(outcome: string | null): {
  label: string
  variant: 'destructive' | 'warning' | 'outline' | 'secondary'
} {
  switch (outcome) {
    case 'success':
      return { label: '成功', variant: 'secondary' }
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
    case 'stream_interrupted':
      return { label: '流中断', variant: 'warning' }
    default:
      return { label: outcome || '未知', variant: 'secondary' }
  }
}

function recoveryVariant(
  disposition: FailureDisposition,
): 'destructive' | 'warning' | 'outline' | 'secondary' {
  if (disposition === 'failed') return 'destructive'
  if (disposition === 'interrupted') return 'warning'
  if (disposition === 'not_sent') return 'secondary'
  return 'outline'
}

function formatTime(ts: string): string {
  const date = new Date(ts)
  if (Number.isNaN(date.getTime())) return ts
  return date.toLocaleString('zh-CN', { hour12: false })
}

function keySourceLabel(rec: TraceRecord): string {
  return rec.keyName ?? `#${rec.keyId}`
}

function credentialLabel(attempt: TraceAttempt): string {
  return attempt.email
    ? `${attempt.email} (#${attempt.credentialId})`
    : `#${attempt.credentialId}`
}

function AttemptRow({
  attempt,
  position,
}: {
  attempt: TraceAttempt
  position: number
}) {
  const style = outcomeStyle(attempt.outcome)
  const endpoint = attempt.endpoint.trim() || '未知端点'
  const http = attempt.httpStatus == null ? '网络错误' : `HTTP ${attempt.httpStatus}`

  return (
    <div className="rounded-md border border-border/50 bg-background/60 p-2.5">
      <div className="flex flex-wrap items-center gap-2 text-[12px]">
        <Badge variant="secondary">第 {position + 1} 跳</Badge>
        <span className="font-medium">{credentialLabel(attempt)}</span>
        <Badge variant="outline">端点：{endpoint}</Badge>
        <span className="font-mono text-muted-foreground">{http}</span>
        <span className="text-muted-foreground">耗时 {attempt.durationMs} ms</span>
        <Badge variant={style.variant}>{style.label}</Badge>
      </div>
      {attempt.errorSnippet ? (
        <pre className="mt-2 max-h-32 overflow-auto whitespace-pre-wrap break-all rounded-md bg-secondary/50 p-2 font-mono text-[11px] text-muted-foreground">
          {attempt.errorSnippet}
        </pre>
      ) : null}
    </div>
  )
}

function FailureRequestCard({
  rec,
  inspectedCredentialId,
}: {
  rec: TraceRecord
  inspectedCredentialId: number
}) {
  const attempts = sortTraceAttempts(rec.attempts)
  const disposition = failureDisposition(rec, inspectedCredentialId)
  const retryCount = Math.max(0, attempts.length - 1)

  return (
    <article className="rounded-lg border border-border/60 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="tabular-nums text-muted-foreground">{formatTime(rec.ts)}</span>
        <Badge variant="secondary">客户端 Key：{keySourceLabel(rec)}</Badge>
        <Badge variant={recoveryVariant(disposition)}>
          {failureDispositionLabel(disposition)}
        </Badge>
        <span className="text-[12px] text-muted-foreground">
          尝试 {attempts.length} 次（含 {retryCount} 次重试）
        </span>
        <span className="ml-auto text-[12px] text-muted-foreground">{rec.model}</span>
      </div>

      {attempts.length === 0 ? (
        <div className="mt-3 text-[13px] text-muted-foreground">请求未到达上游</div>
      ) : (
        <>
          <div className="mt-3 flex flex-wrap items-center gap-1 text-[12px] font-mono text-muted-foreground">
            {attempts.map((attempt, index) => (
              <span
                key={`${rec.traceId}-summary-${attempt.attempt}`}
                className="inline-flex items-center gap-1"
              >
                {index > 0 ? <span aria-hidden="true">→</span> : null}
                <span>{compactAttemptLabel(attempt)}</span>
              </span>
            ))}
          </div>
          <div className="mt-3 space-y-2">
            {attempts.map((attempt, index) => (
              <AttemptRow
                key={`${rec.traceId}-${attempt.attempt}`}
                attempt={attempt}
                position={index}
              />
            ))}
          </div>
        </>
      )}

      {rec.finalStatus === 'interrupted' && rec.interruptedAfterBytes != null ? (
        <div className="mt-2 text-[12px] text-muted-foreground">
          中断前已发送 {rec.interruptedAfterBytes} 字节
        </div>
      ) : null}
    </article>
  )
}

export function CredentialFailuresDialog({
  open,
  onOpenChange,
  credentialId,
  email,
}: CredentialFailuresDialogProps) {
  const { data, isLoading } = useTraces(
    { failedAttemptCredentialId: credentialId, limit: 50 },
    open,
  )
  const records = data?.records ?? []

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle>失败日志详情</DialogTitle>
          <DialogDescription>
            {email || `凭据 #${credentialId}`} 最近涉及失败的请求（最多 50 个请求）
          </DialogDescription>
        </DialogHeader>
        <div className="max-h-[70vh] space-y-3 overflow-y-auto pr-1">
          {isLoading ? (
            <div className="py-6 text-center text-sm text-muted-foreground">加载中…</div>
          ) : records.length === 0 ? (
            <div className="py-6 text-center text-sm text-muted-foreground">
              该凭据暂无失败记录（trace 关闭或近期无失败）。
            </div>
          ) : (
            records.map((rec) => (
              <FailureRequestCard
                key={rec.traceId}
                rec={rec}
                inspectedCredentialId={credentialId}
              />
            ))
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
