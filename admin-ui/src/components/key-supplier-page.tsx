import { useEffect, useMemo, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Bell, Boxes, CheckCheck, Clipboard, CloudCog, Loader2, PackagePlus, Plus, RefreshCw,
  RotateCcw, Send, ShieldCheck, Trash2, Webhook,
} from 'lucide-react'
import {
  createSupplier, deleteSupplier, getSupplierCallbackUrl, getSupplierEntryOverview,
  getSupplierPool, getSupplierPoolStatus, listSuppliers, listSupplierEvents,
  markSupplierEventsRead, purchaseFromSupplier, registerSupplierEntryWebhook, retrySupplierEvent,
  testSupplierEntryWebhook, updateSupplier, updateSupplierPool,
} from '@/api/key-supplier'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { GroupMultiSelect } from '@/components/group-select'
import { useGroupOptions } from '@/hooks/use-groups'
import { extractErrorMessage } from '@/lib/utils'
import {
  emptySupplierEntry, emptySupplierPool, getSupplierEventStatusLabel, getSupplierKindLabel,
  hasUnreadSupplierEvents, isValidSupplierId, parseSupplierNumberDraft, suggestSupplierId,
  toSupplierEntryUpdate, validateSupplierPool,
} from '@/lib/key-supplier'
import type {
  SupplierEntryUpdate, SupplierEvent, SupplierEventStatus, SupplierKind, SupplierPoolConfig,
} from '@/types/api'

const EVENT_PAGE_SIZE = 20
const SUPPLIER_KINDS: readonly SupplierKind[] = [
  'kiro-rs', 'kiro-app', 'kiroapp-io', 'kiro-drop', 'kiro-ceo',
]

type SupplierNumericField =
  | 'minPurchase'
  | 'maxPurchase'
  | 'rpmLimit'
  | 'priority'
  | 'restockUsableThreshold'
  | 'lowQuotaThreshold'
type NumericDrafts = Record<SupplierNumericField, string>

function toNumericDrafts(config: Pick<SupplierEntryUpdate, SupplierNumericField>): NumericDrafts {
  return {
    minPurchase: String(config.minPurchase),
    maxPurchase: String(config.maxPurchase),
    rpmLimit: String(config.rpmLimit),
    priority: String(config.priority),
    restockUsableThreshold: String(config.restockUsableThreshold),
    lowQuotaThreshold: String(config.lowQuotaThreshold),
  }
}

function formatTime(value: string): string {
  const time = new Date(value)
  return Number.isNaN(time.getTime()) ? value : time.toLocaleString()
}

function eventBadgeVariant(status: SupplierEventStatus): 'secondary' | 'success' | 'warning' | 'destructive' {
  if (status === 'succeeded') return 'success'
  if (status === 'failed') return 'destructive'
  if (status === 'processing') return 'warning'
  return 'secondary'
}

function eventDetail(event: SupplierEvent): string {
  const parts = [
    `接收 ${event.quantity}`,
    `导入 ${event.importedCount}`,
    `重复 ${event.duplicateCount}`,
    `失败 ${event.failedCount}`,
  ]
  return parts.join(' · ')
}

/**
 * Renders a key price. `kiroapp-io` prices are tiered by each mother account's
 * cumulative output, so we show the range rather than implying a single price —
 * the actual charge is only known from `totalDebit` after the order lands.
 */
function formatKeyPrice(min: number | null, max: number | null): string {
  if (min === null && max === null) return '—'
  if (min !== null && max !== null && min !== max) return `${min} ~ ${max}`
  return String(min ?? max)
}

function Metric({ label, value }: { label: string; value: number | string }) {
  return (
    <div className="border border-border/50 bg-secondary/20 p-3">
      <div className="text-xs text-muted-foreground">{label}</div>
      <div className="mt-1 font-mono text-lg font-semibold tabular-nums">{value}</div>
    </div>
  )
}

