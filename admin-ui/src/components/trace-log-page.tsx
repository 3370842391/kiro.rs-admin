import { useState } from 'react'
import { toast } from 'sonner'
import {
  ScrollText,
  RefreshCw,
  ChevronRight,
  ChevronLeft,
  ChevronDown,
  AlertTriangle,
  CheckCircle2,
  Unplug,
  Settings2,
  Trash2,
} from 'lucide-react'
import { Card, CardContent } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  DropdownMenu,
  DropdownMenuTrigger,
  DropdownMenuContent,
  DropdownMenuLabel,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'
import {
  Tooltip,
  TooltipContent,
  TooltipProvider,
  TooltipTrigger,
} from '@/components/ui/tooltip'
import {
  Select as UiSelect,
  SelectTrigger as UiSelectTrigger,
  SelectValue as UiSelectValue,
  SelectContent as UiSelectContent,
  SelectItem as UiSelectItem,
} from '@/components/ui/select'
import { useTraces, useClearTraces } from '@/hooks/use-traces'
import { useClientKeys } from '@/hooks/use-client-keys'
import { useGroupOptions } from '@/hooks/use-groups'
import { useConfirm } from '@/components/ui/confirm-dialog'
import {
  useLogGovernanceConfig,
  useSetLogGovernanceConfig,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'
import type { TraceAttempt, TraceQuery, TraceRecord } from '@/types/api'
import { ErrorSnapshotDialog } from '@/components/error-snapshot-dialog'

/** 失败分类 → 中文标签 + Badge 颜色 */
function outcomeStyle(outcome: string): {
  label: string
  variant: 'default' | 'secondary' | 'destructive' | 'outline' | 'success' | 'warning'
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
    case 'stream_interrupted':
      return { label: '流中断', variant: 'warning' }
    default:
      return { label: outcome || '未知', variant: 'secondary' }
  }
}

/** 最终状态 → 徽章 */
function StatusBadge({ status }: { status: string }) {
  if (status === 'success')
    return (
      <Badge variant="success">
        <CheckCircle2 className="mr-1 h-3 w-3" />
        成功
      </Badge>
    )
  if (status === 'interrupted')
    return (
      <Badge variant="warning">
        <Unplug className="mr-1 h-3 w-3" />
        中断
      </Badge>
    )
  return (
    <Badge variant="destructive">
      <AlertTriangle className="mr-1 h-3 w-3" />
      失败
    </Badge>
  )
}

function formatTime(ts: string): string {
  const d = new Date(ts)
  if (isNaN(d.getTime())) return ts
  return d.toLocaleString('zh-CN', { hour12: false })
}

function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`
  return `${(ms / 1000).toFixed(2)}s`
}

function formatTokens(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(2) + 'M'
  if (n >= 1_000) return (n / 1_000).toFixed(1) + 'K'
  return String(n)
}

/** 千位分隔的完整数值（用于明细悬浮框） */
function formatTokenFull(n: number): string {
  return n.toLocaleString('en-US')
}

function credLabel(id: number, email?: string | null): string {
  if (id === 0) return '—'
  return email ? email : `#${id}`
}

function keyLabel(keyId: number, keyName?: string | null): string {
  if (keyName) return keyName
  return `#${keyId}`
}

const STATUS_OPTIONS = [
  { value: '', label: '全部状态' },
  { value: 'success', label: '成功' },
  { value: 'error', label: '失败' },
  { value: 'interrupted', label: '中断' },
]

const ERROR_TYPE_OPTIONS = [
  { value: '', label: '全部错误类型' },
  { value: 'quota_exhausted', label: '额度耗尽' },
  { value: 'account_throttled', label: '账号风控' },
  { value: 'auth_failed', label: '鉴权失败' },
  { value: 'transient', label: '瞬态错误' },
  { value: 'network_error', label: '网络错误' },
  { value: 'bad_request', label: '请求错误' },
  { value: 'stream_interrupted', label: '流中断' },
  { value: 'unknown', label: '未知' },
]

