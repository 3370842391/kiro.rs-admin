import { useEffect, useMemo, useState } from 'react'
import { Copy, Download, Loader2, Pin, PinOff, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { useConfirm } from '@/components/ui/confirm-dialog'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  useDeleteErrorSnapshot,
  useDownloadErrorSnapshot,
  useErrorSnapshot,
  useErrorSnapshotPayload,
  usePinErrorSnapshot,
  useUnpinErrorSnapshot,
} from '@/hooks/use-error-snapshots'
import { dispositionLabel, formatBytes, severityLabel, snapshotDisposition } from '@/lib/error-snapshot-utils'
import { extractErrorMessage } from '@/lib/utils'
import type {
  ErrorSnapshotDetail,
  ErrorSnapshotPayloadMeta,
  SnapshotPayloadKind,
} from '@/types/api'

type SnapshotTab = 'overview' | SnapshotPayloadKind

const TABS: Array<{ key: SnapshotTab; label: string }> = [
  { key: 'overview', label: '概览' },
  { key: 'client_request', label: '客户端请求' },
  { key: 'kiro_request', label: 'Kiro 请求' },
  { key: 'upstream_response', label: '上游响应' },
  { key: 'tool_diagnostics', label: '工具诊断' },
  { key: 'stream_tail', label: '流式尾部' },
  { key: 'internal_error', label: '内部错误' },
]

export interface ErrorSnapshotDialogProps {
  snapshotId: string | null
  open: boolean
  onOpenChange: (open: boolean) => void
}

function displayTime(value: string): string {
  const date = new Date(value)
  return Number.isNaN(date.getTime())
    ? value
    : date.toLocaleString('zh-CN', { hour12: false })
}

function contentText(content: unknown): string {
  if (typeof content === 'string') return content
  return JSON.stringify(content, null, 2) ?? String(content)
}

function severityVariant(severity: ErrorSnapshotDetail['severity']) {
  if (severity === 'critical' || severity === 'error') return 'destructive' as const
  if (severity === 'warning') return 'warning' as const
  return 'secondary' as const
}

function Overview({ detail }: { detail: ErrorSnapshotDetail }) {
  const disposition = snapshotDisposition(detail)
  const rows: Array<[string, string | number]> = [
    ['快照 ID', detail.snapshotId],
    ['Trace ID', detail.traceId],
    ['时间', displayTime(detail.ts)],
    ['模型', detail.model],
    ['最终状态', detail.finalStatus],
    ['错误类型', detail.errorType],
    ['HTTP', detail.httpStatus ?? '—'],
    ['凭据', detail.finalCredentialId],
    ['端点', detail.endpoint ?? '—'],
    ['原始大小', formatBytes(detail.originalBytes)],
    ['压缩大小', formatBytes(detail.compressedBytes)],
  ]

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap gap-2">
        <Badge variant={severityVariant(detail.severity)}>{severityLabel(detail.severity)}</Badge>
        <Badge variant={disposition === 'final_error' ? 'destructive' : disposition === 'recovered' ? 'warning' : 'secondary'}>
          {dispositionLabel(disposition)}
        </Badge>
        {detail.recovered && <Badge variant="success">重试已恢复</Badge>}
        {detail.pinned && <Badge variant="outline">已固定</Badge>}
        {detail.retentionExempt && <Badge variant="outline">永久保留</Badge>}
        {detail.omittedDueToDiskPressure && <Badge variant="destructive">磁盘压力降级</Badge>}
      </div>
      {detail.errorMessage && (
        <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
          {detail.errorMessage}
        </div>
      )}
      <dl className="grid gap-2 text-sm sm:grid-cols-2">
        {rows.map(([label, value]) => (
          <div key={label} className="rounded-lg border border-border/50 bg-secondary/20 p-3">
            <dt className="text-xs text-muted-foreground">{label}</dt>
            <dd className="mt-1 break-all font-mono text-xs">{value}</dd>
          </div>
        ))}
      </dl>
    </div>
  )
}

