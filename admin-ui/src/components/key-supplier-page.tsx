import { useEffect, useMemo, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Bell, Boxes, CheckCheck, ChevronDown, Clipboard, CloudCog, Loader2, PackagePlus, Plus,
  RefreshCw, RotateCcw, Send, Settings2, ShieldCheck, Trash2, Webhook,
} from 'lucide-react'
import {
  createSupplier, deleteSupplier, getSupplierCallbackUrl, getSupplierEntryOverview,
  getSupplierCommon, getSupplierPool, getSupplierPoolStatus, listSuppliers, listSupplierEvents,
  markSupplierEventsRead, purchaseFromSupplier, registerSupplierEntryWebhook, retrySupplierEvent,
  testSupplierEntryWebhook, updateSupplier, updateSupplierCommon, updateSupplierPool,
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
  buildSupplierNicknamePreview, emptySupplierEntry, emptySupplierPool, getSupplierCapabilities, regionModeOptions,
  getSupplierEventStatusLabel, getSupplierKindLabel, hasUnreadSupplierEvents, isValidSupplierId,
  parseSupplierDecimalDraft, parseSupplierNumberDraft, suggestSupplierId,
  toSupplierEntryUpdate, validateSupplierPool,
} from '@/lib/key-supplier'
import type {
  PurchaseRegionMode, SupplierCommonConfig, SupplierDecisionSnapshot, SupplierEntryUpdate,
  SupplierEvent, SupplierEventStatus, SupplierImportOverrides, SupplierKind, SupplierPoolConfig,
  SupplierRegion,
} from '@/types/api'

const EVENT_PAGE_SIZE = 20
const SUPPLIER_KINDS: readonly SupplierKind[] = [
  'kiro-rs', 'kiro-app', 'kiroapp-io', 'kiro-drop', 'kiro-ceo',
]
const REGION_MODE_LABELS: Record<PurchaseRegionMode, string> = {
  omit: '不传区域',
  fixed: '固定区域',
  webhook: '跟随 Webhook',
  bestAvailable: '库存优先',
  batch: '跟随供货批次',
}
const SUPPLIER_REGIONS: readonly SupplierRegion[] = ['us', 'eu']
const SUPPLIER_REGION_LABELS: Record<SupplierRegion, string> = {
  us: '美国区（us）',
  eu: '欧洲区（eu）',
}
const CREDENTIAL_API_REGIONS = [
  { value: 'us-east-1', label: '美国区（us-east-1）' },
  { value: 'eu-central-1', label: '欧洲区（eu-central-1）' },
] as const

type SupplierNumericField =
  | 'minPurchase'
  | 'maxPurchase'
  | 'rpmLimit'
  | 'priority'
  | 'targetUsable'
  | 'lowQuotaThreshold'
  | 'maxUnitPrice'
type NumericDrafts = Record<SupplierNumericField, string>
type ImportOverrideField = keyof SupplierImportOverrides
type ImportOverrideValue = string | number | string[] | boolean

