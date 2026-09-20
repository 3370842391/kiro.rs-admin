import { useState } from 'react'
import { toast } from 'sonner'
import {
  Trash2,
  Plus,
  Upload,
  ToggleLeft,
  ToggleRight,
  Globe,
  Activity,
  Shuffle,
  CheckCircle2,
  XCircle,
  HelpCircle,
  Skull,
  ShieldAlert,
  Fingerprint,
  Search,
  Users,
  AlertTriangle,
  Info,
  Copy,
} from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Checkbox } from '@/components/ui/checkbox'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  getProxyPool,
  addProxy,
  batchAddProxies,
  batchDeleteProxies,
  deleteProxy,
  setProxyEnabled,
  getGlobalProxy,
  setGlobalProxy,
  getProxyBalancingMode,
  setProxyBalancingMode,
  PROXY_BALANCING_LABEL,
  checkProxy,
  checkProxyReputation,
  assignProxiesRoundRobin,
  type ProxyBalancingMode,
} from '@/api/credentials'
import type { ProxyScheme } from '@/types/api'
import { cn, extractErrorMessage, proxyDisplayHost, proxySchemeLabel } from '@/lib/utils'
import {
  ProxyBanBadge,
  ProxyBanStatsPanel,
  ProxyReputationBadge,
  ProxyWeightBadge,
  formatSurvival,
} from '@/components/proxy-ban-stats-panel'
import { ProxyGuardDialog } from '@/components/proxy-guard-dialog'
import type { ProxyPoolEntry } from '@/types/api'

interface ProxyPoolDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 点击"分配"按钮时的回调（传入代理 URL，用于编辑凭据） */
  onSelectProxy?: (url: string) => void
}

function entryHost(proxy: ProxyPoolEntry): string {
  return proxy.host || proxyDisplayHost(proxy.url)
}

async function copyProxyHost(host: string) {
  if (!host) return
  try {
    await navigator.clipboard.writeText(host)
    toast.success(`已复制 ${host}`)
  } catch {
    toast.error('复制失败，请手动选择地址')
  }
}

function splitProxyCandidates(raw: string): string[] {
  return raw
    .split(/[,;\s]+/)
    .map((item) => item.trim())
    .filter(Boolean)
}

function normalizeProxyCandidates(candidates: string[]): string[] {
  const seen = new Set<string>()
  const out: string[] = []
  for (const raw of candidates) {
    const value = raw.trim()
    if (!value) continue
    const key = value.toLowerCase() === 'direct' ? 'direct' : value
    if (seen.has(key)) continue
    seen.add(key)
    out.push(key === 'direct' ? 'direct' : value)
  }
  return out
}

const PROXY_MODE_OPTIONS: ProxyBalancingMode[] = ['sticky', 'round_robin', 'least_load']
type BatchAction = 'check' | 'enable' | 'disable' | 'global' | 'unglobal' | 'delete' | null
type PoolTab = 'pool' | 'bans'

/** 池级告警的一条。整条压成一行，长说明进 title */
interface PoolAlert {
  key: string
  tone: 'danger' | 'warn' | 'info'
  /** 一行以内说清「是什么」 */
  text: string
  /** 展开的完整说明，含为什么要管 */
  hint: string
  action?: { label: string; onClick: () => void; disabled?: boolean }
}

const ALERT_TONE: Record<PoolAlert['tone'], string> = {
  danger: 'border-destructive/50 bg-destructive/10 text-destructive',
  warn: 'border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300',
  info: 'border-border bg-muted/40 text-muted-foreground',
}

/**
 * 池级告警区。
 *
 * 此前每类问题各占一个整块彩色告警框，条件同时成立时列表上方会堆四五个框、
 * 每个都是一段小字，真正要看的代理列表被挤到屏幕外——这是「界面乱」的主因。
 * 现在压成每类一行：一行说清是什么、多少个、点哪个按钮，完整解释放 title。
 */
function PoolAlerts({ alerts }: { alerts: PoolAlert[] }) {
  if (alerts.length === 0) return null
  return (
    <div className="divide-y overflow-hidden rounded-md border">
      {alerts.map((alert) => (
        <div
          key={alert.key}
          className={cn(
            'flex items-center gap-2 px-2.5 py-1.5 text-xs',
            ALERT_TONE[alert.tone],
          )}
          title={alert.hint}
        >
          {alert.tone === 'danger' ? (
            <ShieldAlert className="h-3.5 w-3.5 shrink-0" />
          ) : alert.tone === 'warn' ? (
            <AlertTriangle className="h-3.5 w-3.5 shrink-0" />
          ) : (
            <Info className="h-3.5 w-3.5 shrink-0" />
          )}
          <span className="min-w-0 flex-1 truncate">{alert.text}</span>
          {alert.action && (
            <Button
              size="sm"
              variant="outline"
              className="h-6 shrink-0 px-2 text-[11px]"
              onClick={alert.action.onClick}
              disabled={alert.action.disabled}
            >
              {alert.action.label}
            </Button>
          )}
        </div>
      ))}
    </div>
  )
}

/**
 * 出口筛选条件。围绕「哪些是坏 IP」设计——挑出来批量删除是主要用途，
 * 所以每一项都对应一个已知的淘汰理由，而不是把所有字段都做成筛选器。
 */
type ProxyFilterKey =
  | 'burned'
  | 'burned24h'
  | 'demoted'
  | 'flagged'
  | 'unhealthy'
  | 'disabled'
  | 'idle'
  | 'clean'