export function KeySupplierPage() {
  const queryClient = useQueryClient()
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const [config, setConfig] = useState<SupplierEntryUpdate | null>(null)
  const [apiKey, setApiKey] = useState('')
  const [webhookToken, setWebhookToken] = useState('')
  const [webhookSecret, setWebhookSecret] = useState('')
  const groupOptions = useGroupOptions()
  const [numericDrafts, setNumericDrafts] = useState<NumericDrafts>({
    minPurchase: '', maxPurchase: '', rpmLimit: '', priority: '',
    restockUsableThreshold: '', lowQuotaThreshold: '',
  })
  const [purchaseCountDraft, setPurchaseCountDraft] = useState('1')
  const [poolDraft, setPoolDraft] = useState<SupplierPoolConfig>(emptySupplierPool)
  const [poolTargetDraft, setPoolTargetDraft] = useState('0')
  const [poolLowQuotaDraft, setPoolLowQuotaDraft] = useState('0')
  const [selectedIds, setSelectedIds] = useState<number[]>([])
  const [before, setBefore] = useState<number | undefined>()
  const [previousCursors, setPreviousCursors] = useState<Array<number | undefined>>([])
  const [scopeToSupplier, setScopeToSupplier] = useState(true)
  const previousEvents = useRef<readonly SupplierEvent[] | null>(null)
  const seenEventIds = useRef(new Set<string>())

  // 供货商列表就是这个页面的「配置」来源，沿用 configQuery 这个名字。
  const configQuery = useQuery({ queryKey: ['supplier-config'], queryFn: listSuppliers })
  const suppliers = configQuery.data?.items ?? []
  const selectedEntry = suppliers.find((entry) => entry.id === selectedId) ?? null
  const eventSupplierId = scopeToSupplier && selectedId ? selectedId : undefined

  const overviewQuery = useQuery({
    queryKey: ['supplier-overview', selectedId],
    queryFn: () => getSupplierEntryOverview(selectedId as string),
    enabled: selectedId !== null && !creating,
    refetchInterval: 30000,
  })
  const eventsQuery = useQuery({
    queryKey: ['supplier-events', before, eventSupplierId],
    queryFn: () => listSupplierEvents({ limit: EVENT_PAGE_SIZE, before, supplierId: eventSupplierId }),
    refetchInterval: 5000,
  })

  const poolQuery = useQuery({ queryKey: ['supplier-pool'], queryFn: getSupplierPool })
  const poolStatusQuery = useQuery({
    queryKey: ['supplier-pool-status'],
    queryFn: getSupplierPoolStatus,
    refetchInterval: 30000,
  })

  // 数量输入保持字符串草稿：直接绑 number 会让用户清空输入框时跳成 0。
  const poolDraftFromServer = poolQuery.data ?? null
  const poolTarget = parseSupplierNumberDraft(poolTargetDraft, 0)
  const poolLowQuota = parseSupplierNumberDraft(poolLowQuotaDraft, 0)
  const poolDraftValues: SupplierPoolConfig = {
    enabled: poolDraft.enabled,
    targetCount: poolTarget ?? -1,
    lowQuotaThreshold: poolLowQuota ?? -1,
  }
  const poolValidationError =
    poolTarget === null || poolLowQuota === null
      ? '目标存量与额度水位必须是非负整数'
      : validateSupplierPool(poolDraftValues)

  useEffect(() => {
    if (poolDraftFromServer === null) return
    setPoolDraft(poolDraftFromServer)
    setPoolTargetDraft(String(poolDraftFromServer.targetCount))
    setPoolLowQuotaDraft(String(poolDraftFromServer.lowQuotaThreshold))
  }, [poolDraftFromServer])

  const savePool = useMutation({
    mutationFn: () => updateSupplierPool(poolDraftValues),
    onSuccess: (saved) => {
      toast.success(saved.enabled ? `全局号池已启用，目标存量 ${saved.targetCount}` : '全局号池已关闭')
      queryClient.invalidateQueries({ queryKey: ['supplier-pool'] })
      queryClient.invalidateQueries({ queryKey: ['supplier-pool-status'] })
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })

  // 首次拿到列表时选中第一家。
  useEffect(() => {
    if (selectedId !== null || suppliers.length === 0) return
    setSelectedId(suppliers[0].id)
  }, [selectedId, suppliers])

  // 切换供货商（或列表刷新）时把表单重置成该家的当前值。
  useEffect(() => {
    if (creating || !selectedEntry) return
    const next = toSupplierEntryUpdate(selectedEntry)
    setConfig(next)
    setNumericDrafts(toNumericDrafts(next))
    setPurchaseCountDraft(String(next.minPurchase))
    setApiKey('')
    setWebhookToken('')
    setWebhookSecret('')
  }, [creating, selectedEntry])

  useEffect(() => {
    const current = eventsQuery.data?.items
    if (!current) return

    if (before !== undefined) {
      current.forEach((event) => seenEventIds.current.add(event.eventId))
      return
    }

    if (previousEvents.current === null) {
      current.forEach((event) => seenEventIds.current.add(event.eventId))
      previousEvents.current = current
      return
    }

    const hasNewUnread = hasUnreadSupplierEvents(previousEvents.current, current)
    current.forEach((event) => {
      if (hasNewUnread && event.readAt === null && !seenEventIds.current.has(event.eventId)) {
        toast.info('收到新的供应商事件', { description: `${event.supplierId} · ${event.eventType} · ${event.eventId}` })
      }
      seenEventIds.current.add(event.eventId)
    })
    previousEvents.current = current
  }, [before, eventsQuery.data])

  const invalidateSupplier = () => queryClient.invalidateQueries({ queryKey: ['supplier-events'] })
  const invalidateList = () => queryClient.invalidateQueries({ queryKey: ['supplier-config'] })

  const saveConfig = useMutation({
    mutationFn: (update: SupplierEntryUpdate) =>
      creating ? createSupplier(update) : updateSupplier(selectedId as string, update),
    onSuccess: (saved) => {
      invalidateList()
      setCreating(false)
      setSelectedId(saved.id)
      setApiKey('')
      setWebhookToken('')
      setWebhookSecret('')
      toast.success('供应商配置已保存')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const removeSupplier = useMutation({
    mutationFn: deleteSupplier,
    onSuccess: () => {
      invalidateList()
      setSelectedId(null)
      setConfig(null)
      toast.success('供货商已删除，历史事件保留')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const purchase = useMutation({
    mutationFn: (count: number) => purchaseFromSupplier(selectedId as string, count),
    onSuccess: (result) => {
      toast.success('手动购买已完成', {
        description: `请求 ${result.requested}，购买 ${result.purchased}，导入 ${result.imported}，失败 ${result.failed}`,
      })
      queryClient.invalidateQueries({ queryKey: ['supplier-overview'] })
      invalidateSupplier()
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const registerWebhook = useMutation({
    mutationFn: () => registerSupplierEntryWebhook(selectedId as string),
    onSuccess: () => {
      queryClient.invalidateQueries({ queryKey: ['supplier-overview'] })
      toast.success('Webhook 已注册')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const testWebhook = useMutation({
    mutationFn: () => testSupplierEntryWebhook(selectedId as string),
    onSuccess: (result) => result.success ? toast.success('Webhook 测试成功') : toast.error('Webhook 测试未通过'),
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const callbackUrlQuery = useMutation({
    mutationFn: () => getSupplierCallbackUrl(selectedId as string),
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const markRead = useMutation({
    mutationFn: markSupplierEventsRead,
    onSuccess: (result) => {
      setSelectedIds([])
      toast.success(`已标记 ${result.updated} 条事件为已读`)
      invalidateSupplier()
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const retryEvent = useMutation({
    mutationFn: retrySupplierEvent,
    onSuccess: () => {
      toast.success('事件已进入重试队列')
      invalidateSupplier()
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })

  const updateField = <K extends keyof SupplierEntryUpdate>(field: K, value: SupplierEntryUpdate[K]) => {
    setConfig((current) => current ? { ...current, [field]: value } : current)
  }
  const updateNumericDraft = (field: SupplierNumericField, value: string) => {
    setNumericDrafts((current) => ({ ...current, [field]: value }))
  }
  const startCreating = () => {
    const draft = emptySupplierEntry('kiro-app')
    setCreating(true)
    setConfig(draft)
    setNumericDrafts(toNumericDrafts(draft))
    setApiKey('')
    setWebhookToken('')
    setWebhookSecret('')
  }
  const cancelCreating = () => {
    setCreating(false)
    setSelectedId((current) => current ?? suppliers[0]?.id ?? null)
    if (suppliers.length === 0) setConfig(null)
  }
  const takenIds = useMemo(() => suppliers.map((entry) => entry.id), [suppliers])
  const parsedMinPurchase = parseSupplierNumberDraft(numericDrafts.minPurchase, 1)
  const parsedMaxPurchase = parseSupplierNumberDraft(numericDrafts.maxPurchase, 1)
  const parsedRpmLimit = parseSupplierNumberDraft(numericDrafts.rpmLimit, 0)
  const parsedPriority = parseSupplierNumberDraft(numericDrafts.priority, 0)
  const parsedRestockUsableThreshold = parseSupplierNumberDraft(
    numericDrafts.restockUsableThreshold,
    0,
  )
  const parsedLowQuotaThreshold = parseSupplierNumberDraft(numericDrafts.lowQuotaThreshold, 0)
  const idValid = !creating || isValidSupplierId(config?.id ?? '')
  const configNumbersValid = parsedMinPurchase !== null && parsedMaxPurchase !== null &&
    parsedRpmLimit !== null && parsedPriority !== null && parsedMinPurchase <= parsedMaxPurchase &&
    parsedRestockUsableThreshold !== null && parsedLowQuotaThreshold !== null &&
    idValid
  const parsedPurchaseCount = parseSupplierNumberDraft(purchaseCountDraft, 1)
  const purchaseCountValid = parsedPurchaseCount !== null && config !== null &&
    parsedPurchaseCount >= config.minPurchase && parsedPurchaseCount <= config.maxPurchase
  const supportsWebhookRegistration = config?.kind === 'kiro-rs'
  const handleSave = () => {
    if (!config) return
    if (
      parsedMinPurchase === null || parsedMaxPurchase === null ||
      parsedRpmLimit === null || parsedPriority === null ||
      parsedRestockUsableThreshold === null || parsedLowQuotaThreshold === null
    ) {
      toast.error('请输入有效的非负整数配置')
      return
    }
    if (creating && !isValidSupplierId(config.id ?? '')) {
      toast.error('供货商 ID 只能包含字母、数字、- 和 _')
      return
    }
    saveConfig.mutate({
      ...config,
      minPurchase: parsedMinPurchase,
      maxPurchase: parsedMaxPurchase,
      rpmLimit: parsedRpmLimit,
      priority: parsedPriority,
      restockUsableThreshold: parsedRestockUsableThreshold,
      lowQuotaThreshold: parsedLowQuotaThreshold,
      apiKey: apiKey || undefined,
      webhookToken: webhookToken || undefined,
      webhookSecret: webhookSecret || undefined,
    })
  }
  const copyCallbackUrl = async () => {
    const callbackUrl = callbackUrlQuery.data?.callbackUrl ?? registerWebhook.data?.callbackUrl
    if (!callbackUrl) return
    try {
      await navigator.clipboard.writeText(callbackUrl)
      toast.success('回调地址已复制')
    } catch {
      toast.error('无法访问剪贴板，请手动复制')
    }
  }
  const toggleSelected = (id: number, checked: boolean) => {
    setSelectedIds((current) => checked ? [...new Set([...current, id])] : current.filter((value) => value !== id))
  }
  const rows = eventsQuery.data?.items ?? []
  const showNext = rows.length === EVENT_PAGE_SIZE
  const purchaseResultSummary = '购买结果只显示计数，不展示 Key。'
  const callbackUrl = callbackUrlQuery.data?.callbackUrl ?? registerWebhook.data?.callbackUrl

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-center gap-3">
        <div className="flex items-center gap-2">
          <CloudCog className="h-5 w-5 text-muted-foreground" />
          <h2 className="text-lg font-semibold">Key 供应</h2>
          {(eventsQuery.data?.unreadCount ?? 0) > 0 && <Badge variant="destructive">{eventsQuery.data?.unreadCount} 未读</Badge>}
        </div>
        <Button className="ml-auto" size="sm" variant="outline" onClick={() => {
          configQuery.refetch(); overviewQuery.refetch(); eventsQuery.refetch()
        }} disabled={configQuery.isFetching || overviewQuery.isFetching || eventsQuery.isFetching}>
          <RefreshCw className={`h-3.5 w-3.5 ${configQuery.isFetching || overviewQuery.isFetching || eventsQuery.isFetching ? 'animate-spin' : ''}`} />
          刷新
        </Button>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle>供货商</CardTitle>
          <CardDescription>可同时对接多家。每家一个独立回调地址与采购预设，互不影响。</CardDescription>
        </CardHeader>
        <CardContent className="flex flex-wrap items-center gap-2">
          {suppliers.map((entry) => (
            <Button
              key={entry.id}
              size="sm"
              variant={entry.id === selectedId && !creating ? 'default' : 'outline'}
              onClick={() => { setCreating(false); setSelectedId(entry.id); setBefore(undefined); setPreviousCursors([]) }}
            >
              <span>{entry.name || entry.id}</span>
              <Badge variant="secondary">{getSupplierKindLabel(entry.kind)}</Badge>
              {!entry.enabled && <Badge variant="warning">已停用</Badge>}
            </Button>
          ))}
          {suppliers.length === 0 && !configQuery.isLoading && (
            <span className="text-sm text-muted-foreground">还没有配置任何供货商。</span>
          )}
          <Button className="ml-auto" size="sm" variant="outline" onClick={startCreating} disabled={creating}>
            <Plus className="h-3.5 w-3.5" />
            添加供货商
          </Button>
        </CardContent>
      </Card>

      <Card>
        <CardHeader className="pb-3">
          <CardTitle className="flex items-center gap-2"><Boxes className="h-4 w-4" />全局号池</CardTitle>
          <CardDescription>
            所有自动采购来的可用号合计不超过目标存量。任一供货商推来到货通知时，按「目标存量 − 当前可用数」算出缺口，
            只向推送方那一家下单补齐；缺口为 0 就不买。不设优先级，谁先推来谁先拿到缺口。
          </CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          <div className="flex flex-wrap items-center gap-4">
            <label className="flex items-center gap-2 text-sm">
              <Switch
                checked={poolDraft.enabled}
                onCheckedChange={(checked) => setPoolDraft({ ...poolDraft, enabled: checked })}
                aria-label="启用全局号池"
              />
              <span>启用全局号池</span>
            </label>
            <label className="flex items-center gap-2 text-sm">
              <span className="text-muted-foreground">目标存量</span>
              <Input
                className="w-24"
                value={poolTargetDraft}
                inputMode="numeric"
                aria-label="目标存量"
                onChange={(event) => setPoolTargetDraft(event.target.value)}
              />
            </label>
            <label className="flex items-center gap-2 text-sm">
              <span className="text-muted-foreground">额度水位</span>
              <Input
                className="w-24"
                value={poolLowQuotaDraft}
                inputMode="numeric"
                aria-label="额度水位"
                onChange={(event) => setPoolLowQuotaDraft(event.target.value)}
              />
            </label>
            <Button
              size="sm"
              className="ml-auto"
              onClick={() => savePool.mutate()}
              disabled={savePool.isPending || poolValidationError !== null}
            >
              {savePool.isPending && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
              保存
            </Button>
          </div>

          {poolValidationError !== null && (
            <div className="text-sm text-destructive" role="alert">{poolValidationError}</div>
          )}

          {poolStatusQuery.isError ? (
            <div className="text-sm text-destructive">{extractErrorMessage(poolStatusQuery.error)}</div>
          ) : poolStatusQuery.data ? (
            <>
              <div className="grid gap-px overflow-hidden border border-border/50 sm:grid-cols-2 lg:grid-cols-4">
                <Metric label="当前可用" value={poolStatusQuery.data.globalUsable} />
                <Metric label="还差" value={poolStatusQuery.data.deficit} />
                <Metric label="已判死" value={poolStatusQuery.data.health.dead} />
                <Metric
                  label="额度耗尽 / 低于水位"
                  value={`${poolStatusQuery.data.health.quotaExhausted} / ${poolStatusQuery.data.health.lowQuota}`}
                />
              </div>
              <div className="text-xs text-muted-foreground">
                识别方式：按 supplierId {poolStatusQuery.data.bySupplierId} 个 · 按备注 {poolStatusQuery.data.byLegacyChannel} 个
                {poolStatusQuery.data.matchedChannels.length > 0 && (
                  <> · 参与备注匹配的来源渠道：{poolStatusQuery.data.matchedChannels.join('、')}</>
                )}
              </div>
              {poolStatusQuery.data.byLegacyChannel > 0 && (
                <div className="border border-warning/40 bg-warning/[0.06] p-3 text-xs text-muted-foreground">
                  其中 {poolStatusQuery.data.byLegacyChannel} 个号是升级前买的，只能靠「来源渠道」备注认出来。
                  改动对应供货商的来源渠道会让它们不再计入水位，缺口随之变大、可能重复采购。
                </div>
              )}
              {poolStatusQuery.data.health.dead > 0 && (
                <div className="text-xs text-muted-foreground">
                  已判死的号仍留在池子里等保留期到点清理，但不计入可用数——系统会去补新号。
                </div>
              )}
            </>
          ) : null}
        </CardContent>
      </Card>

      {selectedId !== null && !creating && (
        <Card>
          <CardHeader className="pb-3">
            <CardTitle className="flex items-center gap-2"><ShieldCheck className="h-4 w-4" />供应概览</CardTitle>
            <CardDescription>安全额度与库存状态，每 30 秒刷新。</CardDescription>
          </CardHeader>
          <CardContent>
            {overviewQuery.isLoading ? <div className="py-4 text-sm text-muted-foreground">加载中...</div> : overviewQuery.isError ? <div className="py-4 text-sm text-destructive">{extractErrorMessage(overviewQuery.error)}</div> : overviewQuery.data ? (
              <div className="grid gap-px overflow-hidden border border-border/50 sm:grid-cols-2 lg:grid-cols-5">
                {overviewQuery.data.kind === 'kiro-rs' ? (
                  <>
                    <Metric label={`Profile · ${overviewQuery.data.profile.name}`} value={`${overviewQuery.data.profile.remaining} / ${overviewQuery.data.profile.quota}`} />
                    <Metric label="Stock Max" value={overviewQuery.data.stockMax} />
                    <Metric label="可用 Keys" value={overviewQuery.data.status.keysActive} />
                    <Metric label="库存 Keys" value={overviewQuery.data.status.keysStock} />
                    <Metric label="失效 / 生成状态" value={`${overviewQuery.data.status.keysDead} / ${overviewQuery.data.status.generating ? '生成中' : '空闲'}`} />
                  </>
                ) : (
                  <>
                    <Metric label="可用库存" value={overviewQuery.data.stockMax} />
                    <Metric
                      label={overviewQuery.data.keyPriceMax !== null ? '单价区间（阶梯）' : '单价'}
                      value={formatKeyPrice(overviewQuery.data.keyPrice, overviewQuery.data.keyPriceMax)}
                    />
                    <Metric label="剩余积分" value={overviewQuery.data.balance ?? '—'} />
                    <Metric label="本地号池（可用 / 共）" value={`${overviewQuery.data.credentialHealth.usable} / ${overviewQuery.data.credentialHealth.total}`} />
                    <Metric label="不可用构成（封 / 额度尽 / 额度低）" value={`${overviewQuery.data.credentialHealth.dead} / ${overviewQuery.data.credentialHealth.quotaExhausted} / ${overviewQuery.data.credentialHealth.lowQuota}`} />
                  </>
                )}
              </div>
            ) : null}
          </CardContent>
        </Card>
      )}

      <div className="grid gap-5 xl:grid-cols-2">
        <Card>
          <CardHeader className="pb-3">
            <CardTitle>{creating ? '新增供货商' : '连接配置'}</CardTitle>
            <CardDescription>留空的 secret 不会覆盖服务端现有值。</CardDescription>
          </CardHeader>
          <CardContent className="space-y-4">
            {configQuery.isLoading ? <div className="py-4 text-sm text-muted-foreground">加载配置中...</div> : configQuery.isError ? (
              <div className="flex flex-wrap items-center gap-3 py-4 text-sm text-destructive">
                <span>{extractErrorMessage(configQuery.error)}</span>
                <Button size="sm" variant="outline" onClick={() => configQuery.refetch()} disabled={configQuery.isFetching}>
                  <RefreshCw className={`h-3.5 w-3.5 ${configQuery.isFetching ? 'animate-spin' : ''}`} />
                  重试加载
                </Button>
              </div>
            ) : !config ? <div className="py-4 text-sm text-muted-foreground">暂未获取配置。</div> : (
              <>
                <div className="border border-border/50 bg-secondary/20 p-3 text-xs">
                  <div className="font-medium text-foreground">安全摘要</div>
                  <div className="mt-2 flex flex-wrap gap-2">
                    <Badge variant={selectedEntry?.apiKeyConfigured ? 'success' : 'secondary'}>API Key {selectedEntry?.apiKeyConfigured ? '已配置' : '未配置'}</Badge>
                    <Badge variant={selectedEntry?.webhookTokenConfigured ? 'success' : 'secondary'}>Webhook Token {selectedEntry?.webhookTokenConfigured ? '已配置' : '未配置'}</Badge>
                    <Badge variant={selectedEntry?.webhookSecretConfigured ? 'success' : 'warning'}>验签 {selectedEntry?.webhookSecretConfigured ? '已开启' : '未开启'}</Badge>
                  </div>
                  <p className="mt-2 text-muted-foreground">{purchaseResultSummary}</p>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <Field label="供货商名称"><Input value={config.name} onChange={(event) => {
                    const name = event.target.value
                    setConfig((current) => {
                      if (!current) return current
                      const shouldSuggestId = creating && (!current.id || current.id === suggestSupplierId(current.name, takenIds))
                      return { ...current, name, id: shouldSuggestId ? suggestSupplierId(name, takenIds) : current.id }
                    })
                  }} disabled={saveConfig.isPending} /></Field>
                  <Field label={creating ? '供货商 ID（创建后不可改）' : '供货商 ID'}>
                    <Input value={config.id ?? ''} onChange={(event) => updateField('id', event.target.value)} disabled={!creating || saveConfig.isPending} readOnly={!creating} />
                  </Field>
                  <Field label="协议类型">
                    <select
                      className="h-9 w-full border border-input bg-transparent px-3 text-sm"
                      aria-label="协议类型"
                      value={config.kind}
                      onChange={(event) => updateField('kind', event.target.value as SupplierKind)}
                      disabled={!creating || saveConfig.isPending}
                    >
                      {SUPPLIER_KINDS.map((kind) => <option key={kind} value={kind}>{getSupplierKindLabel(kind)}</option>)}
                    </select>
                  </Field>
                  <Field label="Supplier Base URL"><Input value={config.baseUrl} onChange={(event) => updateField('baseUrl', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Public Base URL"><Input value={config.publicBaseUrl} onChange={(event) => updateField('publicBaseUrl', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="API Key（只写入）"><Input type="password" autoComplete="new-password" placeholder={selectedEntry?.apiKeyConfigured ? '已配置；留空则保持不变' : '仅保存时写入'} value={apiKey} onChange={(event) => setApiKey(event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Webhook Token（只写入）"><Input type="password" autoComplete="new-password" placeholder={selectedEntry?.webhookTokenConfigured ? '已配置；留空则保持不变' : '留空自动生成'} value={webhookToken} onChange={(event) => setWebhookToken(event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Webhook 签名密钥（只写入）"><Input type="password" autoComplete="new-password" placeholder={selectedEntry?.webhookSecretConfigured ? '已配置；留空则保持不变' : '留空则不验签'} value={webhookSecret} onChange={(event) => setWebhookSecret(event.target.value)} disabled={saveConfig.isPending} /></Field>
                </div>
                <p className="text-xs text-muted-foreground">
                  签名密钥填对方保存 Webhook 时生成的那个（用于校验 <code>X-Kiro-Signature</code>）。
                  留空表示不验签——任何知道回调地址的人都能推假事件。
                </p>
                <div className="flex items-center justify-between gap-3 border-y border-border/50 py-3">
                  <div><label htmlFor="supplier-enabled" className="text-sm font-medium">启用</label><p className="text-xs text-muted-foreground">停用后仍会记录 Webhook，但不再采购。</p></div>
                  <Switch id="supplier-enabled" checked={config.enabled} onCheckedChange={(checked) => updateField('enabled', checked)} disabled={saveConfig.isPending} aria-label="启用" />
                </div>
                <div className="flex items-center justify-between gap-3 border-b border-border/50 pb-3">
                  <div><label htmlFor="auto-purchase" className="text-sm font-medium">自动购买</label><p className="text-xs text-muted-foreground">收到新 Key 就绪 Webhook 后自动发起一次购买。同一条推送重复到达不会重复购买。</p></div>
                  <Switch id="auto-purchase" checked={config.autoPurchase} onCheckedChange={(checked) => updateField('autoPurchase', checked)} disabled={saveConfig.isPending} aria-label="自动购买" />
                </div>
                <div className="flex items-center justify-between gap-3 border-b border-border/50 pb-3">
                  <div><label htmlFor="auto-delete-forbidden" className="text-sm font-medium">403 时自动删除</label><p className="text-xs text-muted-foreground">仅删除按此预设导入的自动采购账号。</p></div>
                  <Switch id="auto-delete-forbidden" checked={config.autoDeleteForbidden} onCheckedChange={(checked) => updateField('autoDeleteForbidden', checked)} disabled={saveConfig.isPending} aria-label="403 时自动删除" />
                </div>
                {poolDraft.enabled && (
                  <div className="border border-border/50 bg-secondary/20 p-3 text-xs text-muted-foreground">
                    全局号池已启用：本页的「仅在号不够用时补货」「补货水位」「额度水位」都不再参与判定，改由全局号池统一决定买不买、买几个。
                    「单次最大购买量」仍生效，但只作单笔安全上限——实际数量以全局缺口为准。
                  </div>
                )}
                <div className="flex items-center justify-between gap-3 border-b border-border/50 pb-3">
                  <div><label htmlFor="restock-only-when-exhausted" className="text-sm font-medium">仅在号不够用时补货</label><p className="text-xs text-muted-foreground">{poolDraft.enabled ? '全局号池启用中，此开关不参与判定。' : '开启后到货通知只表示「可以补货了」，不是「立刻买」。手动采购不受影响。'}</p></div>
                  <Switch id="restock-only-when-exhausted" checked={config.restockOnlyWhenExhausted} onCheckedChange={(checked) => updateField('restockOnlyWhenExhausted', checked)} disabled={saveConfig.isPending} aria-label="仅在号不够用时补货" />
                </div>
                <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                  <Field label="单次最小购买量"><Input type="number" min={1} value={numericDrafts.minPurchase} onChange={(event) => updateNumericDraft('minPurchase', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label={poolDraft.enabled ? '单次最大购买量（仅作安全上限）' : '单次最大购买量'}><Input type="number" min={1} value={numericDrafts.maxPurchase} onChange={(event) => updateNumericDraft('maxPurchase', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="API Region"><Input value={config.apiRegion} onChange={(event) => updateField('apiRegion', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="自动采购 RPM 预设"><Input type="number" min={0} value={numericDrafts.rpmLimit} onChange={(event) => updateNumericDraft('rpmLimit', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Priority"><Input type="number" min={0} value={numericDrafts.priority} onChange={(event) => updateNumericDraft('priority', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Source Channel"><Input value={config.sourceChannel} onChange={(event) => updateField('sourceChannel', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="补货水位（可用号数）"><Input type="number" min={0} value={numericDrafts.restockUsableThreshold} onChange={(event) => updateNumericDraft('restockUsableThreshold', event.target.value)} disabled={saveConfig.isPending || !config.restockOnlyWhenExhausted} /></Field>
                  <Field label="额度水位（剩余额度）"><Input type="number" min={0} value={numericDrafts.lowQuotaThreshold} onChange={(event) => updateNumericDraft('lowQuotaThreshold', event.target.value)} disabled={saveConfig.isPending || !config.restockOnlyWhenExhausted} /></Field>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <Field label="自动采购分组预设"><GroupMultiSelect value={config.groups} options={groupOptions} onChange={(groups) => updateField('groups', groups)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Nickname Prefix"><Input value={config.nicknamePrefix} onChange={(event) => updateField('nicknamePrefix', event.target.value)} disabled={saveConfig.isPending} /></Field>
                </div>
                {!idValid && <p className="text-xs text-destructive">供货商 ID 必填，且只能包含字母、数字、- 和 _。</p>}
                {!configNumbersValid && idValid && <p className="text-xs text-destructive">购买量需为正整数，且最小值不能大于最大值；RPM 和 Priority 需为非负整数。</p>}
                <div className="flex flex-wrap gap-2">
                  <Button onClick={handleSave} disabled={saveConfig.isPending || !configNumbersValid}>
                    {saveConfig.isPending && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                    保存配置
                  </Button>
                  {creating && <Button variant="outline" onClick={cancelCreating} disabled={saveConfig.isPending}>取消</Button>}
                  {!creating && selectedId !== null && (
                    <Button className="ml-auto" variant="outline" onClick={() => {
                      if (window.confirm(`删除供货商「${selectedEntry?.name || selectedId}」？历史事件会保留，但不再采购。`)) {
                        removeSupplier.mutate(selectedId)
                      }
                    }} disabled={removeSupplier.isPending}>
                      <Trash2 className="h-3.5 w-3.5" />
                      删除
                    </Button>
                  )}
                </div>
              </>
            )}
          </CardContent>
        </Card>

        <div className="space-y-5">
          <Card>
            <CardHeader className="pb-3"><CardTitle>手动购买</CardTitle><CardDescription>{purchaseResultSummary}{config ? ` 单次允许 ${config.minPurchase}-${config.maxPurchase} 个。` : ''}</CardDescription></CardHeader>
            <CardContent className="flex flex-wrap items-end gap-3">
              <Field label="购买数量" className="w-full sm:w-40"><Input type="number" min={config?.minPurchase ?? 1} max={config?.maxPurchase} value={purchaseCountDraft} onChange={(event) => setPurchaseCountDraft(event.target.value)} disabled={purchase.isPending} /></Field>
              {config && !purchaseCountValid && <p className="w-full text-xs text-destructive sm:w-auto">请输入 {config.minPurchase} 到 {config.maxPurchase} 之间的整数。</p>}
              <Button onClick={() => { if (purchaseCountValid && parsedPurchaseCount !== null) purchase.mutate(parsedPurchaseCount) }} disabled={purchase.isPending || !purchaseCountValid || creating || selectedId === null}><PackagePlus className="h-3.5 w-3.5" />购买</Button>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="pb-3">
              <CardTitle className="flex flex-wrap items-center gap-2"><Webhook className="h-4 w-4" />Webhook
                {supportsWebhookRegistration
                  ? <Badge variant={overviewQuery.data?.webhookRegistered ? 'success' : 'warning'}>{overviewQuery.data?.webhookRegistered ? 'Webhook 已注册' : 'Webhook 未注册'}</Badge>
                  : <Badge variant="secondary">需手动填写</Badge>}
              </CardTitle>
              <CardDescription>
                {supportsWebhookRegistration
                  ? '注册状态来自供应商账号；测试消息只验证连通性，不会购买。'
                  : config?.kind === 'kiroapp-io'
                    // kiroapp.io 的文档没有签名头，所以别让人去找一个不存在的密钥。
                    ? '该供货商没有注册接口。复制下面的回调地址，粘贴到对方面板的「设置 → Webhook 配置」，可先发一条 test 事件验证连通。'
                    : '该供货商没有注册接口。复制下面的回调地址，粘贴到对方面板的「到货通知（Webhook）」里，再把它生成的签名密钥填到左边。'}
              </CardDescription>
            </CardHeader>
            <CardContent className="space-y-3">
              {callbackUrl && <div className="flex min-w-0 gap-2"><Input aria-label="Webhook callback URL" value={callbackUrl} readOnly /><Button size="icon" variant="outline" title="复制回调地址" aria-label="复制回调地址到剪贴板" onClick={copyCallbackUrl}><Clipboard className="h-3.5 w-3.5" /></Button></div>}
              <div className="flex flex-wrap gap-2">
                {supportsWebhookRegistration ? (
                  <>
                    <Button variant="outline" onClick={() => registerWebhook.mutate()} disabled={registerWebhook.isPending || selectedId === null}><Webhook className="h-3.5 w-3.5" />{overviewQuery.data?.webhookRegistered ? '重新注册 Webhook' : '注册 Webhook'}</Button>
                    <Button variant="outline" onClick={() => testWebhook.mutate()} disabled={testWebhook.isPending || selectedId === null}><Send className="h-3.5 w-3.5" />测试 Webhook</Button>
                  </>
                ) : (
                  <Button variant="outline" onClick={() => callbackUrlQuery.mutate()} disabled={callbackUrlQuery.isPending || selectedId === null}><Clipboard className="h-3.5 w-3.5" />获取回调地址</Button>
                )}
              </div>
            </CardContent>
          </Card>
        </div>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <div className="flex flex-wrap items-center gap-2"><CardTitle className="flex items-center gap-2"><Bell className="h-4 w-4" />事件历史</CardTitle><Badge variant="secondary">{eventsQuery.data?.unreadCount ?? 0} 未读</Badge><div className="ml-auto flex flex-wrap gap-2"><Button size="sm" variant={scopeToSupplier ? 'default' : 'outline'} onClick={() => { setScopeToSupplier((current) => !current); setBefore(undefined); setPreviousCursors([]) }} disabled={selectedId === null}>{scopeToSupplier ? '只看当前供货商' : '全部供货商'}</Button><Button size="sm" variant="outline" onClick={() => markRead.mutate({ ids: selectedIds })} disabled={selectedIds.length === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />标记所选已读</Button><Button size="sm" variant="outline" onClick={() => markRead.mutate({ markAll: true, supplierId: eventSupplierId })} disabled={(eventsQuery.data?.unreadCount ?? 0) === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />全部标记已读</Button></div></div>
          <CardDescription>每 5 秒刷新；仅展示事件元数据与处理计数。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-2">
          {eventsQuery.isLoading ? <div className="py-5 text-sm text-muted-foreground">加载事件中...</div> : eventsQuery.isError ? <div className="py-5 text-sm text-destructive">{extractErrorMessage(eventsQuery.error)}</div> : rows.length === 0 ? <div className="py-5 text-sm text-muted-foreground">暂无事件。</div> : rows.map((event) => {
            // 等自动重试的事件也给重试按钮：人明确要求现在就试，不该还让他等退避走完。
            const retryable = event.status === 'failed' || event.status === 'skipped' || event.retryAfter !== null
            return <div key={event.id} className={`grid gap-2 border border-border/50 p-3 sm:grid-cols-[auto_minmax(0,1fr)_auto] sm:items-center ${event.readAt === null ? 'border-primary/40 bg-primary/[0.03]' : ''}`}>
              <Checkbox checked={selectedIds.includes(event.id)} onCheckedChange={(checked) => toggleSelected(event.id, checked === true)} aria-label={`选择事件 ${event.id}`} />
              <div className="min-w-0 space-y-1"><div className="flex flex-wrap items-center gap-2"><Badge variant="outline">{event.supplierId}</Badge><span className="break-all font-mono text-xs text-muted-foreground">{event.eventId}</span><Badge variant={eventBadgeVariant(event.status)}>{getSupplierEventStatusLabel(event.status)}</Badge>{event.retryAfter !== null && <Badge variant="warning">{formatTime(event.retryAfter)} 自动重试</Badge>}<span className="min-w-0 break-all text-sm font-medium">{event.eventType}</span><Badge variant="outline">{event.readAt === null ? '未读' : '已读'}</Badge></div><div className="break-words text-xs text-muted-foreground">{eventDetail(event)} · 尝试 {event.attempts} · 重复推送 {event.webhookDuplicateCount} · {formatTime(event.receivedAt)}</div>{event.purchaseOrderId && <div className="break-all text-xs text-muted-foreground">订单 ID：{event.purchaseOrderId}</div>}{event.message && <div className="break-words text-xs text-muted-foreground">{event.message}</div>}{event.lastError && <div className="break-words text-xs text-destructive">{event.lastError}</div>}</div>
              {retryable && <Button size="sm" variant="outline" onClick={() => retryEvent.mutate(event.id)} disabled={retryEvent.isPending}><RotateCcw className="h-3.5 w-3.5" />重试</Button>}
            </div>
          })}
          <div className="flex items-center justify-end gap-2 pt-2"><Button size="sm" variant="outline" disabled={previousCursors.length === 0 || eventsQuery.isFetching} onClick={() => { const cursor = previousCursors[previousCursors.length - 1]; setPreviousCursors((values) => values.slice(0, -1)); setBefore(cursor) }}>上一页</Button><Button size="sm" variant="outline" disabled={!showNext || eventsQuery.isFetching} onClick={() => { const last = rows[rows.length - 1]; const cursor = last?.id; if (cursor === undefined) return; setPreviousCursors((values) => [...values, before]); setBefore(cursor) }}>下一页</Button></div>
        </CardContent>
      </Card>
    </div>
  )
}

function Field({ children, className, label }: { children: React.ReactNode; className?: string; label: string }) {
  return <label className={`block space-y-1 ${className ?? ''}`}><span className="text-xs font-medium text-muted-foreground">{label}</span>{children}</label>
}