function toNumericDrafts(config: Pick<SupplierEntryUpdate, SupplierNumericField>): NumericDrafts {
  return {
    minPurchase: String(config.minPurchase),
    maxPurchase: String(config.maxPurchase),
    rpmLimit: String(config.rpmLimit),
    priority: String(config.priority),
    targetUsable: String(config.targetUsable),
    lowQuotaThreshold: String(config.lowQuotaThreshold),
    maxUnitPrice: String(config.maxUnitPrice),
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

function importValueFromConfig(
  config: SupplierEntryUpdate,
  field: ImportOverrideField,
): ImportOverrideValue {
  switch (field) {
    case 'sourceChannel': return config.sourceChannel
    case 'nicknameLabel': return config.nicknamePrefix
    case 'rpmLimit': return config.rpmLimit
    case 'priority': return config.priority
    case 'groups': return [...config.groups]
    case 'autoDeleteForbidden': return config.autoDeleteForbidden
  }
}

function importValueFromCommon(
  common: SupplierCommonConfig,
  field: ImportOverrideField,
): ImportOverrideValue {
  return field === 'groups' ? [...common.groups] : common[field]
}

function importConfigPatch(
  field: ImportOverrideField,
  value: ImportOverrideValue,
): Partial<SupplierEntryUpdate> {
  switch (field) {
    case 'sourceChannel': return { sourceChannel: value as string }
    case 'nicknameLabel': return { nicknamePrefix: value as string }
    case 'rpmLimit': return { rpmLimit: value as number }
    case 'priority': return { priority: value as number }
    case 'groups': return { groups: [...value as string[]] }
    case 'autoDeleteForbidden': return { autoDeleteForbidden: value as boolean }
  }
}

function formatDecisionValue(value: string | number | boolean | null | undefined): string {
  if (value === null || value === undefined || value === '') return '—'
  if (typeof value === 'boolean') return value ? '是' : '否'
  return String(value)
}

function DecisionValue({
  label,
  value,
}: {
  label: string
  value: string | number | boolean | null | undefined
}) {
  return (
    <div className="min-w-0">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div className="break-words font-mono text-xs text-foreground">
        {formatDecisionValue(value)}
      </div>
    </div>
  )
}

function DecisionSnapshotDetails({ snapshot }: { snapshot: SupplierDecisionSnapshot }) {
  const health = snapshot.target.health
  return (
    <details className="group mt-2 border-t border-border/50 pt-2">
      <summary className="flex cursor-pointer list-none items-center gap-1.5 text-xs font-medium text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring">
        <ChevronDown className="h-3.5 w-3.5 transition-transform group-open:rotate-180" />
        判定详情
      </summary>
      <div className="mt-3 space-y-3 border-l-2 border-primary/20 pl-3">
        <div className="grid gap-2 sm:grid-cols-3 lg:grid-cols-6">
          <DecisionValue label="结果" value={snapshot.outcome} />
          <DecisionValue label="原因" value={snapshot.reason} />
          <DecisionValue label="目标范围" value={snapshot.target.scope} />
          <DecisionValue label="当时目标" value={snapshot.target.configured} />
          <DecisionValue label="当时计入目标" value={snapshot.target.creditedAtDecision} />
          <DecisionValue label="当时缺口 / 请求" value={`${formatDecisionValue(snapshot.target.deficit)} / ${formatDecisionValue(snapshot.target.requested)}`} />
        </div>
        {health ? (
          <div className="grid gap-2 sm:grid-cols-3 lg:grid-cols-6">
            <DecisionValue label="可调度" value={health.ready} />
            <DecisionValue label="人工保留" value={health.manualReserved} />
            <DecisionValue label="临时冷却" value={health.cooling} />
            <DecisionValue label="系统禁用" value={health.systemDisabled} />
            <DecisionValue label="封禁 / 额度尽" value={`${health.dead} / ${health.quotaExhausted}`} />
            <DecisionValue label="低于额度水位" value={health.lowQuota} />
          </div>
        ) : null}
        <div className="grid gap-2 sm:grid-cols-3 lg:grid-cols-6">
          <DecisionValue label="区域模式" value={snapshot.region.mode} />
          <DecisionValue label="请求区域" value={snapshot.region.requestedRegion} />
          <DecisionValue label="请求区域证据" value={snapshot.region.requestedRegionSource} />
          <DecisionValue label="实际区域" value={snapshot.region.actualRegion} />
          <DecisionValue label="实际区域证据" value={snapshot.region.actualRegionSource} />
          <DecisionValue label="凭据 API 区域兜底" value={snapshot.region.credentialApiRegionFallback} />
        </div>
        <div className="grid gap-2 sm:grid-cols-3 lg:grid-cols-6">
          <DecisionValue label="供货商库存" value={snapshot.quote.vendorStock} />
          <DecisionValue label="报价 / 限价" value={`${formatDecisionValue(snapshot.quote.unitPrice)} / ${formatDecisionValue(snapshot.quote.maxUnitPrice)}`} />
          <DecisionValue label="购买 / 导入" value={`${snapshot.result.purchased} / ${snapshot.result.imported}`} />
          <DecisionValue label="重复 / 失败" value={`${snapshot.result.duplicate} / ${snapshot.result.failed}`} />
          <DecisionValue label="实际扣费" value={snapshot.result.totalDebit} />
          <DecisionValue label="供货商订单" value={snapshot.result.supplierOrderId} />
        </div>
      </div>
    </details>
  )
}

function ImportOverrideSetting({
  children,
  inherited,
  label,
  onInheritedChange,
}: {
  children: React.ReactNode
  inherited: boolean
  label: string
  onInheritedChange: (inherited: boolean) => void
}) {
  return (
    <div className="space-y-2 border border-border/50 p-3">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <span className="text-xs font-medium text-muted-foreground">{label}</span>
        <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
          <Checkbox
            checked={inherited}
            onCheckedChange={(checked) => onInheritedChange(checked === true)}
            aria-label={`${label}继承公共设置`}
          />
          继承公共设置
        </label>
      </div>
      {children}
    </div>
  )
}

export function KeySupplierPage() {
  const queryClient = useQueryClient()
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)
  const [config, setConfig] = useState<SupplierEntryUpdate | null>(null)
  const [commonDraft, setCommonDraft] = useState<SupplierCommonConfig | null>(null)
  const [commonRpmDraft, setCommonRpmDraft] = useState('')
  const [commonPriorityDraft, setCommonPriorityDraft] = useState('')
  const [apiKey, setApiKey] = useState('')
  const [webhookToken, setWebhookToken] = useState('')
  const [webhookSecret, setWebhookSecret] = useState('')
  const groupOptions = useGroupOptions()
  const [numericDrafts, setNumericDrafts] = useState<NumericDrafts>({
    minPurchase: '', maxPurchase: '', rpmLimit: '', priority: '',
    targetUsable: '', lowQuotaThreshold: '', maxUnitPrice: '',
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
  const commonQuery = useQuery({ queryKey: ['supplier-common'], queryFn: getSupplierCommon })
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

  const commonRpm = parseSupplierNumberDraft(commonRpmDraft, 0)
  const commonPriority = parseSupplierNumberDraft(commonPriorityDraft, 0)
  const commonNumbersValid = commonRpm !== null && commonPriority !== null

  useEffect(() => {
    if (!commonQuery.data) return
    setCommonDraft({ ...commonQuery.data, groups: [...commonQuery.data.groups] })
    setCommonRpmDraft(String(commonQuery.data.rpmLimit))
    setCommonPriorityDraft(String(commonQuery.data.priority))
  }, [commonQuery.data])

  const saveCommon = useMutation({
    mutationFn: () => {
      if (!commonDraft || commonRpm === null || commonPriority === null) {
        throw new Error('公共导入设置包含无效数字')
      }
      return updateSupplierCommon({
        ...commonDraft,
        rpmLimit: commonRpm,
        priority: commonPriority,
        groups: [...commonDraft.groups],
      })
    },
    onSuccess: (saved) => {
      queryClient.setQueryData(['supplier-common'], saved)
      queryClient.invalidateQueries({ queryKey: ['supplier-config'] })
      toast.success('公共导入设置已保存')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
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
    mutationFn: async (supplierId: string) => ({
      supplierId,
      ...(await registerSupplierEntryWebhook(supplierId)),
    }),
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
    mutationFn: async (supplierId: string) => ({
      supplierId,
      ...(await getSupplierCallbackUrl(supplierId)),
    }),
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
  const updateImportOverride = (
    field: ImportOverrideField,
    value: ImportOverrideValue,
  ) => {
    setConfig((current) => current ? {
      ...current,
      ...importConfigPatch(field, value),
      importOverrides: {
        ...current.importOverrides,
        [field]: field === 'groups' ? [...value as string[]] : value,
      },
    } : current)
  }
  const setImportFieldInherited = (field: ImportOverrideField, inherited: boolean) => {
    if (inherited && !commonDraft) return
    const commonValue = commonDraft ? importValueFromCommon(commonDraft, field) : null
    setConfig((current) => {
      if (!current) return current
      const importOverrides = { ...current.importOverrides }
      if (inherited && commonValue !== null) {
        delete importOverrides[field]
        return {
          ...current,
          ...importConfigPatch(field, commonValue),
          importOverrides,
        }
      }
      const value = importValueFromConfig(current, field)
      return {
        ...current,
        importOverrides: {
          ...importOverrides,
          [field]: field === 'groups' ? [...value as string[]] : value,
        },
      }
    })
    if (inherited && commonValue !== null && field === 'rpmLimit') setNumericDrafts((values) => ({ ...values, rpmLimit: String(commonValue) }))
    if (inherited && commonValue !== null && field === 'priority') setNumericDrafts((values) => ({ ...values, priority: String(commonValue) }))
  }
  const updateImportNumberDraft = (field: 'rpmLimit' | 'priority', value: string) => {
    updateNumericDraft(field, value)
    const parsed = parseSupplierNumberDraft(value, 0)
    if (parsed !== null) updateImportOverride(field, parsed)
  }
  const startCreating = () => {
    const draft = emptySupplierEntry('kiro-app', commonDraft ?? undefined)
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
  const parsedTargetUsable = parseSupplierNumberDraft(
    numericDrafts.targetUsable,
    0,
  )
  const parsedLowQuotaThreshold = parseSupplierNumberDraft(numericDrafts.lowQuotaThreshold, 0)
  // 单价可以是小数（Drop 报 2.20），所以不能走整数草稿解析。
  const parsedMaxUnitPrice = parseSupplierDecimalDraft(numericDrafts.maxUnitPrice, 0)
  const idValid = !creating || isValidSupplierId(config?.id ?? '')
  const configNumbersValid = parsedMinPurchase !== null && parsedMaxPurchase !== null &&
    parsedRpmLimit !== null && parsedPriority !== null && parsedMinPurchase <= parsedMaxPurchase &&
    parsedTargetUsable !== null && parsedLowQuotaThreshold !== null &&
    parsedMaxUnitPrice !== null &&
    idValid
  const parsedPurchaseCount = parseSupplierNumberDraft(purchaseCountDraft, 1)
  const purchaseCountValid = parsedPurchaseCount !== null && config !== null &&
    parsedPurchaseCount >= config.minPurchase && parsedPurchaseCount <= config.maxPurchase
  const capabilities = config ? getSupplierCapabilities(config.kind) : null
  const supportsWebhookRegistration =
    selectedEntry?.capabilities.supportsWebhookRegistration ?? false
  const nicknamePreview = config
    ? buildSupplierNicknamePreview(config.name, config.id, config.nicknamePrefix)
    : ''
  const changeSupplierKind = (kind: SupplierKind) => {
    const defaults = emptySupplierEntry(kind, commonDraft ?? undefined)
    setConfig((current) => current ? {
      ...current,
      kind,
      baseUrl: defaults.baseUrl,
      purchaseRegionMode: defaults.purchaseRegionMode,
      purchaseRegion: defaults.purchaseRegion,
      apiRegion: defaults.apiRegion,
      credentialApiRegionFallback: defaults.credentialApiRegionFallback,
    } : current)
  }
  const handleSave = () => {
    if (!config) return
    if (
      parsedMinPurchase === null || parsedMaxPurchase === null ||
      parsedRpmLimit === null || parsedPriority === null ||
      parsedTargetUsable === null || parsedLowQuotaThreshold === null ||
      parsedMaxUnitPrice === null
    ) {
      toast.error('请输入有效的非负数配置')
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
      targetUsable: parsedTargetUsable,
      lowQuotaThreshold: parsedLowQuotaThreshold,
      maxUnitPrice: parsedMaxUnitPrice,
      apiRegion: config.credentialApiRegionFallback,
      apiKey: apiKey || undefined,
      webhookToken: webhookToken || undefined,
      webhookSecret: webhookSecret || undefined,
    })
  }
  const copyCallbackUrl = async () => {
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
  const callbackUrl = callbackUrlQuery.data?.supplierId === selectedId
    ? callbackUrlQuery.data.callbackUrl
    : registerWebhook.data?.supplierId === selectedId
      ? registerWebhook.data.callbackUrl
      : undefined

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
          <CardTitle className="flex items-center gap-2">
            <Settings2 className="h-4 w-4" />
            公共导入设置
          </CardTitle>
          <CardDescription>新供应商默认继承这里的凭据导入属性；单家只保存明确覆盖的字段。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-4">
          {commonQuery.isLoading ? (
            <div className="py-4 text-sm text-muted-foreground">加载公共设置中...</div>
          ) : commonQuery.isError ? (
            <div className="flex flex-wrap items-center gap-3 text-sm text-destructive" role="alert">
              <span>{extractErrorMessage(commonQuery.error)}</span>
              <Button size="sm" variant="outline" onClick={() => commonQuery.refetch()}>
                <RefreshCw className="h-3.5 w-3.5" />
                重试加载
              </Button>
            </div>
          ) : commonDraft ? (
            <>
              <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
                <Field label="来源渠道">
                  <Input
                    value={commonDraft.sourceChannel}
                    onChange={(event) => setCommonDraft({ ...commonDraft, sourceChannel: event.target.value })}
                    disabled={saveCommon.isPending}
                  />
                </Field>
                <Field label="Nickname 标签（可选）">
                  <Input
                    value={commonDraft.nicknameLabel}
                    onChange={(event) => setCommonDraft({ ...commonDraft, nicknameLabel: event.target.value })}
                    disabled={saveCommon.isPending}
                  />
                </Field>
                <Field label="自动采购 RPM 预设">
                  <Input
                    type="number"
                    min={0}
                    value={commonRpmDraft}
                    onChange={(event) => setCommonRpmDraft(event.target.value)}
                    disabled={saveCommon.isPending}
                  />
                </Field>
                <Field label="Priority">
                  <Input
                    type="number"
                    min={0}
                    value={commonPriorityDraft}
                    onChange={(event) => setCommonPriorityDraft(event.target.value)}
                    disabled={saveCommon.isPending}
                  />
                </Field>
              </div>
              <div className="grid gap-3 lg:grid-cols-[minmax(0,1fr)_minmax(18rem,0.7fr)] lg:items-end">
                <Field label="自动采购分组">
                  <GroupMultiSelect
                    value={commonDraft.groups}
                    options={groupOptions}
                    onChange={(groups) => setCommonDraft({ ...commonDraft, groups })}
                    disabled={saveCommon.isPending}
                  />
                </Field>
                <div className="flex min-h-9 items-center justify-between gap-3 border border-border/50 px-3 py-2">
                  <label htmlFor="common-auto-delete-forbidden" className="text-sm font-medium">403 时自动删除</label>
                  <Switch
                    id="common-auto-delete-forbidden"
                    checked={commonDraft.autoDeleteForbidden}
                    onCheckedChange={(checked) => setCommonDraft({ ...commonDraft, autoDeleteForbidden: checked })}
                    disabled={saveCommon.isPending}
                  />
                </div>
              </div>
              <div className="flex flex-wrap items-center gap-3 border-t border-border/50 pt-3">
                <div className="min-w-0 text-xs text-muted-foreground">
                  Nickname 预览：
                  <code className="ml-1 break-all text-foreground">
                    {buildSupplierNicknamePreview('ceo', 'ceo', commonDraft.nicknameLabel)}
                  </code>
                </div>
                {!commonNumbersValid && (
                  <span className="text-xs text-destructive" role="alert">RPM 和 Priority 必须是非负整数。</span>
                )}
                <Button
                  className="ml-auto"
                  size="sm"
                  onClick={() => saveCommon.mutate()}
                  disabled={saveCommon.isPending || !commonNumbersValid}
                >
                  {saveCommon.isPending && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                  保存公共设置
                </Button>
              </div>
            </>
          ) : null}
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
              <div className="grid gap-px overflow-hidden border border-border/50 sm:grid-cols-2 lg:grid-cols-3 xl:grid-cols-6">
                <Metric label="目标存量" value={poolStatusQuery.data.targetCount} />
                <Metric label="计入目标" value={poolStatusQuery.data.health.targetCredited} />
                <Metric label="当前可调度" value={poolStatusQuery.data.health.ready} />
                <Metric label="还差" value={poolStatusQuery.data.deficit} />
                <Metric
                  label="人工暂停 / 冷却"
                  value={`${poolStatusQuery.data.health.manualReserved} / ${poolStatusQuery.data.health.cooling}`}
                />
                <Metric label="系统禁用" value={poolStatusQuery.data.health.systemDisabled} />
              </div>
              <div className="text-xs text-muted-foreground">
                系统禁用不计入目标存量；人工暂停和临时冷却仍计入，避免短时状态触发重复采购。
                已判死 {poolStatusQuery.data.health.dead} 个 · 额度耗尽 {poolStatusQuery.data.health.quotaExhausted} 个 · 低于水位 {poolStatusQuery.data.health.lowQuota} 个。
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
          <CardContent className="space-y-3">
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
                    <Metric label="本地号池（计入目标 / 共）" value={`${overviewQuery.data.credentialHealth.targetCredited} / ${overviewQuery.data.credentialHealth.total}`} />
                    <Metric label="可调度 / 系统禁用" value={`${overviewQuery.data.credentialHealth.ready} / ${overviewQuery.data.credentialHealth.systemDisabled}`} />
                  </>
                )}
              </div>
            ) : null}
            {overviewQuery.data ? (
              <div className="text-xs text-muted-foreground">
                人工暂停 {overviewQuery.data.credentialHealth.manualReserved} · 临时冷却 {overviewQuery.data.credentialHealth.cooling} ·
                不可用构成：封禁 {overviewQuery.data.credentialHealth.dead} · 额度耗尽 {overviewQuery.data.credentialHealth.quotaExhausted} ·
                低于额度水位 {overviewQuery.data.credentialHealth.lowQuota}。系统禁用的凭据不占目标存量。
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
                      onChange={(event) => changeSupplierKind(event.target.value as SupplierKind)}
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
                  <Field label="目标存量（本家常备可用号数）"><Input type="number" min={0} value={numericDrafts.targetUsable} onChange={(event) => updateNumericDraft('targetUsable', event.target.value)} disabled={saveConfig.isPending || !config.restockOnlyWhenExhausted} /></Field>
                  <Field label="单价上限（0 = 不限，按本家计价单位）"><Input type="number" min={0} step="0.01" value={numericDrafts.maxUnitPrice} onChange={(event) => updateNumericDraft('maxUnitPrice', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="额度水位（剩余额度）"><Input type="number" min={0} value={numericDrafts.lowQuotaThreshold} onChange={(event) => updateNumericDraft('lowQuotaThreshold', event.target.value)} disabled={saveConfig.isPending || !config.restockOnlyWhenExhausted} /></Field>
                  {capabilities && capabilities.regionModes.some((mode) => mode !== 'omit') ? (
                    <Field label="采购区域模式">
                      <select
                        className="h-9 w-full border border-input bg-transparent px-3 text-sm"
                        aria-label="采购区域模式"
                        value={config.purchaseRegionMode}
                        onChange={(event) => {
                          const mode = event.target.value as PurchaseRegionMode
                          updateField('purchaseRegionMode', mode)
                          if (mode === 'fixed' && config.purchaseRegion === null) updateField('purchaseRegion', 'us')
                        }}
                        disabled={saveConfig.isPending}
                      >
                        {regionModeOptions(capabilities.regionModes, config.purchaseRegionMode).map((mode) => (
                          <option key={mode} value={mode}>
                            {REGION_MODE_LABELS[mode]}
                            {capabilities.regionModes.includes(mode) ? '' : '（旧配置）'}
                          </option>
                        ))}
                      </select>
                    </Field>
                  ) : null}
                  {config.purchaseRegionMode === 'fixed' ? (
                    <Field label="采购区域">
                      <select
                        className="h-9 w-full border border-input bg-transparent px-3 text-sm"
                        aria-label="采购区域"
                        value={config.purchaseRegion ?? 'us'}
                        onChange={(event) => updateField('purchaseRegion', event.target.value as SupplierRegion)}
                        disabled={saveConfig.isPending}
                      >
                        {SUPPLIER_REGIONS.map((region) => (
                          <option key={region} value={region}>{SUPPLIER_REGION_LABELS[region]}</option>
                        ))}
                      </select>
                    </Field>
                  ) : null}
                  <Field label="凭据 API 区域兜底">
                    <select
                      className="h-9 w-full border border-input bg-transparent px-3 text-sm"
                      aria-label="凭据 API 区域兜底"
                      value={config.credentialApiRegionFallback}
                      onChange={(event) => {
                        updateField('credentialApiRegionFallback', event.target.value)
                        updateField('apiRegion', event.target.value)
                      }}
                      disabled={saveConfig.isPending}
                    >
                      {CREDENTIAL_API_REGIONS.map((region) => (
                        <option key={region.value} value={region.value}>{region.label}</option>
                      ))}
                    </select>
                  </Field>
                </div>
                <div className="space-y-3 border-t border-border/50 pt-4">
                  <div>
                    <h3 className="text-sm font-semibold">本家导入设置</h3>
                    <p className="text-xs text-muted-foreground">默认继承公共导入设置；关闭继承后，仅本供应商使用覆盖值。</p>
                  </div>
                  <div className="grid gap-3 sm:grid-cols-2">
                    <ImportOverrideSetting
                      label="来源渠道"
                      inherited={config.importOverrides.sourceChannel === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('sourceChannel', inherited)}
                    >
                      <Input
                        value={config.sourceChannel}
                        onChange={(event) => updateImportOverride('sourceChannel', event.target.value)}
                        disabled={saveConfig.isPending || config.importOverrides.sourceChannel === undefined}
                      />
                    </ImportOverrideSetting>
                    <ImportOverrideSetting
                      label="Nickname 标签（可选）"
                      inherited={config.importOverrides.nicknameLabel === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('nicknameLabel', inherited)}
                    >
                      <Input
                        value={config.nicknamePrefix}
                        onChange={(event) => updateImportOverride('nicknameLabel', event.target.value)}
                        disabled={saveConfig.isPending || config.importOverrides.nicknameLabel === undefined}
                      />
                    </ImportOverrideSetting>
                    <ImportOverrideSetting
                      label="RPM"
                      inherited={config.importOverrides.rpmLimit === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('rpmLimit', inherited)}
                    >
                      <Input
                        type="number"
                        min={0}
                        value={numericDrafts.rpmLimit}
                        onChange={(event) => updateImportNumberDraft('rpmLimit', event.target.value)}
                        disabled={saveConfig.isPending || config.importOverrides.rpmLimit === undefined}
                      />
                    </ImportOverrideSetting>
                    <ImportOverrideSetting
                      label="Priority"
                      inherited={config.importOverrides.priority === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('priority', inherited)}
                    >
                      <Input
                        type="number"
                        min={0}
                        value={numericDrafts.priority}
                        onChange={(event) => updateImportNumberDraft('priority', event.target.value)}
                        disabled={saveConfig.isPending || config.importOverrides.priority === undefined}
                      />
                    </ImportOverrideSetting>
                    <ImportOverrideSetting
                      label="自动采购分组"
                      inherited={config.importOverrides.groups === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('groups', inherited)}
                    >
                      <GroupMultiSelect
                        value={config.groups}
                        options={groupOptions}
                        onChange={(groups) => updateImportOverride('groups', groups)}
                        disabled={saveConfig.isPending || config.importOverrides.groups === undefined}
                      />
                    </ImportOverrideSetting>
                    <ImportOverrideSetting
                      label="403 时自动删除"
                      inherited={config.importOverrides.autoDeleteForbidden === undefined}
                      onInheritedChange={(inherited) => setImportFieldInherited('autoDeleteForbidden', inherited)}
                    >
                      <div className="flex min-h-9 items-center justify-between gap-3 px-1">
                        <span className="text-xs text-muted-foreground">当前值：{config.autoDeleteForbidden ? '开启' : '关闭'}</span>
                        <Switch
                          checked={config.autoDeleteForbidden}
                          onCheckedChange={(checked) => updateImportOverride('autoDeleteForbidden', checked)}
                          disabled={saveConfig.isPending || config.importOverrides.autoDeleteForbidden === undefined}
                          aria-label="本家 403 时自动删除"
                        />
                      </div>
                    </ImportOverrideSetting>
                  </div>
                  <div className="text-xs text-muted-foreground">
                    Nickname 预览：<code className="break-all text-foreground">{nicknamePreview}</code>
                  </div>
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
                  : selectedEntry?.kind === 'kiroapp-io'
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
                    <Button variant="outline" onClick={() => registerWebhook.mutate(selectedId as string)} disabled={registerWebhook.isPending || creating || selectedId === null}><Webhook className="h-3.5 w-3.5" />{overviewQuery.data?.webhookRegistered ? '重新注册 Webhook' : '注册 Webhook'}</Button>
                    <Button variant="outline" onClick={() => testWebhook.mutate()} disabled={testWebhook.isPending || creating || selectedId === null}><Send className="h-3.5 w-3.5" />测试 Webhook</Button>
                  </>
                ) : (
                  <Button variant="outline" onClick={() => callbackUrlQuery.mutate(selectedId as string)} disabled={callbackUrlQuery.isPending || creating || selectedId === null}><Clipboard className="h-3.5 w-3.5" />获取回调地址</Button>
                )}
              </div>
            </CardContent>
          </Card>
        </div>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <div className="flex flex-wrap items-center gap-2"><CardTitle className="flex items-center gap-2"><Bell className="h-4 w-4" />事件历史</CardTitle><Badge variant="secondary">{eventsQuery.data?.unreadCount ?? 0} 未读</Badge><div className="ml-auto flex flex-wrap gap-2"><Button size="sm" variant={scopeToSupplier ? 'default' : 'outline'} onClick={() => { setScopeToSupplier((current) => !current); setBefore(undefined); setPreviousCursors([]) }} disabled={selectedId === null}>{scopeToSupplier ? '只看当前供货商' : '全部供货商'}</Button><Button size="sm" variant="outline" onClick={() => markRead.mutate({ ids: selectedIds })} disabled={selectedIds.length === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />标记所选已读</Button><Button size="sm" variant="outline" onClick={() => markRead.mutate({ markAll: true, supplierId: eventSupplierId })} disabled={(eventsQuery.data?.unreadCount ?? 0) === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />全部标记已读</Button></div></div>
          <CardDescription>每 5 秒刷新；记录处理计数以及当时的存量、报价与区域判定。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-2">
          {eventsQuery.isLoading ? <div className="py-5 text-sm text-muted-foreground">加载事件中...</div> : eventsQuery.isError ? <div className="py-5 text-sm text-destructive">{extractErrorMessage(eventsQuery.error)}</div> : rows.length === 0 ? <div className="py-5 text-sm text-muted-foreground">暂无事件。</div> : rows.map((event) => {
            // 等自动重试的事件也给重试按钮：人明确要求现在就试，不该还让他等退避走完。
            const retryable = event.status === 'failed' || event.status === 'skipped' || event.retryAfter !== null
            return <div key={event.id} className={`grid gap-2 border border-border/50 p-3 sm:grid-cols-[auto_minmax(0,1fr)_auto] sm:items-center ${event.readAt === null ? 'border-primary/40 bg-primary/[0.03]' : ''}`}>
              <Checkbox checked={selectedIds.includes(event.id)} onCheckedChange={(checked) => toggleSelected(event.id, checked === true)} aria-label={`选择事件 ${event.id}`} />
              <div className="min-w-0 space-y-1">
                <div className="flex flex-wrap items-center gap-2">
                  <Badge variant="outline">{event.supplierId}</Badge>
                  <span className="break-all font-mono text-xs text-muted-foreground">{event.eventId}</span>
                  <Badge variant={eventBadgeVariant(event.status)}>{getSupplierEventStatusLabel(event.status)}</Badge>
                  {event.retryAfter !== null && <Badge variant="warning">{formatTime(event.retryAfter)} 自动重试</Badge>}
                  <span className="min-w-0 break-all text-sm font-medium">{event.eventType}</span>
                  <Badge variant="outline">{event.readAt === null ? '未读' : '已读'}</Badge>
                </div>
                <div className="break-words text-xs text-muted-foreground">
                  {eventDetail(event)} · 尝试 {event.attempts} · 重复推送 {event.webhookDuplicateCount} · {formatTime(event.receivedAt)}
                </div>
                {event.purchaseOrderId && <div className="break-all text-xs text-muted-foreground">订单 ID：{event.purchaseOrderId}</div>}
                {event.message && <div className="break-words text-xs text-muted-foreground">{event.message}</div>}
                {event.lastError && <div className="break-words text-xs text-destructive">{event.lastError}</div>}
                {event.decisionSnapshot ? <DecisionSnapshotDetails snapshot={event.decisionSnapshot} /> : null}
              </div>
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