/** 单跳明细行 */
function AttemptRow({ a }: { a: TraceAttempt }) {
  const style = outcomeStyle(a.outcome)
  return (
    <div className="rounded-lg border border-border/50 bg-secondary/30 p-3">
      <div className="flex flex-wrap items-center gap-2 text-[13px]">
        <span className="font-mono text-muted-foreground">#{a.attempt}</span>
        <Badge variant={style.variant}>{style.label}</Badge>
        <span className="text-muted-foreground">凭据</span>
        <span className="font-medium">{credLabel(a.credentialId, a.email)}</span>
        {a.endpoint && <Badge variant="outline">{a.endpoint}</Badge>}
        <span className="text-muted-foreground">HTTP</span>
        <span className="font-mono">{a.httpStatus ?? '—'}</span>
        <span className="ml-auto font-mono text-muted-foreground">
          {formatDuration(a.durationMs)}
        </span>
      </div>
      {a.errorSnippet && (
        <pre className="mt-2 max-h-40 overflow-auto whitespace-pre-wrap break-all rounded-md bg-background/60 p-2 font-mono text-[11px] text-muted-foreground">
          {a.errorSnippet}
        </pre>
      )}
    </div>
  )
}

/** 1M 上下文判定阈值：标准 Claude 窗口 200K，prompt 超过即说明扩展窗口在生效 */
const CONTEXT_1M_INPUT_THRESHOLD = 200_000

/**
 * 思考徽章（对齐 Kiro-Go 的 `effort || schema` 兜底）：
 * - 有具体档位 → 显 high/xhigh/max（high 淡色、xhigh 橙、max 红，高成本档更醒目）
 * - 请求了推理但没解析出档位（如非 opus 模型）→ 显淡色「思考」
 * - 没请求推理 → 不渲染（裸流量不误标）
 */
function EffortBadge({ effort, thinking }: { effort?: string | null; thinking?: boolean }) {
  const e = (effort ?? '').trim().toLowerCase()
  if (e) {
    const variant: 'secondary' | 'warning' | 'destructive' =
      e === 'max' ? 'destructive' : e === 'xhigh' ? 'warning' : 'secondary'
    return (
      <Badge variant={variant} className="ml-1.5" title={`思考档位：${e}`}>
        {e}
      </Badge>
    )
  }
  if (thinking) {
    return (
      <Badge variant="secondary" className="ml-1.5" title="客户端请求了推理，但未解析出具体档位">
        思考
      </Badge>
    )
  }
  return null
}

/**
 * 1M 扩展上下文徽章：客户端声明 beta 头，或 prompt 实测 > 200K（扩展窗口已生效）时显示。
 *
 * 注意用 **prompt 总量** = input + cacheCreation + cacheRead 判定，而非 input 单值：
 * 缓存模拟 / 命中率整形会把 token 从 input 挪进 cache_read，input 单值会被压到 200K 以下，
 * 但这三者之和（真实 prompt 规模）恒定，故用它判定才不受缓存整形影响。
 */
function Context1mBadge({ rec }: { rec: TraceRecord }) {
  const declared = rec.context1m === true
  const promptTotal =
    (rec.inputTokens ?? 0) + (rec.cacheCreationTokens ?? 0) + (rec.cacheReadTokens ?? 0)
  const overStd = promptTotal > CONTEXT_1M_INPUT_THRESHOLD
  if (!declared && !overStd) return null
  const title = declared
    ? '客户端声明 1M 扩展上下文（anthropic-beta: context-1m）'
    : `prompt 超过 200K（标准窗口上限），扩展上下文已生效`
  return (
    <Badge variant="success" className="ml-1.5" title={title}>
      1M{!declared && overStd ? '?' : ''}
    </Badge>
  )
}

/**
 * 缓存命中率徽章：`缓存读/(输入+缓存读)`，与概览图表 calcHitRate 同口径（不含缓存写）。
 * cacheRead=0（冷启动/无缓存）时不渲染，避免满屏 0%。纯展示，用行内已有的 token 数现算。
 */
function CacheHitRateBadge({ rec }: { rec: TraceRecord }) {
  const input = rec.inputTokens ?? 0
  const cacheRead = rec.cacheReadTokens ?? 0
  const denom = input + cacheRead
  if (cacheRead <= 0 || denom <= 0) return null
  const pct = Math.round((cacheRead / denom) * 100)
  return (
    <Badge
      variant="outline"
      className="ml-1.5"
      style={{
        borderColor: 'transparent',
        backgroundColor: 'rgba(6,182,212,0.12)',
        color: '#0891b2',
      }}
      title={`缓存命中率 = 缓存读/(输入+缓存读) = ${pct}%（与概览同口径，不含缓存写）`}
    >
      缓存 {pct}%
    </Badge>
  )
}

