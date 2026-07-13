import { useState } from 'react'
import {
  AlertTriangle,
  ChevronLeft,
  ChevronRight,
  Database,
  Eye,
  Loader2,
  Pin,
  PinOff,
  RefreshCw,
  ShieldAlert,
  Trash2,
} from 'lucide-react'
import { toast } from 'sonner'
import { ErrorSnapshotDialog } from '@/components/error-snapshot-dialog'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { useConfirm } from '@/components/ui/confirm-dialog'
import { Input } from '@/components/ui/input'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import {
  useCleanupErrorSnapshots,
  useDeleteErrorSnapshot,
  useErrorSnapshots,
  useErrorSnapshotStorage,
  usePinErrorSnapshot,
  useUnpinErrorSnapshot,
} from '@/hooks/use-error-snapshots'
import { formatBytes, severityLabel } from '@/lib/error-snapshot-utils'
import { extractErrorMessage } from '@/lib/utils'
import type { ErrorSnapshotQuery, ErrorSnapshotSummary, SnapshotSeverity } from '@/types/api'

const PAGE_SIZE = 50

function formatTime(value: string): string {
  const date = new Date(value)
  return Number.isNaN(date.getTime())
    ? value
    : date.toLocaleString('zh-CN', { hour12: false })
}

function epochSeconds(value: string): string | undefined {
  if (!value) return undefined
  const timestamp = new Date(value).getTime()
  return Number.isNaN(timestamp) ? undefined : String(Math.floor(timestamp / 1000))
}

function triState(value: string): boolean | undefined {
  if (value === 'true') return true
  if (value === 'false') return false
  return undefined
}

function severityVariant(severity: SnapshotSeverity) {
  if (severity === 'critical' || severity === 'error') return 'destructive' as const
  if (severity === 'warning') return 'warning' as const
  return 'secondary' as const
}

function compressionLabel(record: ErrorSnapshotSummary): string {
  if (record.originalBytes <= 0) return '—'
  const saved = Math.max(0, 1 - record.compressedBytes / record.originalBytes)
  return `${record.payloadCount} / 节省 ${Math.round(saved * 100)}%`
}

function FilterSelect({
  value,
  onChange,
  placeholder,
  options,
}: {
  value: string
  onChange: (value: string) => void
  placeholder: string
  options: Array<{ value: string; label: string }>
}) {
  const all = '__all__'
  return (
    <Select value={value || all} onValueChange={(next) => onChange(next === all ? '' : next)}>
      <SelectTrigger className="h-9 min-w-[130px]">
        <SelectValue placeholder={placeholder} />
      </SelectTrigger>
      <SelectContent>
        <SelectItem value={all}>{placeholder}</SelectItem>
        {options.map((option) => (
          <SelectItem key={option.value} value={option.value}>{option.label}</SelectItem>
        ))}
      </SelectContent>
    </Select>
  )
}

function StorageCard() {
  const storage = useErrorSnapshotStorage()
  const data = storage.data

  return (
    <Card className={data?.diskPressure ? 'border-destructive/60' : undefined}>
      <CardHeader className="pb-3">
        <div className="flex flex-wrap items-center justify-between gap-2">
          <CardTitle className="flex items-center gap-2">
            <Database className="h-4 w-4 text-muted-foreground" />
            快照存储
          </CardTitle>
          {data?.diskPressure && (
            <Badge variant="destructive">
              <AlertTriangle className="mr-1 h-3 w-3" />
              磁盘压力，正文采集已降级
            </Badge>
          )}
        </div>
      </CardHeader>
      <CardContent>
        {storage.isLoading ? (
          <div className="text-sm text-muted-foreground">加载存储状态…</div>
        ) : storage.isError ? (
          <div className="text-sm text-destructive">加载失败：{extractErrorMessage(storage.error)}</div>
        ) : data ? (
          <div className="grid gap-3 text-sm sm:grid-cols-2 lg:grid-cols-5">
            <StorageMetric label="当前占用 / 上限" value={`${formatBytes(data.totalBytes)} / ${formatBytes(data.maxStorageBytes)}`} />
            <StorageMetric label="可用磁盘 / 下限" value={`${formatBytes(data.availableBytes)} / ${formatBytes(data.minFreeDiskBytes)}`} />
            <StorageMetric label="Fallback" value={formatBytes(data.fallbackBytes)} />
            <StorageMetric label="记录数" value={String(data.records)} />
            <StorageMetric label="固定 / 严重" value={`${data.pinnedRecords} / ${data.criticalRecords}`} />
          </div>
        ) : null}
      </CardContent>
    </Card>
  )
}

function StorageMetric({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg border border-border/50 bg-secondary/20 p-3">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="mt-1 font-mono text-xs font-medium">{value}</div>
    </div>
  )
}

