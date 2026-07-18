import { useEffect, useState, type FormEvent } from 'react'
import { Database, Gauge, HardDrive, Info, Trash2 } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Progress } from '@/components/ui/progress'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { useConfirm } from '@/components/ui/confirm-dialog'
import {
  useCachePolicy,
  useClearCachePolicyEntries,
  useSetCachePolicy,
} from '@/hooks/use-credentials'
import {
  CACHE_TTL_OPTIONS,
  formatTtl,
  type CacheTtlSeconds,
  validateCachePolicyDraft,
} from '@/lib/cache-policy'
import { extractErrorMessage } from '@/lib/utils'

interface CachePolicyDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CachePolicyDialog({ open, onOpenChange }: CachePolicyDialogProps) {
  const confirm = useConfirm()
  const { data, isLoading, error } = useCachePolicy()
  const { mutate: save, isPending: saving } = useSetCachePolicy()
  const { mutate: clearEntries, isPending: clearing } = useClearCachePolicyEntries()

  const [enabled, setEnabled] = useState(true)
  const [defaultTtlSecs, setDefaultTtlSecs] = useState<CacheTtlSeconds>(1800)
  const [autoWithoutCacheControl, setAutoWithoutCacheControl] = useState(true)
  const [rollingPrefixEnabled, setRollingPrefixEnabled] = useState(true)
  const [rollingPrefixLimit, setRollingPrefixLimit] = useState(8)
  const [capacity, setCapacity] = useState(65_536)
  const [flushIntervalSecs, setFlushIntervalSecs] = useState(60)
  const [minPct, setMinPct] = useState(0)
  const [maxPct, setMaxPct] = useState(0)
  const [validationError, setValidationError] = useState<string | null>(null)

  useEffect(() => {
    if (!data) return
    setEnabled(data.enabled)
    setDefaultTtlSecs(data.defaultTtlSecs)
    setAutoWithoutCacheControl(data.autoWithoutCacheControl)
    setRollingPrefixEnabled(data.rollingPrefixEnabled)
    setRollingPrefixLimit(data.rollingPrefixLimit)
    setCapacity(data.capacity)
    setFlushIntervalSecs(data.flushIntervalSecs)
    setMinPct(data.minPct)
    setMaxPct(data.maxPct)
    setValidationError(null)
  }, [data])

  const busy = saving || clearing

  const handleSave = (event: FormEvent) => {
    event.preventDefault()
    const message = validateCachePolicyDraft({
      capacity,
      flushIntervalSecs,
      rollingPrefixLimit,
      minPct,
      maxPct,
    })
    setValidationError(message)
    if (message) return

    save(
      {
        enabled,
        defaultTtlSecs,
        autoWithoutCacheControl,
        rollingPrefixEnabled,
        rollingPrefixLimit,
        capacity,
        flushIntervalSecs,
        minPct,
        maxPct,
      },
      {
        onSuccess: () => {
          toast.success('缓存策略已保存并立即生效')
          onOpenChange(false)
        },
        onError: (saveError) => {
          toast.error(`保存失败: ${extractErrorMessage(saveError)}`)
        },
      },
    )
  }

