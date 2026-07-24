import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Bell, CheckCheck, Clipboard, CloudCog, Loader2, PackagePlus, RefreshCw,
  RotateCcw, Send, ShieldCheck, Webhook,
} from 'lucide-react'
import {
  getSupplierConfig, getSupplierOverview, listSupplierEvents, manualPurchaseSupplier,
  markSupplierEventsRead, registerSupplierWebhook, retrySupplierEvent, testSupplierWebhook,
  updateSupplierConfig,
} from '@/api/key-supplier'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { extractErrorMessage } from '@/lib/utils'
import { getSupplierEventStatusLabel, hasUnreadSupplierEvents, parseSupplierNumberDraft } from '@/lib/key-supplier'
import type { SupplierConfigUpdate, SupplierEvent, SupplierEventStatus } from '@/types/api'

const EVENT_PAGE_SIZE = 20

type SupplierNumericField = 'minPurchase' | 'maxPurchase' | 'rpmLimit' | 'priority'
type NumericDrafts = Record<SupplierNumericField, string>

function toNumericDrafts(config: Pick<SupplierConfigUpdate, SupplierNumericField>): NumericDrafts {
  return {
    minPurchase: String(config.minPurchase),
    maxPurchase: String(config.maxPurchase),
    rpmLimit: String(config.rpmLimit),
    priority: String(config.priority),
  }
}