function SnapshotActions({
  record,
  onView,
}: {
  record: ErrorSnapshotSummary
  onView: () => void
}) {
  const pin = usePinErrorSnapshot()
  const unpin = useUnpinErrorSnapshot()
  const remove = useDeleteErrorSnapshot()
  const confirm = useConfirm()
  const pending = pin.isPending || unpin.isPending || remove.isPending

  const togglePin = () => {
    const mutation = record.pinned ? unpin : pin
    mutation.mutate(record.snapshotId, {
      onSuccess: () => toast.success(record.pinned ? '已取消固定' : '快照已固定'),
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  const handleDelete = async () => {
    const accepted = await confirm({
      title: '删除错误快照',
      description: '将删除该快照及其全部 payload，此操作不可恢复。',
      confirmText: '删除',
      destructive: true,
    })
    if (!accepted) return
    remove.mutate(record.snapshotId, {
      onSuccess: () => toast.success('错误快照已删除'),
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  return (
    <div className="flex items-center gap-1">
      <Button size="icon" variant="ghost" className="h-8 w-8" title="查看" onClick={onView}>
        <Eye className="h-3.5 w-3.5" />
      </Button>
      <Button size="icon" variant="ghost" className="h-8 w-8" title={record.pinned ? '取消固定' : '固定保留'} disabled={pending} onClick={togglePin}>
        {record.pinned ? <PinOff className="h-3.5 w-3.5" /> : <Pin className="h-3.5 w-3.5" />}
      </Button>
      <Button size="icon" variant="ghost" className="h-8 w-8 text-destructive hover:text-destructive" title="删除" disabled={pending} onClick={handleDelete}>
        <Trash2 className="h-3.5 w-3.5" />
      </Button>
    </div>
  )
}

export function ErrorSnapshotPage() {
  const [traceId, setTraceId] = useState('')
  const [model, setModel] = useState('')
  const [errorType, setErrorType] = useState('')
  const [httpStatus, setHttpStatus] = useState('')
  const [credentialId, setCredentialId] = useState('')
  const [severity, setSeverity] = useState<SnapshotSeverity | ''>('')
  const [recovered, setRecovered] = useState('')
  const [pinned, setPinned] = useState('')
  const [from, setFrom] = useState('')
  const [to, setTo] = useState('')
  const [page, setPage] = useState(0)
  const [selectedSnapshotId, setSelectedSnapshotId] = useState<string | null>(null)
  const [dialogOpen, setDialogOpen] = useState(false)

  const resetPage = (setter: (value: string) => void) => (value: string) => {
    setter(value)
    setPage(0)
  }

  const query: ErrorSnapshotQuery = {
    traceId: traceId || undefined,
    model: model || undefined,
    errorType: errorType || undefined,
    httpStatus: httpStatus ? Number(httpStatus) : undefined,
    credentialId: credentialId ? Number(credentialId) : undefined,
    severity,
    recovered: triState(recovered),
    pinned: triState(pinned),
    from: epochSeconds(from),
    to: epochSeconds(to),
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  }
  const snapshots = useErrorSnapshots(query)
  const cleanup = useCleanupErrorSnapshots()
  const confirm = useConfirm()
  const records = snapshots.data?.records ?? []
  const total = snapshots.data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  const handleCleanup = async () => {
    const accepted = await confirm({
      title: '立即执行快照治理',
      description: '将按保留天数、容量与磁盘空闲策略清理可删除快照；固定和严重快照不参与普通清理。',
      confirmText: '执行清理',
      destructive: true,
    })
    if (!accepted) return
    cleanup.mutate(undefined, {
      onSuccess: () => toast.success('快照治理已执行'),
      onError: (error) => toast.error(extractErrorMessage(error)),
    })
  }

  const openSnapshot = (id: string) => {
    setSelectedSnapshotId(id)
    setDialogOpen(true)
  }

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <ShieldAlert className="h-5 w-5 text-muted-foreground" />
          <h2 className="text-lg font-semibold tracking-tight">错误快照</h2>
          {total > 0 && <Badge variant="secondary">{total}</Badge>}
        </div>
        <div className="ml-auto flex items-center gap-2">
          <Button size="sm" variant="outline" onClick={handleCleanup} disabled={cleanup.isPending}>
            {cleanup.isPending ? <Loader2 className="h-3.5 w-3.5 animate-spin" /> : <Trash2 className="h-3.5 w-3.5" />}
            立即清理
          </Button>
          <Button size="sm" variant="outline" onClick={() => snapshots.refetch()} disabled={snapshots.isFetching}>
            <RefreshCw className={`h-3.5 w-3.5 ${snapshots.isFetching ? 'animate-spin' : ''}`} />
            刷新
          </Button>
        </div>
      </div>

      <StorageCard />

      <Card>
        <CardContent className="p-4">
          <div className="grid gap-2 sm:grid-cols-2 lg:grid-cols-4 xl:grid-cols-5">
            <Input className="h-9" placeholder="Trace ID" value={traceId} onChange={(event) => resetPage(setTraceId)(event.target.value)} />
            <Input className="h-9" placeholder="模型" value={model} onChange={(event) => resetPage(setModel)(event.target.value)} />
            <Input className="h-9" placeholder="错误类型" value={errorType} onChange={(event) => resetPage(setErrorType)(event.target.value)} />
            <Input className="h-9" type="number" min={100} max={599} placeholder="HTTP 状态" value={httpStatus} onChange={(event) => resetPage(setHttpStatus)(event.target.value)} />
            <Input className="h-9" type="number" min={0} placeholder="凭据 ID" value={credentialId} onChange={(event) => resetPage(setCredentialId)(event.target.value)} />
            <FilterSelect
              value={severity}
              onChange={(value) => {
                setSeverity(value as SnapshotSeverity | '')
                setPage(0)
              }}
              placeholder="全部级别"
              options={[
                { value: 'critical', label: '严重' },
                { value: 'error', label: '错误' },
                { value: 'warning', label: '警告' },
                { value: 'info', label: '信息' },
              ]}
            />
            <FilterSelect value={recovered} onChange={resetPage(setRecovered)} placeholder="恢复状态" options={[{ value: 'true', label: '已恢复' }, { value: 'false', label: '未恢复' }]} />
            <FilterSelect value={pinned} onChange={resetPage(setPinned)} placeholder="固定状态" options={[{ value: 'true', label: '已固定' }, { value: 'false', label: '未固定' }]} />
            <Input className="h-9" type="datetime-local" aria-label="开始时间" value={from} onChange={(event) => resetPage(setFrom)(event.target.value)} />
            <Input className="h-9" type="datetime-local" aria-label="结束时间" value={to} onChange={(event) => resetPage(setTo)(event.target.value)} />
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="p-0">
          {snapshots.isLoading ? (
            <div className="p-6 text-sm text-muted-foreground">加载中…</div>
          ) : snapshots.isError ? (
            <div className="p-6 text-sm text-destructive">加载失败：{extractErrorMessage(snapshots.error)}</div>
          ) : records.length === 0 ? (
            <div className="p-6 text-sm text-muted-foreground">暂无符合条件的错误快照。</div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[1100px] text-left">
                <thead>
                  <tr className="whitespace-nowrap border-b border-border/60 text-xs text-muted-foreground">
                    <th className="px-3 py-2 font-medium">时间</th>
                    <th className="pr-3 py-2 font-medium">级别</th>
                    <th className="pr-3 py-2 font-medium">错误类型</th>
                    <th className="pr-3 py-2 font-medium">HTTP</th>
                    <th className="pr-3 py-2 font-medium">模型</th>
                    <th className="pr-3 py-2 font-medium">凭据</th>
                    <th className="pr-3 py-2 font-medium">恢复</th>
                    <th className="pr-3 py-2 font-medium">Payload / 压缩</th>
                    <th className="pr-3 py-2 font-medium">固定</th>
                    <th className="pr-3 py-2 font-medium">操作</th>
                  </tr>
                </thead>
                <tbody>
                  {records.map((record) => (
                    <tr key={record.snapshotId} className="border-b border-border/40 text-sm hover:bg-accent/30">
                      <td className="whitespace-nowrap px-3 py-2.5 text-xs text-muted-foreground">{formatTime(record.ts)}</td>
                      <td className="pr-3 py-2.5"><Badge variant={severityVariant(record.severity)}>{severityLabel(record.severity)}</Badge></td>
                      <td className="pr-3 py-2.5"><span className="inline-block max-w-[220px] truncate align-middle" title={record.errorType}>{record.errorType}</span></td>
                      <td className="pr-3 py-2.5 font-mono text-xs">{record.httpStatus ?? '—'}</td>
                      <td className="pr-3 py-2.5"><span className="inline-block max-w-[220px] truncate align-middle" title={record.model}>{record.model}</span></td>
                      <td className="pr-3 py-2.5 font-mono text-xs">#{record.finalCredentialId}</td>
                      <td className="pr-3 py-2.5">{record.recovered ? <Badge variant="success">是</Badge> : '—'}</td>
                      <td className="pr-3 py-2.5 text-xs text-muted-foreground">{compressionLabel(record)}</td>
                      <td className="pr-3 py-2.5">{record.pinned || record.retentionExempt ? <Badge variant="outline">{record.retentionExempt ? '永久' : '固定'}</Badge> : '—'}</td>
                      <td className="pr-3 py-2.5"><SnapshotActions record={record} onView={() => openSnapshot(record.snapshotId)} /></td>
                    </tr>
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {total > PAGE_SIZE && (
        <div className="flex items-center justify-center gap-2">
          <Button size="sm" variant="outline" onClick={() => setPage((current) => Math.max(0, current - 1))} disabled={page === 0 || snapshots.isFetching}>
            <ChevronLeft className="h-3.5 w-3.5" />
            上一页
          </Button>
          <span className="text-sm text-muted-foreground">第 {page + 1} / {totalPages} 页 · 共 {total} 条</span>
          <Button size="sm" variant="outline" onClick={() => setPage((current) => Math.min(totalPages - 1, current + 1))} disabled={page >= totalPages - 1 || snapshots.isFetching}>
            下一页
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      )}

      <ErrorSnapshotDialog
        snapshotId={selectedSnapshotId}
        open={dialogOpen}
        onOpenChange={setDialogOpen}
      />
    </div>
  )
}
