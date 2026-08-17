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
  deleteProxy,
  setProxyEnabled,
  getGlobalProxy,
  setGlobalProxy,
  getProxyBalancingMode,
  setProxyBalancingMode,
  PROXY_BALANCING_LABEL,
  checkProxy,
  assignProxiesRoundRobin,
  type ProxyBalancingMode,
} from '@/api/credentials'
import { extractErrorMessage, maskProxyUrl } from '@/lib/utils'
import {
  ProxyBanBadge,
  ProxyBanStatsPanel,
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
type BatchAction = 'check' | 'enable' | 'disable' | 'global' | 'unglobal' | null
type PoolTab = 'pool' | 'bans'

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
  const selectedProxies = proxies.filter((proxy) => selectedIds.has(proxy.id))
  const selectedCount = selectedProxies.length
  const allSelected = proxies.length > 0 && selectedCount === proxies.length
  let allProxyCheckboxState: boolean | 'indeterminate' = false
  if (allSelected) {
    allProxyCheckboxState = true
  } else if (selectedCount > 0) {
    allProxyCheckboxState = 'indeterminate'
  }
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
  const orphanGlobalCandidates = globalProxyCandidates.filter(
    (candidate) =>
      candidate.toLowerCase() !== 'direct' && !proxies.some((proxy) => proxy.url === candidate)
  )

  const addMutation = useMutation({
    mutationFn: () => addProxy({ url: newUrl.trim(), label: newLabel.trim() || undefined }),
    onSuccess: (entry) => {
      toast.success(`代理已添加：${entry.url}`)
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
      toast.success(`已用 ${res.proxyCount} 个代理轮询分配给 ${res.assigned} 个凭据`)
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

  const toggleAllSelected = (checked: boolean) => {
    setSelectedIds(checked ? new Set(proxies.map((proxy) => proxy.id)) : new Set())
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
                placeholder="代理 URL（如 socks5://user:pass@host:port）"
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
                批量导入（每行一个代理 URL，# 开头为注释）
              </label>
              <textarea
                placeholder={'# 每行一个代理 URL\nsocks5://user:pass@host1:1080\nsocks5://user:pass@host2:1080\nhttp://user:pass@host3:8080'}
                value={batchText}
                onChange={(e) => setBatchText(e.target.value)}
                className="flex min-h-[120px] w-full rounded-md border border-input bg-background px-3 py-2 text-sm font-mono placeholder:text-muted-foreground focus-visible:outline-none focus-visible:border-ring focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/30"
              />
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
                      title={allSelected ? '取消全选' : '全选代理'}
                    />
                  )}
                  <span>共 {data?.total ?? 0} 个代理</span>
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
                      </>
                    )}
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={() => assignRoundRobinMutation.mutate()}
                      disabled={assignRoundRobinMutation.isPending}
                      title="将可用代理轮询分配给所有凭据"
                    >
                      <Shuffle className="h-3 w-3 mr-1" />
                      轮询分配
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
              {directGlobalEnabled && (
                <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-destructive/60 bg-destructive/15 px-3 py-2 text-xs text-destructive">
                  <span>
                    已开启「直连兜底」：代理连不上时账号会改用<b>本机 IP</b> 直连上游。
                    线上因此烧掉过一批号——真实 IP 被上游记住后，从它出去过的号会接连被判死，
                    而封号会记在各自的代理头上，很难查。除非你确实需要，否则关掉。
                  </span>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 text-xs"
                    onClick={() => toggleDirectFallback(false)}
                    disabled={setGlobalProxyMutation.isPending}
                  >
                    立即关闭
                  </Button>
                </div>
              )}
              {quarantinedProxies.length > 0 && (
                <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-destructive/60 bg-destructive/15 px-3 py-2 text-xs text-destructive">
                  <span>
                    有 {quarantinedProxies.length} 个出口因烧号已被隔离停用（
                    {quarantinedProxies
                      .slice(0, 3)
                      .map((proxy) => maskProxyUrl(proxy.url))
                      .join('、')}
                    {quarantinedProxies.length > 3 ? ' 等' : ''}
                    ）。上面的号已改绑到干净出口；确认机场换过出口 IP 后可手动重新启用。
                  </span>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 text-xs"
                    onClick={() => setGuardOpen(true)}
                  >
                    隔离设置
                  </Button>
                </div>
              )}
              {demotedProxies.length > 0 && (
                <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
                  <span>
                    有 {demotedProxies.length} 个出口的封号率置信下界高于全池基线
                    {poolBaselineRate != null ? ` ${Math.round(poolBaselineRate * 100)}%` : ''}
                    ，已自动降权（
                    {demotedProxies
                      .slice(0, 3)
                      .map((proxy) => maskProxyUrl(proxy.url))
                      .join('、')}
                    {demotedProxies.length > 3 ? ' 等' : ''}
                    ），干净出口用尽前不会轮到它们。代理本身仍处于启用状态。
                  </span>
                  <Button
                    size="sm"
                    variant="outline"
                    className="h-7 text-xs"
                    onClick={() => setTab('bans')}
                  >
                    查看依据
                  </Button>
                </div>
              )}
              {orphanGlobalCandidates.length > 0 && (
                <div className="flex flex-wrap items-center justify-between gap-2 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-700 dark:text-yellow-300">
                  <span>有 {orphanGlobalCandidates.length} 个旧全局代理还不在代理池里。</span>
                  <div className="flex items-center gap-1">
                    <Button
                      size="sm"
                      variant="outline"
                      className="h-7 text-xs"
                      onClick={handleImportOrphanGlobalCandidates}
                      disabled={batchAction !== null}
                    >
                      移入代理池
                    </Button>
                    <Button
                      size="sm"
                      variant="ghost"
                      className="h-7 text-xs"
                      onClick={() => {
                        saveGlobalCandidates(
                          globalProxyCandidates.filter(
                            (candidate) => !orphanGlobalCandidates.includes(candidate)
                          )
                        ).catch(() => undefined)
                      }}
                      disabled={setGlobalProxyMutation.isPending}
                    >
                      移除旧项
                    </Button>
                  </div>
                </div>
              )}
            </div>

            {isLoading && (
              <div className="text-sm text-muted-foreground py-4 text-center">加载中...</div>
            )}

            {data?.proxies.length === 0 && !isLoading && (
              <div className="text-sm text-muted-foreground py-4 text-center">
                暂无代理，请添加
              </div>
            )}

            <div className="border rounded-md divide-y max-h-[320px] overflow-y-auto">
              {proxies.map((proxy: ProxyPoolEntry) => {
                const isGlobal = globalProxyCandidateSet.has(proxy.url)
                const isChecking = checkingIds.has(proxy.id)
                return (
                  <div key={proxy.id} className="flex items-center gap-3 p-3">
                    <Checkbox
                      checked={selectedIds.has(proxy.id)}
                      onCheckedChange={(checked) => toggleSelected(proxy.id, checked === true)}
                      title="选择此代理"
                    />
                    <div className="flex-1 min-w-0">
                      <div className="flex items-center gap-2 flex-wrap">
                        <span className="font-mono text-xs truncate">
                          {maskProxyUrl(proxy.url)}
                        </span>
                        {proxy.label && (
                          <Badge variant="secondary" className="text-xs">{proxy.label}</Badge>
                        )}
                        {isGlobal && (
                          <Badge variant="secondary" className="text-xs gap-1">
                            <Globe className="h-3 w-3" />
                            全局
                          </Badge>
                        )}
                        {renderHealthBadge(proxy)}
                        <ProxyBanBadge stats={proxy.banStats} risk={proxy.risk} />
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
                      </div>
                      <div className="flex items-center gap-3 mt-0.5 flex-wrap">
                        {proxy.credentialCount > 0 && (
                          <span className="text-xs text-muted-foreground">
                            {proxy.credentialCount} 个凭据使用中
                          </span>
                        )}
                        {proxy.banStats?.bans24h > 0 && (
                          <span className="text-xs text-destructive">
                            24h 内烧 {proxy.banStats.bans24h} 个
                          </span>
                        )}
                        {proxy.banStats?.medianSurvivalSecs != null && (
                          <span className="text-xs text-muted-foreground">
                            存活中位 {formatSurvival(proxy.banStats.medianSurvivalSecs)}
                          </span>
                        )}
                        {proxy.risk?.blockers?.[0] && proxy.banStats?.totalBans > 0 && (
                          <span
                            className="text-xs text-muted-foreground truncate max-w-[280px]"
                            title={proxy.risk.blockers.join('\n')}
                          >
                            {proxy.risk.blockers[0]}
                          </span>
                        )}
                        {proxy.lastCheckedAt && (
                          <span className="text-xs text-muted-foreground">
                            检测于 {new Date(proxy.lastCheckedAt).toLocaleString()}
                          </span>
                        )}
                      </div>
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