function parseGroups(value: string): string[] {
  return value.split(',').map((group) => group.trim()).filter(Boolean)
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
  const [config, setConfig] = useState<SupplierConfigUpdate | null>(null)
  const [apiKey, setApiKey] = useState('')
  const [webhookToken, setWebhookToken] = useState('')
  const [groupsText, setGroupsText] = useState('')
  const [numericDrafts, setNumericDrafts] = useState<NumericDrafts>({
    minPurchase: '', maxPurchase: '', rpmLimit: '', priority: '',
  })
  const [purchaseCountDraft, setPurchaseCountDraft] = useState('1')
  const [selectedIds, setSelectedIds] = useState<number[]>([])
  const [before, setBefore] = useState<number | undefined>()
  const [previousCursors, setPreviousCursors] = useState<Array<number | undefined>>([])
  const previousEvents = useRef<readonly SupplierEvent[] | null>(null)
  const seenEventIds = useRef(new Set<string>())

  const configQuery = useQuery({ queryKey: ['supplier-config'], queryFn: getSupplierConfig })
  const overviewQuery = useQuery({
    queryKey: ['supplier-overview'],
    queryFn: getSupplierOverview,
    refetchInterval: 30000,
  })
  const eventsQuery = useQuery({
    queryKey: ['supplier-events', before],
    queryFn: () => listSupplierEvents({ limit: EVENT_PAGE_SIZE, before }),
    refetchInterval: 5000,
  })

  useEffect(() => {
    if (!configQuery.data) return
    const next = configQuery.data
    setConfig({
      baseUrl: next.baseUrl,
      publicBaseUrl: next.publicBaseUrl,
      autoPurchase: next.autoPurchase,
      minPurchase: next.minPurchase,
      maxPurchase: next.maxPurchase,
      apiRegion: next.apiRegion,
      rpmLimit: next.rpmLimit,
      priority: next.priority,
      groups: next.groups,
      sourceChannel: next.sourceChannel,
      nicknamePrefix: next.nicknamePrefix,
    })
    setGroupsText(next.groups.join(', '))
    setNumericDrafts(toNumericDrafts(next))
  }, [configQuery.data])

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
        toast.info('收到新的供应商事件', { description: `${event.eventType} · ${event.eventId}` })
      }
      seenEventIds.current.add(event.eventId)
    })
    previousEvents.current = current
  }, [before, eventsQuery.data])

  const invalidateSupplier = () => queryClient.invalidateQueries({ queryKey: ['supplier-events'] })

  const saveConfig = useMutation({
    mutationFn: updateSupplierConfig,
    onSuccess: (saved) => {
      queryClient.setQueryData(['supplier-config'], saved)
      setApiKey('')
      setWebhookToken('')
      toast.success('供应商配置已保存')
    },
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const purchase = useMutation({
    mutationFn: manualPurchaseSupplier,
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
    mutationFn: registerSupplierWebhook,
    onSuccess: () => toast.success('Webhook 已注册'),
    onError: (error) => toast.error(extractErrorMessage(error)),
  })
  const testWebhook = useMutation({
    mutationFn: testSupplierWebhook,
    onSuccess: (result) => result.success ? toast.success('Webhook 测试成功') : toast.error('Webhook 测试未通过'),
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

  const updateField = <K extends keyof SupplierConfigUpdate>(field: K, value: SupplierConfigUpdate[K]) => {
    setConfig((current) => current ? { ...current, [field]: value } : current)
  }
  const updateNumericDraft = (field: SupplierNumericField, value: string) => {
    setNumericDrafts((current) => ({ ...current, [field]: value }))
  }
  const parsedMinPurchase = parseSupplierNumberDraft(numericDrafts.minPurchase, 0)
  const parsedMaxPurchase = parseSupplierNumberDraft(numericDrafts.maxPurchase, 0)
  const parsedRpmLimit = parseSupplierNumberDraft(numericDrafts.rpmLimit, 0)
  const parsedPriority = parseSupplierNumberDraft(numericDrafts.priority, 0)
  const configNumbersValid = [parsedMinPurchase, parsedMaxPurchase, parsedRpmLimit, parsedPriority]
    .every((value) => value !== null)
  const parsedPurchaseCount = parseSupplierNumberDraft(purchaseCountDraft, 1)
  const handleSave = () => {
    if (!config) return
    if (
      parsedMinPurchase === null || parsedMaxPurchase === null ||
      parsedRpmLimit === null || parsedPriority === null
    ) {
      toast.error('请输入有效的非负整数配置')
      return
    }
    saveConfig.mutate({
      ...config,
      minPurchase: parsedMinPurchase,
      maxPurchase: parsedMaxPurchase,
      rpmLimit: parsedRpmLimit,
      priority: parsedPriority,
      groups: parseGroups(groupsText),
      apiKey: apiKey || undefined,
      webhookToken: webhookToken || undefined,
    })
  }
  const copyCallbackUrl = async () => {
    const callbackUrl = registerWebhook.data?.callbackUrl
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
          <CardTitle className="flex items-center gap-2"><ShieldCheck className="h-4 w-4" />供应概览</CardTitle>
          <CardDescription>安全额度与库存状态，每 30 秒刷新。</CardDescription>
        </CardHeader>
        <CardContent>
          {overviewQuery.isLoading ? <div className="py-4 text-sm text-muted-foreground">加载中...</div> : overviewQuery.isError ? <div className="py-4 text-sm text-destructive">{extractErrorMessage(overviewQuery.error)}</div> : overviewQuery.data ? (
            <div className="grid gap-px overflow-hidden border border-border/50 sm:grid-cols-2 lg:grid-cols-5">
              <Metric label={`Profile · ${overviewQuery.data.profile.name}`} value={`${overviewQuery.data.profile.remaining} / ${overviewQuery.data.profile.quota}`} />
              <Metric label="Stock Max" value={overviewQuery.data.stockMax} />
              <Metric label="可用 Keys" value={overviewQuery.data.status.keysActive} />
              <Metric label="库存 Keys" value={overviewQuery.data.status.keysStock} />
              <Metric label="失效 / 生成中" value={`${overviewQuery.data.status.keysDead} / ${overviewQuery.data.status.generating}`} />
            </div>
          ) : null}
        </CardContent>
      </Card>

      <div className="grid gap-5 xl:grid-cols-2">
        <Card>
          <CardHeader className="pb-3">
            <CardTitle>连接配置</CardTitle>
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
                    <Badge variant={configQuery.data?.apiKeyConfigured ? 'success' : 'secondary'}>API Key {configQuery.data?.apiKeyConfigured ? '已配置' : '未配置'}</Badge>
                    <Badge variant={configQuery.data?.webhookTokenConfigured ? 'success' : 'secondary'}>Webhook Token {configQuery.data?.webhookTokenConfigured ? '已配置' : '未配置'}</Badge>
                  </div>
                  <p className="mt-2 text-muted-foreground">{purchaseResultSummary}</p>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <Field label="Supplier Base URL"><Input value={config.baseUrl} onChange={(event) => updateField('baseUrl', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Public Base URL"><Input value={config.publicBaseUrl} onChange={(event) => updateField('publicBaseUrl', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="API Key（只写入）"><Input type="password" autoComplete="new-password" placeholder={configQuery.data?.apiKeyConfigured ? '已配置；留空则保持不变' : '仅保存时写入'} value={apiKey} onChange={(event) => setApiKey(event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Webhook Token（只写入）"><Input type="password" autoComplete="new-password" placeholder={configQuery.data?.webhookTokenConfigured ? '已配置；留空则保持不变' : '仅保存时写入'} value={webhookToken} onChange={(event) => setWebhookToken(event.target.value)} disabled={saveConfig.isPending} /></Field>
                </div>
                <div className="flex items-center justify-between gap-3 border-y border-border/50 py-3">
                  <div><label htmlFor="auto-purchase" className="text-sm font-medium">自动购买</label><p className="text-xs text-muted-foreground">根据库存下限触发采购。</p></div>
                  <Switch id="auto-purchase" checked={config.autoPurchase} onCheckedChange={(checked) => updateField('autoPurchase', checked)} disabled={saveConfig.isPending} aria-label="自动购买" />
                </div>
                <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-3">
                  <Field label="最小库存"><Input type="number" min={0} value={numericDrafts.minPurchase} onChange={(event) => updateNumericDraft('minPurchase', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="最大库存"><Input type="number" min={0} value={numericDrafts.maxPurchase} onChange={(event) => updateNumericDraft('maxPurchase', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="API Region"><Input value={config.apiRegion} onChange={(event) => updateField('apiRegion', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="RPM"><Input type="number" min={0} value={numericDrafts.rpmLimit} onChange={(event) => updateNumericDraft('rpmLimit', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Priority"><Input type="number" min={0} value={numericDrafts.priority} onChange={(event) => updateNumericDraft('priority', event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Source Channel"><Input value={config.sourceChannel} onChange={(event) => updateField('sourceChannel', event.target.value)} disabled={saveConfig.isPending} /></Field>
                </div>
                <div className="grid gap-3 sm:grid-cols-2">
                  <Field label="Groups（逗号分隔）"><Input value={groupsText} onChange={(event) => setGroupsText(event.target.value)} disabled={saveConfig.isPending} /></Field>
                  <Field label="Nickname Prefix"><Input value={config.nicknamePrefix} onChange={(event) => updateField('nicknamePrefix', event.target.value)} disabled={saveConfig.isPending} /></Field>
                </div>
                {!configNumbersValid && <p className="text-xs text-destructive">请填写有效的非负整数配置。</p>}
                <Button onClick={handleSave} disabled={saveConfig.isPending || !configNumbersValid}>
                  {saveConfig.isPending && <Loader2 className="h-3.5 w-3.5 animate-spin" />}
                  保存配置
                </Button>
              </>
            )}
          </CardContent>
        </Card>

        <div className="space-y-5">
          <Card>
            <CardHeader className="pb-3"><CardTitle>手动购买</CardTitle><CardDescription>{purchaseResultSummary}</CardDescription></CardHeader>
            <CardContent className="flex flex-wrap items-end gap-3">
              <Field label="购买数量" className="w-full sm:w-40"><Input type="number" min={1} value={purchaseCountDraft} onChange={(event) => setPurchaseCountDraft(event.target.value)} disabled={purchase.isPending} /></Field>
              {parsedPurchaseCount === null && <p className="w-full text-xs text-destructive sm:w-auto">请输入大于 0 的整数。</p>}
              <Button onClick={() => { if (parsedPurchaseCount !== null) purchase.mutate(parsedPurchaseCount) }} disabled={purchase.isPending || parsedPurchaseCount === null}><PackagePlus className="h-3.5 w-3.5" />购买</Button>
            </CardContent>
          </Card>
          <Card>
            <CardHeader className="pb-3"><CardTitle className="flex items-center gap-2"><Webhook className="h-4 w-4" />Webhook</CardTitle><CardDescription>注册供应商回调并执行连通性测试。</CardDescription></CardHeader>
            <CardContent className="space-y-3">
              {registerWebhook.data?.callbackUrl && <div className="flex min-w-0 gap-2"><Input aria-label="Webhook callback URL" value={registerWebhook.data.callbackUrl} readOnly /><Button size="icon" variant="outline" title="复制回调地址" onClick={copyCallbackUrl}><Clipboard className="h-3.5 w-3.5" /></Button></div>}
              <div className="flex flex-wrap gap-2"><Button variant="outline" onClick={() => registerWebhook.mutate()} disabled={registerWebhook.isPending}><Webhook className="h-3.5 w-3.5" />注册 Webhook</Button><Button variant="outline" onClick={() => testWebhook.mutate()} disabled={testWebhook.isPending}><Send className="h-3.5 w-3.5" />测试 Webhook</Button></div>
            </CardContent>
          </Card>
        </div>
      </div>

      <Card>
        <CardHeader className="pb-3">
          <div className="flex flex-wrap items-center gap-2"><CardTitle className="flex items-center gap-2"><Bell className="h-4 w-4" />事件历史</CardTitle><Badge variant="secondary">{eventsQuery.data?.unreadCount ?? 0} 未读</Badge><div className="ml-auto flex flex-wrap gap-2"><Button size="sm" variant="outline" onClick={() => markRead.mutate({ ids: selectedIds })} disabled={selectedIds.length === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />标记所选已读</Button><Button size="sm" variant="outline" onClick={() => markRead.mutate({ markAll: true })} disabled={(eventsQuery.data?.unreadCount ?? 0) === 0 || markRead.isPending}><CheckCheck className="h-3.5 w-3.5" />全部标记已读</Button></div></div>
          <CardDescription>每 5 秒刷新；仅展示事件元数据与处理计数。</CardDescription>
        </CardHeader>
        <CardContent className="space-y-2">
          {eventsQuery.isLoading ? <div className="py-5 text-sm text-muted-foreground">加载事件中...</div> : eventsQuery.isError ? <div className="py-5 text-sm text-destructive">{extractErrorMessage(eventsQuery.error)}</div> : rows.length === 0 ? <div className="py-5 text-sm text-muted-foreground">暂无事件。</div> : rows.map((event) => {
            const retryable = event.status === 'failed' || event.status === 'skipped'
            return <div key={event.id} className={`grid gap-2 border border-border/50 p-3 sm:grid-cols-[auto_minmax(0,1fr)_auto] sm:items-center ${event.readAt === null ? 'border-primary/40 bg-primary/[0.03]' : ''}`}>
              <Checkbox checked={selectedIds.includes(event.id)} onCheckedChange={(checked) => toggleSelected(event.id, checked === true)} aria-label={`选择事件 ${event.id}`} />
              <div className="min-w-0 space-y-1"><div className="flex flex-wrap items-center gap-2"><span className="break-all font-mono text-xs text-muted-foreground">{event.eventId}</span><Badge variant={eventBadgeVariant(event.status)}>{getSupplierEventStatusLabel(event.status)}</Badge><span className="min-w-0 break-all text-sm font-medium">{event.eventType}</span><Badge variant="outline">{event.readAt === null ? '未读' : '已读'}</Badge></div><div className="break-words text-xs text-muted-foreground">{eventDetail(event)} · 尝试 {event.attempts} · {formatTime(event.receivedAt)}</div>{event.purchaseOrderId && <div className="break-all text-xs text-muted-foreground">订单 ID：{event.purchaseOrderId}</div>}{event.message && <div className="break-words text-xs text-muted-foreground">{event.message}</div>}{event.lastError && <div className="break-words text-xs text-destructive">{event.lastError}</div>}</div>
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