const PROXY_FILTERS: {
  key: ProxyFilterKey
  label: string
  hint: string
  match: (proxy: ProxyPoolEntry) => boolean
}[] = [
  {
    key: 'burned',
    label: '烧过号',
    hint: '历史上有账号在这个出口被判死',
    match: (p) => (p.banStats?.totalBans ?? 0) > 0,
  },
  {
    key: 'burned24h',
    label: '24h 内烧号',
    hint: '最近一天烧过号。出口是会换 IP 的，近期证据比累计更能说明现在的状态',
    match: (p) => (p.banStats?.bans24h ?? 0) > 0,
  },
  {
    key: 'demoted',
    label: '已降权',
    hint: '封号率置信下界高于全池基线，分配时排在干净出口之后',
    match: (p) => p.risk != null && p.risk.selectionTier !== 'normal',
  },
  {
    key: 'flagged',
    label: '被标记为代理',
    hint: '公开情报库把它标成代理/VPN。实测这一项直接影响账号寿命',
    match: (p) => p.reputationGrade === 'flaggedProxy',
  },
  {
    key: 'unhealthy',
    label: '连通异常',
    hint: '健康检查失败',
    match: (p) => p.health === 'unhealthy',
  },
  {
    key: 'disabled',
    label: '已禁用',
    hint: '手动禁用、自动禁用或烧号隔离',
    match: (p) => !p.enabled,
  },
  {
    key: 'idle',
    label: '空闲',
    hint: '当前没有凭据绑定，删掉不影响在跑的号',
    match: (p) => p.credentialCount === 0,
  },
  {
    key: 'clean',
    label: '零封号',
    hint: '台账里一次都没烧过号',
    match: (p) => (p.banStats?.totalBans ?? 0) === 0,
  },
]

