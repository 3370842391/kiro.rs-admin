import { useEffect, useMemo, useRef, useState } from 'react'
import { Network, ArrowUp, ArrowDown, RotateCcw, ChevronDown } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import { Switch } from '@/components/ui/switch'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter,
} from '@/components/ui/dialog'
import {
  useEndpointChains, useSetEndpointChains, useEndpointMode, useSetEndpointMode,
} from '@/hooks/use-credentials'
import type { EndpointBucketOption } from '@/api/credentials'
import type { EnterpriseRetryEndpoint, EnterpriseRetrySettings, EnterpriseSelectionPolicy } from '@/types/api'
import { cn, extractErrorMessage } from '@/lib/utils'

interface EndpointChainsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/** 主端点中文标签 */
const PRIMARY_LABEL: Record<string, string> = {
  ide: 'Kiro IDE 协议',
  cli: 'CLI 协议',
}

/** 备用端点说明；不同入口不代表限流额度相互独立。 */
const BUCKET_HINT: Record<string, string> = {
  runtime:
    'runtime.kiro.dev — Kiro Runtime 服务入口；是否与其它入口共享限流以实际返回为准。',
  codewhisperer:
    'codewhisperer — 与 q 使用同一 host 的不同服务入口。',
  amazonq:
    'amazonq — 与 q 使用同一 host 的不同服务入口。',
  runtime_cli: 'runtime_cli — CLI 协议的 runtime 桶。',
  cli: 'cli — CLI 协议主端点桶。',
  ide: 'ide — Kiro IDE 主端点桶。',
}

const ENDPOINT_LABEL: Record<string, string> = {
  runtime: 'Kiro Runtime',
  ide: 'Legacy Kiro IDE',
  codewhisperer: 'Legacy CodeWhisperer',
  amazonq: 'Legacy Amazon Q',
}

const ENTERPRISE_ENDPOINTS: EnterpriseRetryEndpoint[] = ['ide', 'runtime', 'amazonq', 'codewhisperer']
const DEFAULT_ENTERPRISE_RETRY: EnterpriseRetrySettings = {
  endpoints: ENTERPRISE_ENDPOINTS,
  firstEventTimeoutMs: 10_000,
  totalTimeoutMs: 30_000,
}

/**
 * 429 降级桶链配置：主端点 429 时「换桶不换号」依次尝试的备用桶（有序、可勾选）。
 * 未配置时走各端点静态默认链。空选 = 该主端点不降级。
 */