/** Token 用量单元格：紧凑展示总量，hover 显示分项明细 */
function TokenCell({ rec }: { rec: TraceRecord }) {
  const input = rec.inputTokens ?? 0
  const output = rec.outputTokens ?? 0
  const cacheCreation = rec.cacheCreationTokens ?? 0
  const cacheRead = rec.cacheReadTokens ?? 0
  const total = rec.totalTokens ?? input + output + cacheCreation + cacheRead
  // 全 0（早期失败、未走到上游）时不显示明细，仅占位
  if (total === 0) {
    return <span className="text-muted-foreground">—</span>
  }
  const rows: Array<[string, number]> = [
    ['输入 Token', input],
    ['输出 Token', output],
  ]
  if (cacheCreation > 0) rows.push(['缓存创建 Token', cacheCreation])
  if (cacheRead > 0) rows.push(['缓存读取 Token', cacheRead])
  return (
    <TooltipProvider delayDuration={150}>
      <Tooltip>
        <TooltipTrigger asChild>
          <span className="inline-flex items-center gap-1 font-mono tabular-nums cursor-default border-b border-dotted border-muted-foreground/40">
            <span className="text-emerald-600 dark:text-emerald-400">
              ↓{formatTokens(input + cacheCreation + cacheRead)}
            </span>
            <span className="text-violet-600 dark:text-violet-400">
              ↑{formatTokens(output)}
            </span>
          </span>
        </TooltipTrigger>
        <TooltipContent className="p-0">
          <div className="min-w-[180px] px-3 py-2">
            <div className="mb-1.5 text-[13px] font-semibold">Token 明细</div>
            <div className="space-y-1 text-[12px]">
              {rows.map(([label, val]) => (
                <div key={label} className="flex items-center justify-between gap-6">
                  <span className="text-muted-foreground">{label}</span>
                  <span className="font-mono tabular-nums">{formatTokenFull(val)}</span>
                </div>
              ))}
              <div className="mt-1 flex items-center justify-between gap-6 border-t border-border/50 pt-1">
                <span className="font-medium">总 Token</span>
                <span className="font-mono font-semibold tabular-nums">
                  {formatTokenFull(total)}
                </span>
              </div>
            </div>
          </div>
        </TooltipContent>
      </Tooltip>
    </TooltipProvider>
  )
}

function TraceRow({ rec }: { rec: TraceRecord }) {
  const [open, setOpen] = useState(false)
  const errStyle = rec.errorType ? outcomeStyle(rec.errorType) : null
  return (
    <>
      <tr
        className="cursor-pointer whitespace-nowrap border-b border-border/40 hover:bg-accent/40"
        onClick={() => setOpen((v) => !v)}
      >
        <td className="py-2.5 pl-3 pr-2">
          {open ? (
            <ChevronDown className="h-4 w-4 text-muted-foreground" />
          ) : (
            <ChevronRight className="h-4 w-4 text-muted-foreground" />
          )}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground whitespace-nowrap">
          {formatTime(rec.ts)}
        </td>
        <td className="py-2.5 pr-3 text-[13px]">
          <span className="inline-block max-w-[220px] truncate align-middle">{rec.model}</span>
          {rec.isStream && <Badge variant="outline" className="ml-1.5">流式</Badge>}
          <EffortBadge effort={rec.reasoningEffort} thinking={rec.thinking} />
          <Context1mBadge rec={rec} />
          <CacheHitRateBadge rec={rec} />
        </td>
        <td className="py-2.5 pr-3 text-[13px]">
          <Badge variant="outline">{keyLabel(rec.keyId, rec.keyName)}</Badge>
        </td>
        <td className="py-2.5 pr-3">
          <StatusBadge status={rec.finalStatus} />
        </td>
        <TraceCredentialCell rec={rec} />
        <td className="py-2.5 pr-3 text-[12px] tabular-nums">
          <TokenCell rec={rec} />
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums">
          {rec.credits != null && rec.credits > 0 ? rec.credits.toFixed(4) : '—'}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground">
          {rec.firstTokenMs != null ? formatDuration(rec.firstTokenMs) : '—'}
        </td>
        <td className="py-2.5 pr-3">
          {errStyle ? <Badge variant={errStyle.variant}>{errStyle.label}</Badge> : '—'}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums">
          {Math.max(0, rec.totalAttempts - 1)}
        </td>
        <td className="py-2.5 pr-3 text-[13px] tabular-nums text-muted-foreground">
          {formatDuration(rec.durationMs)}
        </td>
      </tr>
      {open && <ExpandedTraceRow rec={rec} />}
    </>
  )
}