  const handleClear = async () => {
    const accepted = await confirm({
      title: '清空模拟缓存',
      description:
        '清空后，后续请求会重新产生 cache_creation；不会删除用量历史。确定继续？',
      confirmText: '清空缓存',
      destructive: true,
    })
    if (!accepted) return
    clearEntries(undefined, {
      onSuccess: ({ clearedEntries }) => {
        toast.success(`已清空 ${clearedEntries} 条缓存记录`)
      },
      onError: (clearError) => {
        toast.error(`清空失败: ${extractErrorMessage(clearError)}`)
      },
    })
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Database className="h-4 w-4" />
            缓存策略
          </DialogTitle>
          <DialogDescription>
            管理 rs 的模拟 prompt cache 计量。客户端显式 TTL 始终优先于这里的默认值。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <p className="py-10 text-center text-sm text-muted-foreground">加载缓存策略…</p>
        ) : error || !data ? (
          <div role="alert" className="rounded-xl border border-destructive/30 bg-destructive/5 p-4 text-sm text-destructive">
            加载失败：{extractErrorMessage(error)}
          </div>
        ) : (
          <form onSubmit={handleSave} className="space-y-5">
            <section className="space-y-3 rounded-2xl border border-border/70 p-4">
              <SettingSwitch
                id="cache-metering-enabled"
                label="模拟缓存计量"
                description="关闭后不再产生 cache_creation 或 cache_read 拆分，全部计入 input。"
                checked={enabled}
                onCheckedChange={setEnabled}
                disabled={busy}
              />
              <SettingSwitch
                id="cache-auto-without-control"
                label="无 cache_control 自动缓存"
                description="对稳定的多轮前缀自动模拟缓存；关闭后只处理客户端明确标记的请求。"
                checked={autoWithoutCacheControl}
                onCheckedChange={setAutoWithoutCacheControl}
                disabled={busy || !enabled}
              />
              <SettingSwitch
                id="cache-rolling-prefix-enabled"
                label="最近前缀滚动缓存"
                description="每次只登记最近的可复用前缀，避免超长对话占满缓存；关闭后恢复旧算法。"
                checked={rollingPrefixEnabled}
                onCheckedChange={setRollingPrefixEnabled}
                disabled={busy || !enabled}
              />
            </section>

            {!rollingPrefixEnabled && enabled && (
              <div
                role="alert"
                className="rounded-xl border border-amber-500/30 bg-amber-500/10 p-3 text-xs leading-relaxed text-amber-700 dark:text-amber-300"
              >
                已恢复旧的全历史前缀算法。超长对话可能一次写入数千条缓存记录，建议只用于临时对比或紧急回退。
              </div>
            )}

            <section className="grid gap-4 rounded-2xl border border-border/70 p-4 sm:grid-cols-2">
              <div className="space-y-2">
                <label htmlFor="cache-default-ttl" className="text-sm font-medium">
                  默认 TTL
                </label>
                <Select
                  value={String(defaultTtlSecs)}
                  onValueChange={(value) => setDefaultTtlSecs(Number(value) as CacheTtlSeconds)}
                  disabled={busy || !enabled}
                >
                  <SelectTrigger id="cache-default-ttl">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {CACHE_TTL_OPTIONS.map((seconds) => (
                      <SelectItem key={seconds} value={String(seconds)}>
                        {formatTtl(seconds)}{seconds === 1800 ? '（默认）' : ''}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
                <p className="text-xs text-muted-foreground">
                  支持 5 分钟、30 分钟和 1 小时；显式 TTL 不会被覆盖。
                </p>
              </div>

              <NumberField
                id="cache-capacity"
                label="最大缓存条目"
                description="范围 256–65536，推荐 65536；调小会立即按 LRU 淘汰。"
                min={256}
                max={65_536}
                value={capacity}
                onChange={setCapacity}
                disabled={busy || !enabled}
              />

              <NumberField
                id="cache-rolling-prefix-limit"
                label="每请求滚动前缀数"
                description="范围 2–64，推荐 8；仅在滚动缓存开启时生效。"
                min={2}
                max={64}
                value={rollingPrefixLimit}
                onChange={setRollingPrefixLimit}
                disabled={busy || !enabled || !rollingPrefixEnabled}
              />

              <NumberField
                id="cache-flush-interval"
                label="清理 / 落盘周期（秒）"
                description="范围 10–600，修改后无需重启。"
                min={10}
                max={600}
                value={flushIntervalSecs}
                onChange={setFlushIntervalSecs}
                disabled={busy || !enabled}
              />

              <div className="space-y-2">
                <span className="text-sm font-medium">命中率整形（%）</span>
                <div className="grid grid-cols-2 gap-2">
                  <NumberInput
                    id="cache-min-pct"
                    label="下界"
                    min={0}
                    max={100}
                    value={minPct}
                    onChange={setMinPct}
                    disabled={busy || !enabled}
                  />
                  <NumberInput
                    id="cache-max-pct"
                    label="上界"
                    min={0}
                    max={100}
                    value={maxPct}
                    onChange={setMaxPct}
                    disabled={busy || !enabled}
                  />
                </div>
                <p className="text-xs text-muted-foreground">
                  0 / 0 表示关闭；冷启动 cache_read=0 时不会被下界抬高。
                </p>
              </div>
            </section>

            <section className="space-y-3 rounded-2xl border border-border/70 bg-muted/25 p-4">
              <div className="flex items-center justify-between gap-4">
                <div className="flex items-center gap-2 text-sm font-medium">
                  <Gauge className="h-4 w-4" />
                  运行状态
                </div>
                <span className="text-xs text-muted-foreground">
                  {data.activeEntries} / {data.capacity} 条
                </span>
              </div>
              <Progress value={data.usagePct} aria-label={`缓存容量使用率 ${data.usagePct.toFixed(1)}%`} />
              <div className="grid gap-2 text-xs text-muted-foreground sm:grid-cols-3">
                <StatusItem label="容量使用率" value={`${data.usagePct.toFixed(1)}%`} />
                <StatusItem label="段命中率" value={formatSegmentHitRate(data.segmentLookups, data.segmentHits)} />
                <StatusItem label="累计淘汰" value={`${data.evictions} 条`} />
                <StatusItem label="过期清理" value={`${data.expiredEntriesRemoved} 条`} />
                <StatusItem label="落盘状态" value={data.persistEnabled ? (data.dirty ? '等待落盘' : '已同步') : '未启用'} />
                <StatusItem label="最后落盘" value={formatTimestamp(data.lastFlushAt)} />
              </div>
            </section>

            <div className="flex gap-2 rounded-xl border border-sky-500/20 bg-sky-500/5 p-3 text-xs leading-relaxed text-muted-foreground">
              <Info className="mt-0.5 h-4 w-4 shrink-0 text-sky-500" />
              <p>
                这里仅改变 rs 对 input / cache_creation / cache_read 的计量拆分，不会减少 Kiro
                上游实际 token、费用或延迟。缓存命中率整形保持三项 token 总量不变。
              </p>
            </div>

            {validationError && (
              <p role="alert" className="text-sm text-destructive">
                {validationError}
              </p>
            )}

            <DialogFooter className="gap-2 sm:justify-between">
              <Button
                type="button"
                variant="destructive"
                size="sm"
                onClick={handleClear}
                disabled={busy || data.activeEntries === 0}
              >
                <Trash2 className="mr-1.5 h-3.5 w-3.5" />
                {clearing ? '清空中…' : '清空缓存'}
              </Button>
              <Button type="submit" size="sm" disabled={busy}>
                <HardDrive className="mr-1.5 h-3.5 w-3.5" />
                {saving ? '保存中…' : '保存并立即生效'}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}

function SettingSwitch({
  id,
  label,
  description,
  checked,
  onCheckedChange,
  disabled,
}: {
  id: string
  label: string
  description: string
  checked: boolean
  onCheckedChange: (checked: boolean) => void
  disabled: boolean
}) {
  return (
    <div className="flex items-center justify-between gap-4">
      <div>
        <label htmlFor={id} className="text-sm font-medium">
          {label}
        </label>
        <p className="mt-0.5 text-xs text-muted-foreground">{description}</p>
      </div>
      <Switch id={id} checked={checked} onCheckedChange={onCheckedChange} disabled={disabled} />
    </div>
  )
}

function NumberField({
  id,
  label,
  description,
  min,
  max,
  value,
  onChange,
  disabled,
}: {
  id: string
  label: string
  description: string
  min: number
  max: number
  value: number
  onChange: (value: number) => void
  disabled: boolean
}) {
  return (
    <div className="space-y-2">
      <label htmlFor={id} className="text-sm font-medium">
        {label}
      </label>
      <Input
        id={id}
        type="number"
        min={min}
        max={max}
        step={1}
        value={value}
        onChange={(event) => onChange(Number(event.target.value))}
        disabled={disabled}
      />
      <p className="text-xs text-muted-foreground">{description}</p>
    </div>
  )
}

function NumberInput({
  id,
  label,
  min,
  max,
  value,
  onChange,
  disabled,
}: {
  id: string
  label: string
  min: number
  max: number
  value: number
  onChange: (value: number) => void
  disabled: boolean
}) {
  return (
    <label htmlFor={id} className="space-y-1 text-xs text-muted-foreground">
      <span>{label}</span>
      <Input
        id={id}
        type="number"
        min={min}
        max={max}
        step={1}
        value={value}
        onChange={(event) => onChange(Number(event.target.value))}
        disabled={disabled}
      />
    </label>
  )
}

function StatusItem({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-lg bg-background/70 px-2.5 py-2">
      <span>{label}</span>
      <strong className="mt-0.5 block font-medium text-foreground">{value}</strong>
    </div>
  )
}

function formatTimestamp(value?: string | null): string {
  if (!value) return '尚未落盘'
  const timestamp = new Date(value)
  return Number.isNaN(timestamp.getTime()) ? value : timestamp.toLocaleString()
}

function formatSegmentHitRate(lookups: number, hits: number): string {
  if (lookups <= 0) return '尚无数据'
  return `${((hits / lookups) * 100).toFixed(1)}%（${hits}/${lookups}）`
}