function PayloadMetaButton({
  active,
  meta,
  onSelect,
}: {
  active: boolean
  meta: ErrorSnapshotPayloadMeta
  onSelect: () => void
}) {
  return (
    <Button size="sm" variant={active ? 'default' : 'outline'} onClick={onSelect}>
      #{meta.seq}
      {meta.attempt != null ? ` · 尝试 ${meta.attempt}` : ''}
    </Button>
  )
}

export function ErrorSnapshotDialog({
  snapshotId,
  open,
  onOpenChange,
}: ErrorSnapshotDialogProps) {
  const [tab, setTab] = useState<SnapshotTab>('overview')
  const [selectedSeq, setSelectedSeq] = useState<number | null>(null)
  const detailQuery = useErrorSnapshot(snapshotId, open)
  const detail = detailQuery.data
  const payloads = useMemo(
    () => (tab === 'overview' ? [] : detail?.payloads.filter((payload) => payload.kind === tab) ?? []),
    [detail?.payloads, tab],
  )
  const activeSeq = payloads.some((payload) => payload.seq === selectedSeq)
    ? selectedSeq
    : payloads[0]?.seq ?? null
  const payloadQuery = useErrorSnapshotPayload(snapshotId, activeSeq, open && tab !== 'overview')
  const pinMutation = usePinErrorSnapshot()
  const unpinMutation = useUnpinErrorSnapshot()
  const deleteMutation = useDeleteErrorSnapshot()
  const downloadMutation = useDownloadErrorSnapshot()
  const confirm = useConfirm()

  useEffect(() => {
    setTab('overview')
    setSelectedSeq(null)
  }, [snapshotId])

  useEffect(() => {
    if (!open) {
      setTab('overview')
      setSelectedSeq(null)
    }
  }, [open])

  const runMutation = (
    mutation: typeof pinMutation | typeof unpinMutation,
    successMessage: string,
  ) => {
    if (!snapshotId) return
    mutation.mutate(snapshotId, {
      onSuccess: () => toast.success(successMessage),
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  const handleDelete = async () => {
    if (!snapshotId) return
    const accepted = await confirm({
      title: '删除错误快照',
      description: '将删除该快照及其全部脱敏 payload，此操作不可恢复。',
      confirmText: '删除',
      destructive: true,
    })
    if (!accepted) return
    deleteMutation.mutate(snapshotId, {
      onSuccess: () => {
        toast.success('错误快照已删除')
        onOpenChange(false)
      },
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  const handleDownload = () => {
    if (!snapshotId) return
    downloadMutation.mutate(snapshotId, {
      onSuccess: (blob) => {
        const url = URL.createObjectURL(blob)
        const link = document.createElement('a')
        link.href = url
        link.download = `error-snapshot-${snapshotId}.json`
        document.body.appendChild(link)
        link.click()
        link.remove()
        window.setTimeout(() => URL.revokeObjectURL(url), 0)
      },
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  const handleCopy = async () => {
    if (payloadQuery.data == null) return
    try {
      await navigator.clipboard.writeText(contentText(payloadQuery.data.content))
      toast.success('当前 payload 已复制')
    } catch (error) {
      toast.error('复制失败：' + extractErrorMessage(error))
    }
  }

  const pending = pinMutation.isPending || unpinMutation.isPending

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[92vh] flex-col sm:max-w-6xl">
        <DialogHeader>
          <DialogTitle>错误快照</DialogTitle>
          <DialogDescription className="break-all font-mono text-xs">
            {snapshotId ?? '未选择快照'}
          </DialogDescription>
        </DialogHeader>

        <div className="flex flex-wrap items-center gap-2">
          <Button
            size="sm"
            variant="outline"
            onClick={handleCopy}
            disabled={tab === 'overview' || payloadQuery.data == null}
          >
            <Copy className="h-3.5 w-3.5" />
            复制当前 payload
          </Button>
          <Button size="sm" variant="outline" onClick={handleDownload} disabled={!snapshotId || downloadMutation.isPending}>
            {downloadMutation.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Download className="h-3.5 w-3.5" />}
            下载完整快照
          </Button>
          {detail?.pinned ? (
            <Button size="sm" variant="outline" disabled={pending} onClick={() => runMutation(unpinMutation, '已取消固定')}>
              <PinOff className="h-3.5 w-3.5" />
              取消固定
            </Button>
          ) : (
            <Button size="sm" variant="outline" disabled={pending || !detail} onClick={() => runMutation(pinMutation, '快照已固定')}>
              <Pin className="h-3.5 w-3.5" />
              固定保留
            </Button>
          )}
          <Button
            size="sm"
            variant="outline"
            className="text-destructive hover:text-destructive"
            disabled={!detail || deleteMutation.isPending}
            onClick={handleDelete}
          >
            <Trash2 className="h-3.5 w-3.5" />
            删除
          </Button>
        </div>

        <div className="flex gap-1 overflow-x-auto border-b border-border/60 pb-2">
          {TABS.map((item) => {
            const count = item.key === 'overview'
              ? null
              : detail?.payloads.filter((payload) => payload.kind === item.key).length ?? 0
            return (
              <Button
                key={item.key}
                size="sm"
                variant={tab === item.key ? 'default' : 'ghost'}
                className="shrink-0"
                onClick={() => {
                  setTab(item.key)
                  setSelectedSeq(null)
                }}
              >
                {item.label}{count ? ` (${count})` : ''}
              </Button>
            )
          })}
        </div>

        <div className="min-h-0 flex-1 overflow-auto pr-1">
          {detailQuery.isLoading ? (
            <div className="flex items-center justify-center gap-2 py-12 text-sm text-muted-foreground">
              <Loader2 className="h-4 w-4 animate-spin" />
              加载快照详情…
            </div>
          ) : detailQuery.isError ? (
            <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
              加载失败：{extractErrorMessage(detailQuery.error)}
            </div>
          ) : detail && tab === 'overview' ? (
            <Overview detail={detail} />
          ) : payloads.length === 0 ? (
            <div className="py-12 text-center text-sm text-muted-foreground">该类型没有 payload</div>
          ) : (
            <div className="space-y-3">
              {payloads.length > 1 && (
                <div className="flex flex-wrap gap-2">
                  {payloads.map((meta) => (
                    <PayloadMetaButton
                      key={meta.seq}
                      active={meta.seq === activeSeq}
                      meta={meta}
                      onSelect={() => setSelectedSeq(meta.seq)}
                    />
                  ))}
                </div>
              )}
              {payloadQuery.isLoading ? (
                <div className="flex items-center gap-2 py-8 text-sm text-muted-foreground">
                  <Loader2 className="h-4 w-4 animate-spin" />
                  解压 payload…
                </div>
              ) : payloadQuery.isError ? (
                <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
                  加载 payload 失败：{extractErrorMessage(payloadQuery.error)}
                </div>
              ) : payloadQuery.data ? (
                <div className="space-y-2">
                  <div className="flex flex-wrap gap-2 text-xs text-muted-foreground">
                    <Badge variant="outline">{payloadQuery.data.contentType}</Badge>
                    <span>{formatBytes(payloadQuery.data.originalBytes)} → {formatBytes(payloadQuery.data.compressedBytes)}</span>
                    <span className="break-all font-mono">SHA-256 {payloadQuery.data.sha256}</span>
                  </div>
                  <pre className="max-h-[54vh] overflow-auto whitespace-pre-wrap break-all rounded-lg border border-border/50 bg-secondary/25 p-4 font-mono text-xs leading-relaxed">
                    {contentText(payloadQuery.data.content)}
                  </pre>
                </div>
              ) : null}
            </div>
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