function TraceCredentialCell({ rec }: { rec: TraceRecord }) {
  return (
    <td className="py-2.5 pr-3 text-[13px]">
      <span className="inline-block max-w-[220px] truncate align-middle">
        {credLabel(rec.finalCredentialId, rec.finalEmail)}
      </span>
    </td>
  )
}

function ExpandedTraceRow({ rec }: { rec: TraceRecord }) {
  return (
    <tr className="border-b border-border/40 bg-secondary/20">
      <td colSpan={12} className="px-3 py-3">
        <ExpandedDetail rec={rec} />
      </td>
    </tr>
  )
}

/** 展开后的链路详情：错误摘要 + 每跳时间线 */
function ExpandedDetail({ rec }: { rec: TraceRecord }) {
  const [snapshotOpen, setSnapshotOpen] = useState(false)
  return (
    <div className="space-y-3">
      {rec.errorMessage && (
        <div className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-[13px] text-destructive">
          {rec.errorMessage}
        </div>
      )}
      {rec.interruptedAfterBytes != null && (
        <div className="text-[12px] text-muted-foreground">
          中断前已发送 {rec.interruptedAfterBytes} 字节
        </div>
      )}
      {rec.snapshotId && (
        <>
          <Button size="sm" variant="outline" onClick={() => setSnapshotOpen(true)}>
            <AlertTriangle className="h-3.5 w-3.5" />
            查看错误快照
          </Button>
          <ErrorSnapshotDialog
            snapshotId={rec.snapshotId}
            open={snapshotOpen}
            onOpenChange={setSnapshotOpen}
          />
        </>
      )}
      <div className="text-[12px] font-medium text-muted-foreground">
        尝试链路（{rec.attempts.length} 次
        {rec.attempts.length > 1 ? `，含 ${rec.attempts.length - 1} 次重试` : "，未重试"}）
      </div>
      <div className="space-y-2">
        {rec.attempts.length === 0 ? (
          <div className="text-[13px] text-muted-foreground">无尝试记录（请求未到达上游）</div>
        ) : (
          rec.attempts.map((a) => <AttemptRow key={a.attempt} a={a} />)
        )}
      </div>
    </div>
  )
}

/** 下拉筛选器 */
function Select({
  value,
  onChange,
  options,
}: {
  value: string
  onChange: (v: string) => void
  options: { value: string; label: string }[]
}) {
  // radix Select 不允许空字符串 value，用哨兵 "__all__" 代表「空/全部」，对外透明。
  const SENTINEL = '__all__'
  return (
    <UiSelect
      value={value === '' ? SENTINEL : value}
      onValueChange={(v) => onChange(v === SENTINEL ? '' : v)}
    >
      <UiSelectTrigger className="h-8 w-auto min-w-[120px]">
        <UiSelectValue />
      </UiSelectTrigger>
      <UiSelectContent>
        {options.map((o) => (
          <UiSelectItem key={o.value} value={o.value === '' ? SENTINEL : o.value}>
            {o.label}
          </UiSelectItem>
        ))}
      </UiSelectContent>
    </UiSelect>
  )
}