export function EndpointChainsDialog({ open, onOpenChange }: EndpointChainsDialogProps) {
  const { data, isLoading } = useEndpointChains()
  const { mutate: save, isPending: saving } = useSetEndpointChains()
  const { data: modeData } = useEndpointMode()
  const { mutate: saveMode, isPending: savingMode } = useSetEndpointMode()

  // 本地编辑态：primary -> 有序桶名数组
  const [draft, setDraft] = useState<Record<string, string[]>>({})
  const [maxAttempts, setMaxAttempts] = useState<number>(6)
  const [idleTimeout, setIdleTimeout] = useState<number>(120)
  const [autoContinue, setAutoContinue] = useState(false)
  const [autoContinueMax, setAutoContinueMax] = useState(3)
  const [partialRecovery, setPartialRecovery] = useState(false)
  const [partialWindowMs, setPartialWindowMs] = useState(750)
  const [expanded, setExpanded] = useState<string | null>(null)
  const [defaultEndpoint, setDefaultEndpoint] = useState('ide')
  const [bucketMode, setBucketMode] = useState<'same-endpoint' | 'hop' | 'none'>('same-endpoint')
  const [sameEndpointAttempts, setSameEndpointAttempts] = useState(3)
  const [enterpriseSpecialHandling, setEnterpriseSpecialHandling] = useState(false)
  const [enterpriseSelectionPolicy, setEnterpriseSelectionPolicy] = useState<EnterpriseSelectionPolicy>('priority')
  const [enterpriseDefaultEndpoint, setEnterpriseDefaultEndpoint] = useState('ide')
  const [enterpriseMaxRetries, setEnterpriseMaxRetries] = useState(32)
  const [enterpriseRetry, setEnterpriseRetry] = useState<EnterpriseRetrySettings>(DEFAULT_ENTERPRISE_RETRY)

  const hydratedRef = useRef(false)

  // 仅在弹窗打开时灌入服务端值，避免 refetch 或窗口聚焦刷新把编辑中的桶链冲掉。
  useEffect(() => {
    if (!open) {
      hydratedRef.current = false
      setExpanded(null)
      return
    }
    if (!data || hydratedRef.current) return
    hydratedRef.current = true
    setDraft(JSON.parse(JSON.stringify(data.chains)))
    setMaxAttempts(data.maxBucketAttemptsPerRequest)
    setIdleTimeout(data.streamIdleTimeoutSecs)
    setAutoContinue(data.autoContinueEnabled)
    setAutoContinueMax(data.autoContinueMax)
    setPartialRecovery(data.partialStreamRecoveryEnabled)
    setPartialWindowMs(data.partialStreamRecoveryWindowMs)
    setDefaultEndpoint(data.defaultEndpoint || 'ide')
    setBucketMode(data.rateLimitBucketMode || 'same-endpoint')
    setSameEndpointAttempts(data.sameEndpointAttempts || 3)
    setEnterpriseSpecialHandling(data.enterpriseSpecialHandling ?? false)
    setEnterpriseSelectionPolicy(data.enterpriseSelectionPolicy ?? 'priority')
    setEnterpriseDefaultEndpoint(data.enterpriseDefaultEndpoint || 'ide')
    setEnterpriseMaxRetries(data.enterpriseMaxRetries || 32)
    setEnterpriseRetry(data.enterpriseRetry ?? DEFAULT_ENTERPRISE_RETRY)
  }, [open, data])

  const primaries = useMemo(
    () => Object.keys(data?.availableBuckets ?? {}).sort(),
    [data],
  )

  const toggleBucket = (primary: string, bucket: string) => {
    setDraft((prev) => {
      const cur = prev[primary] ?? []
      const next = cur.includes(bucket)
        ? cur.filter((b) => b !== bucket)
        : [...cur, bucket]
      return { ...prev, [primary]: next }
    })
  }

  const move = (primary: string, idx: number, dir: -1 | 1) => {
    setDraft((prev) => {
      const cur = [...(prev[primary] ?? [])]
      const j = idx + dir
      if (j < 0 || j >= cur.length) return prev
      ;[cur[idx], cur[j]] = [cur[j], cur[idx]]
      return { ...prev, [primary]: cur }
    })
  }

  const resetToDefaults = () => {
    if (!data) return
    setDraft(JSON.parse(JSON.stringify(data.defaults)))
    toast.info('已重置为静态默认链（未保存）')
  }

  const handleSave = () => {
    if (!Number.isInteger(enterpriseMaxRetries) || enterpriseMaxRetries < 1 || enterpriseMaxRetries > 256) {
      toast.error('企业最大发送次数必须为 1–256 的整数，包含首次发送')
      return
    }
    if (enterpriseRetry.endpoints.length === 0) {
      toast.error('企业速打至少启用 1 个端点')
      return
    }
    if (!Number.isInteger(enterpriseRetry.firstEventTimeoutMs)
      || enterpriseRetry.firstEventTimeoutMs < 10
      || enterpriseRetry.firstEventTimeoutMs > 120_000) {
      toast.error('企业首事件超时必须为 10–120000 毫秒的整数')
      return
    }
    if (!Number.isInteger(enterpriseRetry.totalTimeoutMs)
      || enterpriseRetry.totalTimeoutMs < 30
      || enterpriseRetry.totalTimeoutMs > 300_000
      || enterpriseRetry.totalTimeoutMs < enterpriseRetry.firstEventTimeoutMs) {
      toast.error('企业总等待必须为 30–300000 毫秒的整数，且不小于首事件超时')
      return
    }
    save(
      {
        chains: draft,
        maxBucketAttemptsPerRequest: maxAttempts,
        streamIdleTimeoutSecs: idleTimeout,
        autoContinueEnabled: autoContinue,
        autoContinueMax,
        partialStreamRecoveryEnabled: partialRecovery,
        partialStreamRecoveryWindowMs: partialWindowMs,
        defaultEndpoint,
        rateLimitBucketMode: bucketMode,
        sameEndpointAttempts,
        enterpriseSpecialHandling,
        enterpriseSelectionPolicy,
        enterpriseDefaultEndpoint,
        enterpriseMaxRetries,
        enterpriseRetry,
      },
      {
        onSuccess: () => {
          toast.success('降级桶链已保存')
          onOpenChange(false)
        },
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[min(88dvh,800px)] flex-col gap-3 overflow-hidden p-4 sm:max-w-2xl sm:p-6">
        <DialogHeader className="shrink-0 space-y-1">
          <DialogTitle className="flex items-center gap-2">
            <Network className="h-4 w-4" />
            429 降级桶链
          </DialogTitle>
          <DialogDescription>
            主端点被 429 限流时，用<b>同一张凭据</b>依次尝试下列备用桶（换桶不换号），
            命中第一个 2xx 即返回。勾选启用、上下箭头排序。不勾选任何桶 = 该主端点不降级。
            未配置时走内置默认链。
          </DialogDescription>
        </DialogHeader>

        <div className="min-h-0 flex-1 space-y-3 overflow-y-auto overscroll-contain pr-1">
        <div className="rounded-lg border bg-muted/30 p-3">
          <div className="mb-2 text-sm font-medium">全局端点运行模式</div>
          <div className="flex flex-wrap gap-2">
            <Button
              type="button"
              size="sm"
              variant={modeData?.mode === 'best' ? 'default' : 'outline'}
              disabled={savingMode || !modeData}
              onClick={() => saveMode('best')}
            >
              默认最好模式
            </Button>
            <Button
              type="button"
              size="sm"
              variant={modeData?.mode === 'manual' ? 'default' : 'outline'}
              disabled={savingMode || !modeData}
              onClick={() => saveMode('manual')}
            >
              手动端点模式
            </Button>
          </div>
          {modeData && (
            <p className="mt-2 text-xs text-muted-foreground">
              当前：{modeData.label}；主端点 {ENDPOINT_LABEL[modeData.primaryEndpoint] ?? modeData.primaryEndpoint}
              {modeData.fallbackEndpoints.length > 0
                ? `，故障降级：${modeData.fallbackEndpoints.map((name) => ENDPOINT_LABEL[name] ?? name).join(' → ')}`
                : ''}
              {modeData.adaptiveScheduling ? '；已启用会话粘性和实时调度。' : ''}
            </p>
          )}
        </div>

        <div className="space-y-3 rounded-lg border bg-muted/30 p-3">
          <div>
            <div className="mb-2 text-sm font-medium">默认协议</div>
            <div className="flex flex-wrap gap-2">
              {(['ide', 'runtime'] as const).map((name) => (
                <Button
                  key={name}
                  type="button"
                  size="sm"
                  variant={defaultEndpoint === name ? 'default' : 'outline'}
                  onClick={() => setDefaultEndpoint(name)}
                >
                  {name}
                </Button>
              ))}
            </div>
            <p className="mt-2 text-xs text-muted-foreground">
              没钉端点的号走这个。单号仍可单独指定 ide / runtime。
            </p>
          </div>
          <div>
            <div className="mb-2 text-sm font-medium">429 策略</div>
            <div className="flex flex-wrap gap-2">
              {(
                [
                  { value: 'same-endpoint', label: '同端点3次后换号' },
                  { value: 'hop', label: '换桶救援' },
                  { value: 'none', label: '不重试' },
                ] as const
              ).map((item) => (
                <Button
                  key={item.value}
                  type="button"
                  size="sm"
                  variant={bucketMode === item.value ? 'default' : 'outline'}
                  onClick={() => setBucketMode(item.value)}
                >
                  {item.label}
                </Button>
              ))}
            </div>
            <p className="mt-2 text-xs text-muted-foreground">
              {bucketMode === 'same-endpoint'
                ? `同一张号、同一协议最多试 ${sameEndpointAttempts} 次，再换号，不切桶。`
                : bucketMode === 'hop'
                  ? '沿下面的桶链换桶不换号（旧行为）。'
                  : '普通 429 后立刻换号或失败，同号不再打。'}
            </p>
          </div>
          <div>
            <div className="mb-2 flex items-center justify-between gap-3">
              <div>
                <div className="text-sm font-medium">企业号专项处理</div>
                <p className="mt-1 text-xs text-muted-foreground">
                  只控制企业账号的端点轮询、首事件等待、429 退避和共享发送节奏；同一会话仍保留粘滞以命中缓存。
                </p>
              </div>
              <Switch
                checked={enterpriseSpecialHandling}
                onCheckedChange={setEnterpriseSpecialHandling}
                aria-label="企业号专项处理"
              />
            </div>
            {enterpriseSpecialHandling && (
              <div className="mt-2 rounded-md border bg-background/60 p-2.5">
                <div className="mb-2 text-xs font-medium text-foreground">企业账号选择策略</div>
                <div className="flex flex-wrap gap-2">
                  <Button
                    type="button"
                    size="sm"
                    variant={enterpriseSelectionPolicy === 'priority' ? 'default' : 'outline'}
                    onClick={() => setEnterpriseSelectionPolicy('priority')}
                  >
                    遵循账号优先级
                  </Button>
                  <Button
                    type="button"
                    size="sm"
                    variant={enterpriseSelectionPolicy === 'enterprise-first' ? 'default' : 'outline'}
                    onClick={() => setEnterpriseSelectionPolicy('enterprise-first')}
                  >
                    企业账号优先
                  </Button>
                </div>
                <p className="mt-2 mb-3 text-xs text-muted-foreground">
                  推荐“遵循账号优先级”：优先级数字越小越先用；高优先级账号满并发、满 RPM 或冷却时，才使用低优先级企业账号作为替补。
                  同一会话的可用粘滞账号仍优先命中缓存。
                </p>
                <div className="mb-2 text-xs font-medium text-foreground">启用企业端点（至少 1 个）</div>
                <div className="flex flex-wrap gap-x-4 gap-y-2">
                  {ENTERPRISE_ENDPOINTS.map((name, index) => (
                    <label key={name} className="flex items-center gap-2 text-xs">
                      <Checkbox
                        checked={enterpriseRetry.endpoints.includes(name)}
                        onCheckedChange={(checked) => setEnterpriseRetry((previous) => ({
                          ...previous,
                          endpoints: checked === true
                            ? [...previous.endpoints.filter((endpoint) => endpoint !== name), name]
                            : previous.endpoints.filter((endpoint) => endpoint !== name),
                        }))}
                      />
                      <span>企业{index + 1}（{name}）</span>
                    </label>
                  ))}
                </div>
                <p className="mt-2 mb-3 text-xs text-muted-foreground">
                  启用顺序：{enterpriseRetry.endpoints.join(' → ') || '尚未选择'}。不同端点不代表拥有独立限流额度。
                </p>
                <div className="mb-2 text-xs font-medium text-foreground">企业号默认端点</div>
                <div className="flex flex-wrap gap-2">
                  {ENTERPRISE_ENDPOINTS.map((name) => (
                    <Button
                      key={name}
                      type="button"
                      size="sm"
                      variant={enterpriseDefaultEndpoint === name ? 'default' : 'outline'}
                      onClick={() => setEnterpriseDefaultEndpoint(name)}
                    >
                      {name === 'ide' ? 'q 端点 (ide)' : name}
                    </Button>
                  ))}
                </div>
                <p className="mt-2 text-xs text-muted-foreground">
                  默认端点仅决定启用列表的轮询起点；未启用该端点时从列表首项开始，不会额外发送。ide 即 q 区域域名（q.*.amazonaws.com）。
                </p>
                <label className="mt-3 flex flex-wrap items-center gap-2 text-sm">
                  <span className="shrink-0 text-xs font-medium text-foreground">企业最大发送次数</span>
                  <Input
                    type="number"
                    min={1}
                    max={256}
                    value={enterpriseMaxRetries}
                    onChange={(e) =>
                      setEnterpriseMaxRetries(
                        Math.min(256, Math.max(1, Number(e.target.value) || 1)),
                      )
                    }
                    className="h-8 w-20"
                  />
                  <span className="text-xs text-muted-foreground">含首次，1–256，默认 32</span>
                </label>
                <p className="mt-2 text-xs text-muted-foreground">
                  同时受「单请求备用尝试上限 + 1」（0 表示该项不限）和总等待限制。
                  当前最多 {maxAttempts === 0 ? enterpriseMaxRetries : Math.min(enterpriseMaxRetries, maxAttempts + 1)} 次真实企业发送，可能因超时或账号额度提前结束。
                </p>
                <div className="mt-3 grid gap-3 sm:grid-cols-2">
                  <label className="flex items-center gap-2 text-xs">
                    <span className="shrink-0 font-medium">首事件超时（ms）</span>
                    <Input
                      type="number"
                      min={10}
                      max={120_000}
                      step={1}
                      value={enterpriseRetry.firstEventTimeoutMs}
                      onChange={(event) => {
                        const value = Number(event.target.value)
                        setEnterpriseRetry((previous) => ({ ...previous, firstEventTimeoutMs: value }))
                      }}
                      className="h-8 w-24"
                    />
                  </label>
                  <label className="flex items-center gap-2 text-xs">
                    <span className="shrink-0 font-medium">总等待（ms）</span>
                    <Input
                      type="number"
                      min={30}
                      max={300_000}
                      step={1}
                      value={enterpriseRetry.totalTimeoutMs}
                      onChange={(event) => {
                        const value = Number(event.target.value)
                        setEnterpriseRetry((previous) => ({ ...previous, totalTimeoutMs: value }))
                      }}
                      className="h-8 w-24"
                    />
                  </label>
                </div>
                <p className="mt-2 text-xs text-muted-foreground">
                  默认首事件 10000 ms、总等待 30000 ms；总等待不得小于首事件超时。
                </p>
              </div>
            )}
          </div>
        </div>

        {isLoading ? (
          <div className="py-8 text-center text-sm text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-2">
            {primaries.map((primary) => {
              const options: EndpointBucketOption[] = data?.availableBuckets[primary] ?? []
              const selected = draft[primary] ?? []
              const unselected = options
                .filter((o) => !selected.includes(o.name))
                .map((o) => o.name)
              const isOpen = expanded === primary
              return (
                <div key={primary} className="rounded-lg border">
                  <button
                    type="button"
                    className="flex w-full items-center gap-2 px-3 py-2 text-left"
                    onClick={() => setExpanded(isOpen ? null : primary)}
                    aria-expanded={isOpen}
                  >
                    <ChevronDown
                      className={cn(
                        'h-3.5 w-3.5 shrink-0 text-muted-foreground transition-transform',
                        isOpen ? 'rotate-0' : '-rotate-90',
                      )}
                    />
                    <div className="min-w-0 flex-1">
                      <div className="text-sm font-medium">
                        {PRIMARY_LABEL[primary] ?? primary}
                        <span className="ml-2 font-mono text-xs text-muted-foreground">{primary}</span>
                      </div>
                      <p className="truncate font-mono text-[11px] text-muted-foreground">
                        {selected.length > 0 ? selected.join(' → ') : '不降级'}
                      </p>
                    </div>
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {isOpen ? '收起' : '展开'}
                    </span>
                  </button>

                  {isOpen && (
                    <div className="space-y-1 border-t px-3 py-2">
                      {selected.map((bucket, idx) => (
                        <div
                          key={bucket}
                          className="flex items-center gap-2 rounded-md bg-muted/50 px-2 py-1"
                          title={BUCKET_HINT[bucket]}
                        >
                          <span className="w-5 text-center text-xs text-muted-foreground">
                            {idx + 1}
                          </span>
                          <Checkbox
                            checked
                            onCheckedChange={() => toggleBucket(primary, bucket)}
                          />
                          <span className="flex-1 font-mono text-[13px]">{bucket}</span>
                          <Button
                            type="button"
                            size="icon"
                            variant="ghost"
                            className="h-6 w-6"
                            disabled={idx === 0}
                            onClick={() => move(primary, idx, -1)}
                            title="上移"
                            aria-label={`将 ${bucket} 上移一位`}
                          >
                            <ArrowUp className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            type="button"
                            size="icon"
                            variant="ghost"
                            className="h-6 w-6"
                            disabled={idx === selected.length - 1}
                            onClick={() => move(primary, idx, 1)}
                            title="下移"
                            aria-label={`将 ${bucket} 下移一位`}
                          >
                            <ArrowDown className="h-3.5 w-3.5" />
                          </Button>
                        </div>
                      ))}
                      {unselected.map((bucket) => (
                        <div
                          key={bucket}
                          className="flex items-center gap-2 px-2 py-1"
                          title={BUCKET_HINT[bucket]}
                        >
                          <span className="w-5" />
                          <Checkbox
                            checked={false}
                            onCheckedChange={() => toggleBucket(primary, bucket)}
                          />
                          <span className="flex-1 font-mono text-[13px] text-muted-foreground">
                            {bucket}
                          </span>
                        </div>
                      ))}
                      {options.length === 0 && (
                        <p className="text-xs text-muted-foreground">该协议无可选备用桶。</p>
                      )}
                    </div>
                  )}
                </div>
              )
            })}
          </div>
        )}

        <div className="space-y-3 border-t pt-3">
          <div className="grid gap-2 sm:grid-cols-2">
            <label className="flex items-center gap-2 text-sm">
              <span className="shrink-0 text-muted-foreground">单请求备用尝试上限</span>
              <Input
                type="number"
                min={0}
                value={maxAttempts}
                onChange={(e) => setMaxAttempts(Math.max(0, Number(e.target.value) || 0))}
                className="h-8 w-20"
              />
              <span className="text-xs text-muted-foreground">0 = 不限</span>
            </label>
            <label className="flex items-center gap-2 text-sm">
              <span className="shrink-0 text-muted-foreground">流式空闲超时（秒）</span>
              <Input
                type="number"
                min={0}
                value={idleTimeout}
                onChange={(e) => setIdleTimeout(Math.max(0, Number(e.target.value) || 0))}
                className="h-8 w-20"
              />
              <span className="text-xs text-muted-foreground">0 = 关闭</span>
            </label>
          </div>

          <div className="space-y-2">
            <div>
              <div className="text-sm font-medium">流式自动恢复</div>
              <p className="mt-0.5 text-xs text-muted-foreground">
                默认关闭。开启后纯文本截断会自动请求续写，可能增加上游调用次数、总耗时和费用；
                不会续写工具调用、空流、复读熔断或显式错误。
              </p>
            </div>
            <div className="flex flex-col gap-2 text-sm sm:flex-row sm:flex-wrap sm:items-center sm:gap-3">
              <label className="flex min-h-8 items-center gap-2">
                <Checkbox
                  checked={autoContinue}
                  onCheckedChange={(checked) => setAutoContinue(checked === true)}
                />
                <span>启用纯文本自动续写</span>
              </label>
              <label className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">最大轮数</span>
                <Input
                  type="number"
                  min={0}
                  max={10}
                  value={autoContinueMax}
                  onChange={(event) => setAutoContinueMax(
                    Math.min(10, Math.max(0, Number(event.target.value) || 0)),
                  )}
                  className="h-8 w-20"
                  disabled={!autoContinue}
                />
                <span className="text-xs text-muted-foreground">0–10</span>
              </label>
            </div>
            <div className="flex flex-col gap-2 text-sm sm:flex-row sm:flex-wrap sm:items-center sm:gap-3">
              <label className="flex min-h-8 items-center gap-2">
                <Checkbox
                  checked={partialRecovery}
                  onCheckedChange={(checked) => setPartialRecovery(checked === true)}
                  disabled={!autoContinue}
                />
                <span>恢复可疑半截流</span>
              </label>
              <label className="flex items-center gap-2">
                <span className="text-xs text-muted-foreground">判定窗口</span>
                <Input
                  type="number"
                  min={100}
                  max={10000}
                  step={50}
                  value={partialWindowMs}
                  onChange={(event) => setPartialWindowMs(
                    Math.min(10000, Math.max(100, Number(event.target.value) || 750)),
                  )}
                  className="h-8 w-24"
                  disabled={!autoContinue || !partialRecovery}
                />
                <span className="text-xs text-muted-foreground">毫秒</span>
              </label>
              <span className="text-xs text-muted-foreground">
                可能误判正常短答，建议从 750 开始灰度
              </span>
            </div>
          </div>
        </div>
        </div>

        <DialogFooter className="shrink-0 gap-2 sm:justify-between">
          <Button type="button" variant="outline" size="sm" onClick={resetToDefaults} disabled={saving}>
            <RotateCcw className="mr-1 h-3.5 w-3.5" />
            恢复默认
          </Button>
          <Button type="button" size="sm" onClick={handleSave} disabled={saving || isLoading}>
            {saving ? '保存中…' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