export function ProxyPoolDialog({ open, onOpenChange, onSelectProxy }: ProxyPoolDialogProps) {
  const [tab, setTab] = useState<PoolTab>('pool')
  const [newUrl, setNewUrl] = useState('')
  const [newLabel, setNewLabel] = useState('')
  const [batchText, setBatchText] = useState('')
  const [showBatch, setShowBatch] = useState(false)
  const [batchErrors, setBatchErrors] = useState<string[]>([])
  const [selectedIds, setSelectedIds] = useState<Set<number>>(() => new Set())
  const [checkingIds, setCheckingIds] = useState<Set<number>>(() => new Set())
  const [batchAction, setBatchAction] = useState<BatchAction>(null)
  const [guardOpen, setGuardOpen] = useState(false)
  const [search, setSearch] = useState('')
  const [activeFilters, setActiveFilters] = useState<Set<ProxyFilterKey>>(() => new Set())
  const [batchScheme, setBatchScheme] = useState<ProxyScheme>('socks5')
  const queryClient = useQueryClient()

  const { data, isLoading } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  const { data: globalProxyData } = useQuery({
    queryKey: ['global-proxy'],
    queryFn: getGlobalProxy,
    enabled: open,
  })

  const { data: proxyBalancingData, isLoading: proxyBalancingLoading } = useQuery({
    queryKey: ['proxy-balancing'],
    queryFn: getProxyBalancingMode,
    enabled: open,
  })

  const setProxyBalancingMutation = useMutation({
    mutationFn: setProxyBalancingMode,
    onSuccess: (res) => {
      toast.success(`代理策略已切换为${PROXY_BALANCING_LABEL[res.mode]}`)
      queryClient.invalidateQueries({ queryKey: ['proxy-balancing'] })
    },
    onError: (err) => toast.error(`切换失败: ${extractErrorMessage(err)}`),
  })

  const reputationMutation = useMutation({
    mutationFn: (ids?: number[]) => checkProxyReputation(ids),
    onSuccess: (res) => {
      if (res.checked === 0) {
        toast.info('没有可检测的出口')
        return
      }
      const parts = [
        res.flaggedProxy > 0 ? `已标记代理 ${res.flaggedProxy}` : null,
        res.hosting > 0 ? `机房 ${res.hosting}` : null,
        res.clean > 0 ? `未标记 ${res.clean}` : null,
        res.unreachable > 0 ? `检测失败 ${res.unreachable}` : null,
        res.mismatched > 0 ? `出口不符 ${res.mismatched}` : null,
      ].filter(Boolean)
      toast.success(`检测 ${res.checked} 个出口：${parts.join('、')}`)
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`检测失败: ${extractErrorMessage(err)}`),
  })

  const setGlobalProxyMutation = useMutation({
    mutationFn: (url: string | null) => setGlobalProxy({ proxyUrl: url }),
    onSuccess: (_, url) => {
      const count = url ? splitProxyCandidates(url).length : 0
      toast.success(url ? `已设置 ${count} 个全局代理候选` : '已清除全局代理')
      queryClient.invalidateQueries({ queryKey: ['global-proxy'] })
    },
    onError: (err) => toast.error(`操作失败: ${extractErrorMessage(err)}`),
  })

  const currentGlobalProxy = globalProxyData?.proxyUrl ?? null
  const globalProxyCandidates = currentGlobalProxy ? splitProxyCandidates(currentGlobalProxy) : []
  const globalProxyCandidateSet = new Set(globalProxyCandidates.filter((c) => c.toLowerCase() !== 'direct'))
  const directGlobalEnabled = globalProxyCandidates.some((c) => c.toLowerCase() === 'direct')
  const proxies = data?.proxies ?? []

  // 筛选：多个条件取交集（「烧过号」+「空闲」= 烧过号且当前没人用，正是最该删的那批）
  const keyword = search.trim().toLowerCase()
  const visibleProxies = proxies.filter((proxy) => {
    if (keyword) {
      const haystack = `${entryHost(proxy)} ${proxy.url} ${proxy.label ?? ''}`.toLowerCase()
      if (!haystack.includes(keyword)) return false
    }
    for (const filter of PROXY_FILTERS) {
      if (activeFilters.has(filter.key) && !filter.match(proxy)) return false
    }
    return true
  })
  const filterActive = keyword.length > 0 || activeFilters.size > 0

  // 选中集合跨筛选保留：先按「烧过号」勾一批、再换条件勾另一批，是常见操作。
  // 但全选框只对当前可见的这批负责，否则「全选」会悄悄带上看不见的条目。
  const selectedProxies = proxies.filter((proxy) => selectedIds.has(proxy.id))
  const selectedCount = selectedProxies.length
  const visibleSelectedCount = visibleProxies.filter((proxy) => selectedIds.has(proxy.id)).length
  const allVisibleSelected =
    visibleProxies.length > 0 && visibleSelectedCount === visibleProxies.length
  let allProxyCheckboxState: boolean | 'indeterminate' = false
  if (allVisibleSelected) {
    allProxyCheckboxState = true
  } else if (visibleSelectedCount > 0) {
    allProxyCheckboxState = 'indeterminate'
  }
  const selectedInUseCount = selectedProxies.filter((proxy) => proxy.credentialCount > 0).length
  const globalPoolCount = proxies.filter((proxy) => globalProxyCandidateSet.has(proxy.url)).length
  // 全池累计封号，含已从池中删除的代理，所以用后端汇总而不是当前列表求和
  const poolTotalBans = data?.totalBans ?? 0
  // 全池合计封号率基线。后端对每个出口都算了同一个值，取任意一个即可。
  // 摆到顶端是因为单看某个出口的「44%」毫无意义——必须和基线比才知道它是否异常。
  const poolBaselineRate = proxies.find((proxy) => proxy.risk != null)?.risk?.pooledBanRate ?? null
  // 已被自动降权的出口。降权是实际生效的行为，比「建议隔离」更值得顶端提示
  const demotedProxies = proxies.filter(
    (proxy) => proxy.risk && proxy.risk.selectionTier !== 'normal'
  )
  // 已被隔离守卫停用的出口。比降权更硬：这些出口已经完全不参与分配
  const quarantinedProxies = proxies.filter((proxy) => proxy.quarantinedAt)
  // 被公开情报库标记为代理的出口。这是目前唯一被实测证明会显著缩短账号寿命的属性
  const flaggedProxies = proxies.filter((proxy) => proxy.reputationGrade === 'flaggedProxy')
  const uncheckedCount = proxies.filter((proxy) => proxy.reputationGrade === 'unknown').length
  const orphanGlobalCandidates = globalProxyCandidates.filter(
    (candidate) =>
      candidate.toLowerCase() !== 'direct' && !proxies.some((proxy) => proxy.url === candidate)
  )

  const addMutation = useMutation({
    mutationFn: () => addProxy({ url: newUrl.trim(), label: newLabel.trim() || undefined }),
    onSuccess: (entry) => {
      toast.success(`代理已添加：${entryHost(entry)}`)
      setNewUrl('')
      setNewLabel('')
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`添加失败: ${extractErrorMessage(err)}`),
  })

  const batchMutation = useMutation({
    mutationFn: () =>
      batchAddProxies({
        urls: batchText.split('\n').map((l) => l.trim()).filter(Boolean),
        scheme: batchScheme,
      }),
    onSuccess: (res) => {
      if (res.errors === 0) {
        toast.success(`批量导入完成：成功 ${res.added} 个`)
      } else {
        toast.info(`批量导入完成：成功 ${res.added} 个，跳过 ${res.errors} 个`)
      }
      setBatchErrors(res.errorMessages)
      setBatchText('')
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`批量导入失败: ${extractErrorMessage(err)}`),
  })

  const assignRoundRobinMutation = useMutation({
    mutationFn: () => assignProxiesRoundRobin(null),
    onSuccess: (res) => {
      toast.success(
        `独立IP：新分配 ${res.assigned}，启用 ${res.enabled ?? 0}，禁用无IP ${res.disabled ?? 0}（可用出口 ${res.proxyCount}）`
      )
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (err) => toast.error(`分配失败: ${extractErrorMessage(err)}`),
  })

  const handleAdd = (e: React.FormEvent) => {
    e.preventDefault()
    if (!newUrl.trim()) return
    addMutation.mutate()
  }

  const saveGlobalCandidates = (candidates: string[]) => {
    const next = normalizeProxyCandidates(candidates)
    return setGlobalProxyMutation.mutateAsync(next.length > 0 ? next.join('\n') : null)
  }

  const toggleSelected = (id: number, checked: boolean) => {
    setSelectedIds((prev) => {
      const next = new Set(prev)
      if (checked) next.add(id)
      else next.delete(id)
      return next
    })
  }

  /** 只对当前筛选出来的这批生效；取消全选也只取消可见的那些 */
  const toggleAllSelected = (checked: boolean) => {
    setSelectedIds((prev) => {
      const next = new Set(prev)
      for (const proxy of visibleProxies) {
        if (checked) next.add(proxy.id)
        else next.delete(proxy.id)
      }
      return next
    })
  }

  const toggleFilter = (key: ProxyFilterKey) => {
    setActiveFilters((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  }

  const clearFilters = () => {
    setActiveFilters(new Set())
    setSearch('')
  }

  const handleBatchDelete = async (force: boolean) => {
    if (selectedCount === 0) return
    const ids = selectedProxies.map((proxy) => proxy.id)
    const warning =
      selectedInUseCount > 0 && force
        ? `\n\n其中 ${selectedInUseCount} 个仍有凭据在用。删掉池内条目不会解绑凭据——` +
          '号照样从那个 IP 出去，只是从此没有健康检查、没有封号统计，也不再自动改绑。'
        : ''
    if (!window.confirm(`确认删除选中的 ${ids.length} 个代理？${warning}`)) return

    setBatchAction('delete')
    try {
      const res = await batchDeleteProxies({ ids, force })
      if (res.deleted > 0) {
        toast.success(`已删除 ${res.deleted} 个代理`)
      }
      if (res.skippedInUse.length > 0) {
        toast.warning(
          `${res.skippedInUse.length} 个出口仍有凭据绑定，已跳过：` +
            res.skippedInUse
              .slice(0, 3)
              .map((item) => `${item.url}（${item.credentialCount} 个号）`)
              .join('、') +
            (res.skippedInUse.length > 3 ? ' 等' : '') +
            '。先把号改绑到干净出口，或用「强制删除」。',
          { duration: 8000 },
        )
      }
      if (res.deleted === 0 && res.skippedInUse.length === 0) {
        toast.info('没有可删除的条目')
      }
      setSelectedIds((prev) => {
        const next = new Set(prev)
        for (const id of ids) next.delete(id)
        return next
      })
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      queryClient.invalidateQueries({ queryKey: ['global-proxy'] })
    } catch (err) {
      toast.error(`批量删除失败: ${extractErrorMessage(err)}`)
    } finally {
      setBatchAction(null)
    }
  }

  const toggleProxyGlobal = async (proxy: ProxyPoolEntry, checked: boolean) => {
    try {
      const next = checked
        ? [...globalProxyCandidates, proxy.url]
        : globalProxyCandidates.filter((candidate) => candidate !== proxy.url)
      await saveGlobalCandidates(next)
    } catch {
      // setGlobalProxyMutation already shows the toast.
    }
  }

  const toggleDirectFallback = async (checked: boolean) => {
    try {
      const next = checked
        ? [...globalProxyCandidates, 'direct']
        : globalProxyCandidates.filter((candidate) => candidate.toLowerCase() !== 'direct')
      await saveGlobalCandidates(next)
    } catch {
      // setGlobalProxyMutation already shows the toast.
    }
  }

  const handleSetProxyEnabled = async (proxy: ProxyPoolEntry, enabled: boolean) => {
    try {
      await setProxyEnabled(proxy.id, enabled)
      if (!enabled && globalProxyCandidateSet.has(proxy.url)) {
        await saveGlobalCandidates(globalProxyCandidates.filter((candidate) => candidate !== proxy.url))
      }
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } catch (err) {
      toast.error(`操作失败: ${extractErrorMessage(err)}`)
    }
  }

  const handleDeleteProxy = async (proxy: ProxyPoolEntry) => {
    try {
      await deleteProxy(proxy.id)
      if (globalProxyCandidateSet.has(proxy.url)) {
        await saveGlobalCandidates(globalProxyCandidates.filter((candidate) => candidate !== proxy.url))
      }
      setSelectedIds((prev) => {
        const next = new Set(prev)
        next.delete(proxy.id)
        return next
      })
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } catch (err) {
      toast.error(`删除失败: ${extractErrorMessage(err)}`)
    }
  }

  const handleBatchEnabled = async (enabled: boolean) => {
    if (selectedCount === 0) return
    setBatchAction(enabled ? 'enable' : 'disable')
    try {
      await Promise.all(selectedProxies.map((proxy) => setProxyEnabled(proxy.id, enabled)))
      if (!enabled) {
        const disabledUrls = new Set(selectedProxies.map((proxy) => proxy.url))
        await saveGlobalCandidates(
          globalProxyCandidates.filter((candidate) => !disabledUrls.has(candidate))
        )
      }
      toast.success(`已${enabled ? '启用' : '禁用'} ${selectedCount} 个代理`)
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } catch (err) {
      toast.error(`批量${enabled ? '启用' : '禁用'}失败: ${extractErrorMessage(err)}`)
    } finally {
      setBatchAction(null)
    }
  }

  const handleBatchGlobal = async (enabled: boolean) => {
    if (selectedCount === 0) return
    setBatchAction(enabled ? 'global' : 'unglobal')
    try {
      const selectedUrls = selectedProxies
        .filter((proxy) => enabled ? proxy.enabled : true)
        .map((proxy) => proxy.url)
      if (enabled && selectedUrls.length === 0) {
        toast.info('选中的代理都未启用，先启用后再设为全局')
        return
      }
      const selectedSet = new Set(selectedUrls)
      const next = enabled
        ? [...globalProxyCandidates, ...selectedUrls]
        : globalProxyCandidates.filter((candidate) => !selectedSet.has(candidate))
      await saveGlobalCandidates(next)
    } catch {
      // setGlobalProxyMutation already shows the toast.
    } finally {
      setBatchAction(null)
    }
  }

  const handleImportOrphanGlobalCandidates = async () => {
    if (orphanGlobalCandidates.length === 0) return
    setBatchAction('global')
    try {
      const res = await batchAddProxies({ urls: orphanGlobalCandidates })
      if (res.errors === 0) {
        toast.success(`已导入 ${res.added} 个旧全局代理到代理池`)
      } else {
        toast.info(`已导入 ${res.added} 个旧全局代理，跳过 ${res.errors} 个`)
      }
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } catch (err) {
      toast.error(`导入失败: ${extractErrorMessage(err)}`)
    } finally {
      setBatchAction(null)
    }
  }

  const handleCheckOne = async (proxy: ProxyPoolEntry) => {
    setCheckingIds((prev) => new Set(prev).add(proxy.id))
    try {
      const res = await checkProxy(proxy.id)
      if (res.health === 'healthy') {
        toast.success(`代理可用，延迟 ${res.latencyMs ?? '-'} ms`)
      } else {
        toast.error(res.autoDisabled ? '代理探测失败，已自动禁用' : '代理探测失败')
      }
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } catch (err) {
      toast.error(`探测失败: ${extractErrorMessage(err)}`)
    } finally {
      setCheckingIds((prev) => {
        const next = new Set(prev)
        next.delete(proxy.id)
        return next
      })
    }
  }

  const handleBatchCheck = async () => {
    const targets = selectedCount > 0 ? selectedProxies : proxies.filter((proxy) => proxy.enabled)
    if (targets.length === 0) return
    setBatchAction('check')
    setCheckingIds((prev) => {
      const next = new Set(prev)
      targets.forEach((proxy) => next.add(proxy.id))
      return next
    })
    try {
      const results = await Promise.allSettled(targets.map((proxy) => checkProxy(proxy.id)))
      const healthy = results.filter(
        (result) => result.status === 'fulfilled' && result.value.health === 'healthy'
      ).length
      const failed = results.length - healthy
      toast.success(`批量测试完成：可用 ${healthy}，异常 ${failed}`)
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    } finally {
      setCheckingIds((prev) => {
        const next = new Set(prev)
        targets.forEach((proxy) => next.delete(proxy.id))
        return next
      })
      setBatchAction(null)
    }
  }

  /** 池级告警。按严重度排：直连兜底会暴露服务器 IP，是唯一会连锁烧号的一项 */
  const preview = (list: ProxyPoolEntry[]) =>
    list
      .slice(0, 3)
      .map((proxy) => entryHost(proxy))
      .join('、') + (list.length > 3 ? ' 等' : '')

  const poolAlerts: PoolAlert[] = []
  if (directGlobalEnabled) {
    poolAlerts.push({
      key: 'direct',
      tone: 'danger',
      text: '已开启「直连兜底」：代理连不上时会用本机 IP 打上游',
      hint:
        '线上因此烧掉过一批号。服务器真实 IP 被上游记住后，从它出去过的号会接连被判死，' +
        '而封号会记在各自的代理头上，很难查到根因。除非你确实需要，否则关掉。',
      action: {
        label: '立即关闭',
        onClick: () => toggleDirectFallback(false),
        disabled: setGlobalProxyMutation.isPending,
      },
    })
  }
  if (quarantinedProxies.length > 0) {
    poolAlerts.push({
      key: 'quarantined',
      tone: 'danger',
      text: `${quarantinedProxies.length} 个出口因烧号被隔离停用：${preview(quarantinedProxies)}`,
      hint: '上面的号已改绑到干净出口。确认机场换过出口 IP 之后可手动重新启用。',
      action: { label: '隔离设置', onClick: () => setGuardOpen(true) },
    })
  }
  if (flaggedProxies.length > 0) {
    poolAlerts.push({
      key: 'flagged',
      tone: 'danger',
      text: `${flaggedProxies.length} 个出口被公开情报库标记为代理/VPN：${preview(flaggedProxies)}`,
      hint:
        '线上实测「被标记程度」直接决定账号寿命，这些出口优先淘汰。\n' +
        '注意判据是有没有被标记，不是机房还是家宽——干净的机房 IP 是可用的。',
    })
  }
  if (demotedProxies.length > 0) {
    poolAlerts.push({
      key: 'demoted',
      tone: 'warn',
      text: `${demotedProxies.length} 个出口已自动降权：${preview(demotedProxies)}`,
      hint:
        `封号率置信下界高于全池基线${
          poolBaselineRate != null ? ` ${Math.round(poolBaselineRate * 100)}%` : ''
        }。\n` +
        '干净出口用尽前不会轮到它们。降权只影响选择顺序，代理本身仍处于启用状态。',
      action: { label: '查看依据', onClick: () => setTab('bans') },
    })
  }
  if (orphanGlobalCandidates.length > 0) {
    poolAlerts.push({
      key: 'orphan',
      tone: 'warn',
      text: `${orphanGlobalCandidates.length} 个旧全局代理还不在代理池里`,
      hint: '它们仍会被当作全局候选使用，但没有健康检查、也不在封号统计里。建议移入池中统一管理。',
      action: {
        label: '移入代理池',
        onClick: handleImportOrphanGlobalCandidates,
        disabled: batchAction !== null,
      },
    })
  }
  if (uncheckedCount > 0 && proxies.length > 0) {
    poolAlerts.push({
      key: 'unchecked',
      tone: 'info',
      text: `${uncheckedCount} 个出口还没查过 IP 信誉`,
      hint: '未检测不等于干净。检测会给出 ASN、是否机房、是否已被公开标记为代理。',
      action: {
        label: '立即检测',
        onClick: () => reputationMutation.mutate(undefined),
        disabled: reputationMutation.isPending,
      },
    })
  }

  const renderHealthBadge = (proxy: ProxyPoolEntry) => {
    if (proxy.health === 'healthy') {
      return (
        <Badge variant="outline" className="text-xs gap-1 border-green-500/50 text-green-600 dark:text-green-400">
          <CheckCircle2 className="h-3 w-3" />
          {proxy.latencyMs != null ? `${proxy.latencyMs}ms` : '可用'}
        </Badge>
      )
    }
    if (proxy.health === 'unhealthy') {
      return (
        <Badge variant="outline" className="text-xs gap-1 border-destructive/50 text-destructive">
          <XCircle className="h-3 w-3" />
          异常{proxy.consecutiveFailures > 0 ? ` ×${proxy.consecutiveFailures}` : ''}
        </Badge>
      )
    }
    return (
      <Badge variant="outline" className="text-xs gap-1 text-muted-foreground">
        <HelpCircle className="h-3 w-3" />
        未检测
      </Badge>
    )
  }

  return (
    <>
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>代理 IP 池管理</DialogTitle>
        </DialogHeader>

        <div className="flex items-center gap-1 border-b -mt-1">
          <button
            type="button"
            onClick={() => setTab('pool')}
            className={`px-3 py-2 text-sm border-b-2 -mb-px transition-colors ${
              tab === 'pool'
                ? 'border-primary font-medium'
                : 'border-transparent text-muted-foreground hover:text-foreground'
            }`}
          >
            代理池
          </button>
          <button
            type="button"
            onClick={() => setTab('bans')}
            className={`px-3 py-2 text-sm border-b-2 -mb-px inline-flex items-center gap-1 transition-colors ${
              tab === 'bans'
                ? 'border-primary font-medium'
                : 'border-transparent text-muted-foreground hover:text-foreground'
            }`}
          >
            <Skull className="h-3.5 w-3.5" />
            封号统计
            {poolTotalBans > 0 && (
              <Badge variant="outline" className="h-4 px-1 text-[10px] border-destructive/60 text-destructive">
                {poolTotalBans}
              </Badge>
            )}
          </button>
        </div>

        {tab === 'bans' && (
          <div className="flex-1 overflow-y-auto py-2">
            <ProxyBanStatsPanel />
          </div>
        )}

        {tab === 'pool' && (
        <div className="flex-1 overflow-y-auto space-y-4 py-2">
          <div className="rounded-md border p-3 space-y-2">
            <div className="flex items-center justify-between gap-3">
              <div>
                <div className="text-sm font-medium">代理选择策略</div>
              </div>
              <Badge variant="secondary" className="shrink-0">
                {PROXY_BALANCING_LABEL[proxyBalancingData?.mode ?? 'sticky']}
              </Badge>
            </div>
            <div className="grid grid-cols-3 gap-2">
              {PROXY_MODE_OPTIONS.map((mode) => (
                <Button
                  key={mode}
                  type="button"
                  size="sm"
                  variant={(proxyBalancingData?.mode ?? 'sticky') === mode ? 'default' : 'outline'}
                  disabled={proxyBalancingLoading || setProxyBalancingMutation.isPending}
                  onClick={() => setProxyBalancingMutation.mutate(mode)}
                  title={
                    mode === 'sticky'
                      ? '账号成功命中代理后固定使用，失败后再换'
                      : mode === 'round_robin'
                        ? '按代理候选轮询分配'
                        : '优先选择当前请求数最少的代理'
                  }
                >
                  {PROXY_BALANCING_LABEL[mode]}
                </Button>
              ))}
            </div>
          </div>

          {/* 单条添加 */}
          {!showBatch && (
            <form onSubmit={handleAdd} className="flex gap-2">
              <Input
                placeholder="socks5://user:pass@host:port，或直接粘 host:端口:用户名:密码"
                title="不带协议时按 socks5 处理；要用别的协议请写完整 URL，或用批量导入选协议"
                value={newUrl}
                onChange={(e) => setNewUrl(e.target.value)}
                className="flex-1 font-mono text-sm"
              />
              <Input
                placeholder="备注（可选）"
                value={newLabel}
                onChange={(e) => setNewLabel(e.target.value)}
                className="w-32"
              />
              <Button type="submit" size="sm" disabled={addMutation.isPending || !newUrl.trim()}>
                <Plus className="h-4 w-4 mr-1" />
                添加
              </Button>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => setShowBatch(true)}
              >
                <Upload className="h-4 w-4 mr-1" />
                批量
              </Button>
            </form>
          )}

          {/* 批量导入 */}
          {showBatch && (
            <div className="space-y-2">
              <label className="text-sm font-medium">
                批量导入（每行一个，# 开头为注释）
              </label>
              <textarea
                placeholder={
                  '# 支持两种写法，可混用\n' +
                  '# 1) 代理商导出格式 host:端口:用户名:密码\n' +
                  '203.0.113.10:1080:user:pass\n' +
                  '# 2) 完整 URL\n' +
                  'socks5://user:pass@host:1080'
                }
                value={batchText}
                onChange={(e) => setBatchText(e.target.value)}
                className="flex min-h-[120px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm font-mono placeholder:text-muted-foreground focus-visible:outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/30"
              />
              <div className="flex flex-wrap items-center gap-2 rounded-md border px-2 py-1.5">
                <span className="text-xs text-muted-foreground">不带协议的行按</span>
                {(['socks5', 'http', 'socks4', 'https'] as ProxyScheme[]).map((scheme) => (
                  <button
                    key={scheme}
                    type="button"
                    onClick={() => setBatchScheme(scheme)}
                    className={
                      'h-6 rounded-full border px-2 font-mono text-[11px] transition-colors ' +
                      (batchScheme === scheme
                        ? 'border-primary bg-primary/10 font-medium text-primary'
                        : 'border-border text-muted-foreground hover:text-foreground')
                    }
                  >
                    {scheme}
                  </button>
                ))}
                <span
                  className="text-xs text-muted-foreground"
                  title="导出清单里不含协议，只能按这里选的补。选错会让出口在健康检查里一直失败，导入后记得点「批量测试」确认"
                >
                  导入。已写明协议的行不受影响
                </span>
              </div>
              <div className="flex gap-2">
                <Button
                  size="sm"
                  onClick={() => batchMutation.mutate()}
                  disabled={batchMutation.isPending || !batchText.trim()}
                >
                  导入
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => { setShowBatch(false); setBatchText(''); setBatchErrors([]) }}
                >
                  {batchMutation.isSuccess ? '关闭' : '取消'}
                </Button>
              </div>
              {/* 批量导入失败明细 */}
              {batchErrors.length > 0 && (
                <div className="text-xs text-muted-foreground space-y-1 max-h-24 overflow-y-auto border rounded-md p-2">
                  <div className="font-medium text-yellow-600 dark:text-yellow-400">跳过的条目：</div>
                  {batchErrors.map((msg, i) => (
                    <div key={i}>{msg}</div>
                  ))}
                </div>
              )}
            </div>
          )}

          {/* 代理列表 */}
          <div className="space-y-1">
            <div className="space-y-2">
              <div className="flex flex-col gap-2 sm:flex-row sm:items-center sm:justify-between">
                <div className="flex flex-wrap items-center gap-2 text-sm text-muted-foreground">
                  {(data?.total ?? 0) > 0 && (
                    <Checkbox
                      checked={allProxyCheckboxState}
                      onCheckedChange={(checked) => toggleAllSelected(checked === true)}
                      title={
                        allVisibleSelected
                          ? '取消全选'
                          : filterActive
                            ? `全选筛出的 ${visibleProxies.length} 个`
                            : '全选代理'
                      }
                    />
                  )}
                  <span>
                    {filterActive
                      ? `筛出 ${visibleProxies.length} / ${data?.total ?? 0} 个代理`
                      : `共 ${data?.total ?? 0} 个代理`}
                  </span>
                  <Badge variant="secondary" className="text-xs">
                    全局 {globalPoolCount}{directGlobalEnabled ? ' + 直连' : ''}
                  </Badge>
                  {poolBaselineRate != null && (
                    <Badge
                      variant="outline"
                      className="text-xs text-muted-foreground"
                      title={
                        '全池合计封号率 = 总封号 / 总绑定过的账号。\n' +
                        '判断某个出口是否真的更脏，要看它的封号率置信下界有没有超过这条基线；\n' +
                        '没超过就只是它服役期间赶上过全池清扫，不是它自己的问题。'
                      }
                    >
                      全池基线 {Math.round(poolBaselineRate * 100)}%
                    </Badge>
                  )}
                  {selectedCount > 0 && (
                    <Badge variant="outline" className="text-xs">
                      已选 {selectedCount}
                    </Badge>
                  )}
                </div>
                {(data?.total ?? 0) > 0 && (
                  <div className="flex flex-wrap items-center gap-1">
                    <label
                      className={
                        'inline-flex h-7 items-center gap-1 rounded-md border px-2 text-xs' +
                        (directGlobalEnabled
                          ? ' border-destructive/60 bg-destructive/10 text-destructive'
                          : '')
                      }
                      title="开启后，代理连不上时会改用本机 IP 直连上游，账号会因此暴露服务器真实 IP"
                    >
                      <Checkbox
                        checked={directGlobalEnabled}
                        onCheckedChange={(checked) => toggleDirectFallback(checked === true)}
                        disabled={setGlobalProxyMutation.isPending}
                      />
                      直连兜底
                    </label>
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={handleBatchCheck}
                      disabled={batchAction === 'check' || proxies.length === 0}
                      title={selectedCount > 0 ? '测试选中的代理' : '测试所有已启用代理'}
                    >
                      <Activity className="h-3 w-3 mr-1" />
                      {batchAction === 'check' ? '测试中...' : '批量测试'}
                    </Button>
                    {selectedCount > 0 && (
                      <>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs"
                          onClick={() => handleBatchEnabled(true)}
                          disabled={batchAction !== null}
                        >
                          启用
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs"
                          onClick={() => handleBatchEnabled(false)}
                          disabled={batchAction !== null}
                        >
                          禁用
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs"
                          onClick={() => handleBatchGlobal(true)}
                          disabled={batchAction !== null || setGlobalProxyMutation.isPending}
                        >
                          设为全局
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs"
                          onClick={() => handleBatchGlobal(false)}
                          disabled={batchAction !== null || setGlobalProxyMutation.isPending}
                        >
                          取消全局
                        </Button>
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs border-destructive/50 text-destructive hover:text-destructive"
                          onClick={() => handleBatchDelete(false)}
                          disabled={batchAction !== null}
                          title={
                            selectedInUseCount > 0
                              ? `其中 ${selectedInUseCount} 个仍有凭据绑定，会被跳过`
                              : '删除选中的代理'
                          }
                        >
                          <Trash2 className="h-3 w-3 mr-1" />
                          {batchAction === 'delete' ? '删除中...' : `删除 ${selectedCount}`}
                        </Button>
                        {selectedInUseCount > 0 && (
                          <Button
                            size="sm"
                            variant="ghost"
                            className="h-7 text-xs text-destructive hover:text-destructive"
                            onClick={() => handleBatchDelete(true)}
                            disabled={batchAction !== null}
                            title="连仍有凭据绑定的一起删。凭据不会被解绑，只是从此不再被监控与改绑"
                          >
                            强制删除
                          </Button>
                        )}
                      </>
                    )}
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={() => assignRoundRobinMutation.mutate()}
                      disabled={assignRoundRobinMutation.isPending}
                      title="给未分配独立IP的个人号各绑一个出口并启用；IP不够的个人号自动禁用。企业号不参与。"
                    >
                      <Shuffle className="h-3 w-3 mr-1" />
                      {assignRoundRobinMutation.isPending ? '分配中...' : '分配独立IP'}
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={() => reputationMutation.mutate(
                        selectedCount > 0 ? selectedProxies.map((p) => p.id) : undefined
                      )}
                      disabled={reputationMutation.isPending || proxies.length === 0}
                      title="查出口的 ASN / 是否机房 / 是否已被公开标记为代理。判据是有没有被标记，不是机房还是家宽"
                    >
                      <Fingerprint className="h-3 w-3 mr-1" />
                      {reputationMutation.isPending ? '检测中...' : '检测信誉'}
                    </Button>
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={() => setGuardOpen(true)}
                      title="窗口内封够几个号就停用该出口，并把幸存号迁走"
                    >
                      <ShieldAlert className="h-3 w-3 mr-1" />
                      烧号隔离
                    </Button>
                  </div>
                )}
              </div>
              {/* 筛选：挑出坏 IP 批量处理 */}
              {(data?.total ?? 0) > 0 && (
                <div className="space-y-1.5 rounded-md border p-2">
                  <div className="flex items-center gap-2">
                    <Search className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                    <Input
                      placeholder="按 IP / 端口 / 备注搜索"
                      value={search}
                      onChange={(e) => setSearch(e.target.value)}
                      className="h-7 flex-1 font-mono text-xs"
                    />
                    {filterActive && (
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 shrink-0 text-xs"
                        onClick={clearFilters}
                      >
                        清除筛选
                      </Button>
                    )}
                  </div>
                  <div className="flex flex-wrap gap-1">
                    {PROXY_FILTERS.map((filter) => {
                      const count = proxies.filter(filter.match).length
                      const on = activeFilters.has(filter.key)
                      return (
                        <button
                          key={filter.key}
                          type="button"
                          onClick={() => toggleFilter(filter.key)}
                          title={filter.hint}
                          className={
                            'inline-flex h-6 items-center gap-1 rounded-full border px-2 text-[11px] transition-colors ' +
                            (on
                              ? 'border-primary bg-primary/10 font-medium text-primary'
                              : 'border-border text-muted-foreground hover:text-foreground')
                          }
                        >
                          {filter.label}
                          <span className="tabular-nums opacity-70">{count}</span>
                        </button>
                      )
                    })}
                  </div>
                  {activeFilters.size > 1 && (
                    <p className="text-[11px] text-muted-foreground">
                      多个条件取交集。例如「烧过号」+「空闲」= 烧过号且当前没有凭据在用，
                      这批删掉不影响在跑的号。
                    </p>
                  )}
                </div>
              )}
              <PoolAlerts alerts={poolAlerts} />
            </div>

            {isLoading && (
              <div className="text-sm text-muted-foreground py-4 text-center">加载中...</div>
            )}

            {proxies.length === 0 && !isLoading && (
              <div className="text-sm text-muted-foreground py-4 text-center">
                暂无代理，请添加
              </div>
            )}

            {proxies.length > 0 && visibleProxies.length === 0 && !isLoading && (
              <div className="space-y-2 py-4 text-center text-sm text-muted-foreground">
                <div>没有符合筛选条件的代理</div>
                <Button size="sm" variant="outline" className="h-7 text-xs" onClick={clearFilters}>
                  清除筛选
                </Button>
              </div>
            )}

            <div className="border rounded-md divide-y max-h-[320px] overflow-y-auto">
              {visibleProxies.map((proxy: ProxyPoolEntry) => {
                const isGlobal = globalProxyCandidateSet.has(proxy.url)
                const isChecking = checkingIds.has(proxy.id)
                return (
                  <div key={proxy.id} className="flex items-center gap-3 p-3">
                    <Checkbox
                      checked={selectedIds.has(proxy.id)}
                      onCheckedChange={(checked) => toggleSelected(proxy.id, checked === true)}
                      title="选择此代理"
                    />
                    <div className="min-w-0 flex-1">
                      {/* 第一行只放「这是哪个 IP、能不能用、脏不脏」——扫视时唯一要看的。
                          备注、全局标记、检测时间这些属于查证信息，压到第二行。 */}
                      <div className="flex min-w-0 items-center gap-2">
                        <button
                          type="button"
                          className="min-w-0 flex-1 truncate text-left font-mono text-sm font-medium hover:text-primary"
                          title="点击复制 IP"
                          onClick={() => copyProxyHost(entryHost(proxy))}
                        >
                          {entryHost(proxy)}
                        </button>
                        {proxySchemeLabel(proxy.url) && (
                          <span className="shrink-0 text-[11px] uppercase text-muted-foreground/80">
                            {proxySchemeLabel(proxy.url)}
                          </span>
                        )}
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          className="h-7 w-7 shrink-0 p-0"
                          title="复制 IP"
                          onClick={() => copyProxyHost(entryHost(proxy))}
                        >
                          <Copy className="h-3.5 w-3.5" />
                        </Button>
                        {proxy.credentialCount > 0 && (
                          <span
                            className="inline-flex shrink-0 items-center gap-0.5 rounded bg-secondary px-1.5 text-xs tabular-nums text-muted-foreground"
                            title={`${proxy.credentialCount} 个凭据正绑在这个出口上。同出口的号会一起暴露，一个被标记容易连坐`}
                          >
                            <Users className="h-3 w-3" />
                            {proxy.credentialCount}
                          </span>
                        )}
                        {renderHealthBadge(proxy)}
                        <ProxyBanBadge stats={proxy.banStats} risk={proxy.risk} />
                      </div>
                      <div className="mt-1 flex flex-wrap items-center gap-1.5">
                        <ProxyReputationBadge
                          grade={proxy.reputationGrade}
                          reputation={proxy.reputation}
                        />
                        <ProxyWeightBadge risk={proxy.risk} />
                        {!proxy.enabled && (
                          <Badge
                            variant="outline"
                            className={
                              proxy.quarantinedAt
                                ? 'text-xs text-destructive border-destructive/50 gap-1'
                                : 'text-xs text-muted-foreground'
                            }
                            title={proxy.quarantineReason ?? undefined}
                          >
                            {proxy.quarantinedAt && <ShieldAlert className="h-3 w-3" />}
                            {proxy.quarantinedAt
                              ? '烧号隔离'
                              : proxy.autoDisabled
                                ? '自动禁用'
                                : '已禁用'}
                          </Badge>
                        )}
                        {isGlobal && (
                          <Badge variant="secondary" className="gap-1 text-xs">
                            <Globe className="h-3 w-3" />
                            全局
                          </Badge>
                        )}
                        {proxy.label && (
                          <Badge variant="secondary" className="text-xs">
                            {proxy.label}
                          </Badge>
                        )}
                        {proxy.banStats?.bans24h > 0 && (
                          <span
                            className="text-xs font-medium text-destructive"
                            title="最近 24 小时烧掉的号数。出口会换 IP，近期数据比累计更能说明它现在的状态"
                          >
                            24h 烧 {proxy.banStats.bans24h}
                          </span>
                        )}
                        {proxy.banStats?.medianSurvivalSecs != null && (
                          <span
                            className="text-xs text-muted-foreground"
                            title="被封账号的存活中位时长。越短说明这个 IP 越脏"
                          >
                            存活 {formatSurvival(proxy.banStats.medianSurvivalSecs)}
                          </span>
                        )}
                        {proxy.lastCheckedAt && (
                          <span
                            className="text-xs text-muted-foreground/70"
                            title={`上次连通性检测：${new Date(proxy.lastCheckedAt).toLocaleString()}`}
                          >
                            {new Date(proxy.lastCheckedAt).toLocaleDateString()}
                          </span>
                        )}
                      </div>
                      {/* 「为什么没判它有问题」只在确实烧过号时才说，且收进 title */}
                      {proxy.risk?.blockers?.[0] && proxy.banStats?.totalBans > 0 && (
                        <p
                          className="mt-1 truncate text-[11px] text-muted-foreground/80"
                          title={proxy.risk.blockers.join('\n')}
                        >
                          {proxy.risk.blockers[0]}
                        </p>
                      )}
                    </div>
                    <div className="flex items-center gap-1 shrink-0">
                      <label
                        className="inline-flex h-7 items-center gap-1 rounded-md border px-2 text-xs"
                        title={proxy.enabled || isGlobal ? '是否作为全局代理候选' : '启用代理后才能设为全局'}
                      >
                        <Checkbox
                          checked={isGlobal}
                          onCheckedChange={(checked) => toggleProxyGlobal(proxy, checked === true)}
                          disabled={setGlobalProxyMutation.isPending || (!proxy.enabled && !isGlobal)}
                        />
                        全局
                      </label>
                      <Button
                        size="sm"
                        variant="outline"
                        className="h-7 text-xs"
                        onClick={() => handleCheckOne(proxy)}
                        disabled={isChecking}
                        title="测试此代理连通性"
                      >
                        <Activity className="h-3 w-3 mr-1" />
                        {isChecking ? '测试中' : '测试'}
                      </Button>
                      {onSelectProxy && proxy.enabled && (
                        <Button
                          size="sm"
                          variant="outline"
                          className="h-7 text-xs"
                          onClick={() => {
                            onSelectProxy(proxy.url)
                            onOpenChange(false)
                          }}
                        >
                          选用
                        </Button>
                      )}
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 w-7 p-0"
                        onClick={() => handleSetProxyEnabled(proxy, !proxy.enabled)}
                        title={proxy.enabled ? '禁用此代理' : '启用此代理'}
                      >
                        {proxy.enabled ? (
                          <ToggleRight className="h-4 w-4 text-green-500" />
                        ) : (
                          <ToggleLeft className="h-4 w-4 text-muted-foreground" />
                        )}
                      </Button>
                      <Button
                        size="sm"
                        variant="ghost"
                        className="h-7 w-7 p-0 text-destructive hover:text-destructive"
                        onClick={() => handleDeleteProxy(proxy)}
                      >
                        <Trash2 className="h-4 w-4" />
                      </Button>
                    </div>
                  </div>
                )
              })}
            </div>
          </div>
        </div>
        )}
      </DialogContent>
    </Dialog>
    <ProxyGuardDialog open={guardOpen} onOpenChange={setGuardOpen} />
    </>
  )
}