/** 日志治理设置下拉：trace、usage 与错误快照的运行时治理。 */
function GovernanceButton() {
  const [open, setOpen] = useState(false)
  const { data: cfg, isLoading } = useLogGovernanceConfig()
  const { mutate, isPending } = useSetLogGovernanceConfig()
  const [traceDays, setTraceDays] = useState('')
  const [usageDays, setUsageDays] = useState('')
  const [snapshotDays, setSnapshotDays] = useState('')
  const [snapshotMaxGb, setSnapshotMaxGb] = useState('')
  const [snapshotMinFreeGb, setSnapshotMinFreeGb] = useState('')

  const enabled = cfg?.traceEnabled ?? true
  const errorSnapshotEnabled = cfg?.errorSnapshotEnabled ?? true
  const errorSnapshotCaptureRecovered = cfg?.errorSnapshotCaptureRecovered ?? true
  const errorSnapshotCaptureBodies = cfg?.errorSnapshotCaptureBodies ?? true

  const save = (patch: Record<string, unknown>, ok: string) => {
    mutate(patch, {
      onSuccess: () => toast.success(ok),
      onError: (err) => toast.error('保存失败：' + extractErrorMessage(err)),
    })
  }

  const submitNumber = (
    e: React.FormEvent,
    field:
      | 'traceRetentionDays'
      | 'usageLogRetentionDays'
      | 'errorSnapshotRetentionDays'
      | 'errorSnapshotMaxStorageGb'
      | 'errorSnapshotMinFreeDiskGb',
    raw: string,
    max: number,
    label: string,
    reset: () => void,
  ) => {
    e.preventDefault()
    const n = parseInt(raw, 10)
    if (isNaN(n) || n < 1 || n > max) {
      toast.error(`${label}需在 1..=${max}`)
      return
    }
    save({ [field]: n }, `${label}已更新`)
    reset()
  }

  return (
    <DropdownMenu open={open} onOpenChange={setOpen}>
      <DropdownMenuTrigger asChild>
        <Button size="sm" variant="outline">
          <Settings2 className="h-3.5 w-3.5" />
          治理设置
        </Button>
      </DropdownMenuTrigger>
      <DropdownMenuContent align="end" className="max-h-[80vh] w-80 overflow-y-auto">
        <DropdownMenuLabel>请求链路追踪</DropdownMenuLabel>
        <div className="px-2 pb-2">
          <div className="flex items-center justify-between gap-2 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium text-foreground">
                {enabled ? '已启用' : '已关闭'}
              </div>
              <div className="leading-snug text-muted-foreground">
                {enabled
                  ? '记录每次请求的完整重试链路到 traces.db'
                  : '不再写入新链路（历史记录仍可查询）'}
              </div>
            </div>
            <Switch
              checked={enabled}
              disabled={isLoading || isPending}
              onCheckedChange={(v) =>
                save({ traceEnabled: v }, v ? '已开启链路追踪' : '已关闭链路追踪')
              }
            />
          </div>
        </div>
        <DropdownMenuLabel className="pt-1">
          trace 保留天数（当前 {cfg?.traceRetentionDays ?? '—'}）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitNumber(e, 'traceRetentionDays', traceDays, 365, 'trace 保留天数', () => setTraceDays(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={365}
            placeholder="天数"
            value={traceDays}
            onChange={(e) => setTraceDays(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !traceDays.trim()}>
            保存
          </Button>
        </form>
        <DropdownMenuLabel className="pt-1">
          usage 日志保留天数（当前 {cfg?.usageLogRetentionDays ?? '—'}）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitNumber(e, 'usageLogRetentionDays', usageDays, 365, 'usage 保留天数', () => setUsageDays(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={365}
            placeholder="天数"
            value={usageDays}
            onChange={(e) => setUsageDays(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !usageDays.trim()}>
            保存
          </Button>
        </form>
        <DropdownMenuSeparator />
        <DropdownMenuLabel>错误快照</DropdownMenuLabel>
        <div className="space-y-2 px-2 pb-2">
          <div className="flex items-center justify-between gap-3 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium text-foreground">保存错误快照</div>
              <div className="leading-snug text-muted-foreground">
                关闭只停止 error_snapshots.db 写入，不影响 traces.db。
              </div>
            </div>
            <Switch
              checked={errorSnapshotEnabled}
              disabled={isLoading || isPending}
              onCheckedChange={(value) => save(
                { errorSnapshotEnabled: value },
                value ? '已开启错误快照' : '已关闭错误快照',
              )}
            />
          </div>
          <div className="flex items-center justify-between gap-3 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium text-foreground">捕获重试后恢复的请求</div>
              <div className="leading-snug text-muted-foreground">
                保留首次失败但最终恢复的诊断现场。
              </div>
            </div>
            <Switch
              checked={errorSnapshotCaptureRecovered}
              disabled={isLoading || isPending}
              onCheckedChange={(value) => save(
                { errorSnapshotCaptureRecovered: value },
                value ? '已捕获恢复请求' : '已忽略恢复请求',
              )}
            />
          </div>
          <div className="flex items-center justify-between gap-3 rounded-md bg-secondary/40 px-2.5 py-2">
            <div className="text-xs">
              <div className="font-medium text-foreground">保存脱敏正文</div>
              <div className="leading-snug text-muted-foreground">
                关闭后仍保存元数据、工具诊断、上游错误和流尾。
              </div>
            </div>
            <Switch
              checked={errorSnapshotCaptureBodies}
              disabled={isLoading || isPending}
              onCheckedChange={(value) => save(
                { errorSnapshotCaptureBodies: value },
                value ? '已保存脱敏正文' : '已关闭正文保存',
              )}
            />
          </div>
        </div>
        <DropdownMenuLabel className="pt-1">
          快照保留天数（当前 {cfg?.errorSnapshotRetentionDays ?? '—'}）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitNumber(e, 'errorSnapshotRetentionDays', snapshotDays, 3650, '快照保留天数', () => setSnapshotDays(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={3650}
            placeholder="1..3650 天"
            value={snapshotDays}
            onChange={(e) => setSnapshotDays(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !snapshotDays.trim()}>
            保存
          </Button>
        </form>
        <DropdownMenuLabel className="pt-1">
          最大存储（当前 {cfg?.errorSnapshotMaxStorageGb ?? '—'} GB）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitNumber(e, 'errorSnapshotMaxStorageGb', snapshotMaxGb, 900, '最大存储 GB', () => setSnapshotMaxGb(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={900}
            placeholder="1..900 GB"
            value={snapshotMaxGb}
            onChange={(e) => setSnapshotMaxGb(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !snapshotMaxGb.trim()}>
            保存
          </Button>
        </form>
        <DropdownMenuLabel className="pt-1">
          最小空闲磁盘（当前 {cfg?.errorSnapshotMinFreeDiskGb ?? '—'} GB）
        </DropdownMenuLabel>
        <form
          onSubmit={(e) => submitNumber(e, 'errorSnapshotMinFreeDiskGb', snapshotMinFreeGb, 900, '最小空闲磁盘 GB', () => setSnapshotMinFreeGb(''))}
          className="flex items-center gap-1.5 px-2 pb-2"
        >
          <Input
            type="number"
            min={1}
            max={900}
            placeholder="1..900 GB"
            value={snapshotMinFreeGb}
            onChange={(e) => setSnapshotMinFreeGb(e.target.value)}
            disabled={isPending}
            className="h-7 text-xs"
          />
          <Button type="submit" size="sm" variant="outline" className="h-7 text-xs" disabled={isPending || !snapshotMinFreeGb.trim()}>
            保存
          </Button>
        </form>
      </DropdownMenuContent>
    </DropdownMenu>
  )
}


const PAGE_SIZE = 50

export function TraceLogPage() {
  const [status, setStatus] = useState('')
  const [errorType, setErrorType] = useState('')
  const [keyId, setKeyId] = useState('')
  const [group, setGroup] = useState('')
  const [onlyFailed, setOnlyFailed] = useState(false)
  const [page, setPage] = useState(0)

  const { data: keysData } = useClientKeys()
  const keyOptions = [
    { value: '', label: '全部 Key' },
    ...(keysData?.keys ?? []).map((k) => ({ value: String(k.id), label: k.name })),
  ]

  const groupOptions = useGroupOptions()
  const groupSelectOptions = [
    { value: '', label: '全部分组' },
    ...groupOptions.map((g) => ({ value: g, label: g })),
  ]

  // 筛选条件变化时回到第一页
  const resetTo = <T,>(setter: (v: T) => void) => (v: T) => {
    setter(v)
    setPage(0)
  }

  const query: TraceQuery = {
    status: status || undefined,
    errorType: errorType || undefined,
    keyId: keyId ? Number(keyId) : undefined,
    group: group || undefined,
    onlyFailed: onlyFailed || undefined,
    limit: PAGE_SIZE,
    offset: page * PAGE_SIZE,
  }
  const { data, isLoading, isFetching, refetch } = useTraces(query)
  const records = data?.records ?? []
  const total = data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / PAGE_SIZE))

  const confirm = useConfirm()
  const clearTraces = useClearTraces()
  const handleClear = async () => {
    const ok = await confirm({
      title: '清空请求日志',
      description: '将删除全部链路记录，不可恢复。',
      confirmText: '清空',
      destructive: true,
    })
    if (!ok) return
    clearTraces.mutate(undefined, {
      onSuccess: (n) => {
        toast.success(`已清空请求日志（${n} 条）`)
        setPage(0)
      },
      onError: (err) => toast.error('清空失败：' + extractErrorMessage(err)),
    })
  }

  return (
    <div className="space-y-5">
      {/* 筛选栏 */}
      <div className="flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <ScrollText className="h-5 w-5 text-muted-foreground" />
          <h2 className="text-lg font-semibold tracking-tight">请求日志</h2>
          {total > 0 && <Badge variant="secondary">{total}</Badge>}
        </div>
        <div className="ml-auto flex flex-wrap items-center gap-2">
          <Select value={keyId} onChange={resetTo(setKeyId)} options={keyOptions} />
          <Select value={group} onChange={resetTo(setGroup)} options={groupSelectOptions} />
          <Select value={status} onChange={resetTo(setStatus)} options={STATUS_OPTIONS} />
          <Select
            value={errorType}
            onChange={resetTo(setErrorType)}
            options={ERROR_TYPE_OPTIONS}
          />
          <Button
            size="sm"
            variant={onlyFailed ? 'default' : 'outline'}
            onClick={() => {
              setOnlyFailed((v) => !v)
              setPage(0)
            }}
          >
            只看失败
          </Button>
          <GovernanceButton />
          <Button size="sm" variant="outline" onClick={() => refetch()} disabled={isFetching}>
            <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
            刷新
          </Button>
          <Button
            size="sm"
            variant="outline"
            onClick={handleClear}
            disabled={clearTraces.isPending || total === 0}
            className="text-destructive hover:text-destructive"
          >
            <Trash2 className="h-3.5 w-3.5" />
            清空
          </Button>
        </div>
      </div>

      <Card>
        <CardContent className="p-0">
          {isLoading ? (
            <div className="p-6 text-sm text-muted-foreground">加载中…</div>
          ) : records.length === 0 ? (
            <div className="p-6 text-sm text-muted-foreground">
              暂无记录。发起几次 /v1/messages 请求后即可看到链路。
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full min-w-[1080px] text-left">
                <thead>
                  <tr className="whitespace-nowrap border-b border-border/60 text-[12px] uppercase tracking-wider text-muted-foreground">
                    <th className="py-2 pl-3 pr-2 font-medium"></th>
                    <th className="py-2 pr-3 font-medium">时间</th>
                    <th className="py-2 pr-3 font-medium">模型</th>
                    <th className="py-2 pr-3 font-medium">入口 Key</th>
                    <th className="py-2 pr-3 font-medium">状态</th>
                    <th className="py-2 pr-3 font-medium">最终凭据</th>
                    <th className="py-2 pr-3 font-medium">Token</th>
                    <th className="py-2 pr-3 font-medium">费用</th>
                    <th className="py-2 pr-3 font-medium">首Token</th>
                    <th className="py-2 pr-3 font-medium">错误类型</th>
                    <th className="py-2 pr-3 font-medium">重试</th>
                    <th className="py-2 pr-3 font-medium">耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {records.map((rec) => (
                    <TraceRow key={rec.traceId} rec={rec} />
                  ))}
                </tbody>
              </table>
            </div>
          )}
        </CardContent>
      </Card>

      {total > PAGE_SIZE && (
        <div className="flex items-center justify-center gap-2">
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => Math.max(0, p - 1))}
            disabled={page === 0 || isFetching}
          >
            <ChevronLeft className="h-3.5 w-3.5" />
            上一页
          </Button>
          <div className="px-3 text-sm tabular-nums text-muted-foreground">
            第 <span className="font-medium text-foreground">{page + 1}</span> /{' '}
            {totalPages} 页
            <span className="mx-1.5 text-muted-foreground/50">·</span>共 {total} 条
          </div>
          <Button
            variant="outline"
            size="sm"
            onClick={() => setPage((p) => Math.min(totalPages - 1, p + 1))}
            disabled={page >= totalPages - 1 || isFetching}
          >
            下一页
            <ChevronRight className="h-3.5 w-3.5" />
          </Button>
        </div>
      )}
    </div>
  )
}




