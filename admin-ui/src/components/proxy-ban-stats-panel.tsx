import { useState } from 'react'
import { toast } from 'sonner'
import {
  AlertTriangle,
  ChevronDown,
  ChevronRight,
  Flame,
  Info,
  RotateCcw,
  ShieldAlert,
  Skull,
  TrendingDown,
} from 'lucide-react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { getProxyBanStats, resetProxyBanStats } from '@/api/credentials'
import { extractErrorMessage, cn } from '@/lib/utils'
import { maskEmailAddress } from '@/lib/utils'
import type {
  ProxyBanDetailEntry,
  ProxyBanSummary,
  ProxyRiskAssessment,
  ProxyRiskLevel,
  ProxySelectionTier,
} from '@/types/api'

/** 存活时长：被封账号从加入到判死之间的秒数，短到分钟级就说明这个出口 IP 已经被上游盯上了 */
export function formatSurvival(secs: number | undefined | null): string {
  if (secs == null) return '-'
  if (secs < 60) return `${secs} 秒`
  if (secs < 3600) return `${Math.round(secs / 60)} 分钟`
  if (secs < 86400) return `${(secs / 3600).toFixed(1)} 小时`
  return `${(secs / 86400).toFixed(1)} 天`
}

/** 死前成功请求数：接近 0 说明出口已被上游标记，很大说明号是被打死的 */
export function formatSuccesses(n: number | undefined | null): string {
  if (n == null) return '-'
  if (n >= 10000) return `${(n / 1000).toFixed(1)}k`
  return String(n)
}

const LEVEL_CLASS: Record<ProxyRiskLevel, string> = {
  ok: 'text-muted-foreground',
  watch: 'border-yellow-500/50 text-yellow-600 dark:text-yellow-400',
  suspect: 'border-orange-500/60 text-orange-600 dark:text-orange-400',
  quarantineRecommended: 'border-destructive/60 text-destructive',
}

const LEVEL_LABEL: Record<ProxyRiskLevel, string> = {
  ok: '正常',
  watch: '观察中',
  suspect: '存疑',
  quarantineRecommended: '建议隔离',
}

export function riskLevelOf(risk: ProxyRiskAssessment | undefined): ProxyRiskLevel {
  return risk?.level ?? 'ok'
}

const TIER_LABEL: Record<ProxySelectionTier, string> = {
  normal: '正常轮换',
  degraded: '降权',
  penalized: '基本不选',
}

const TIER_CLASS: Record<ProxySelectionTier, string> = {
  normal: '',
  degraded: 'border-orange-500/60 text-orange-600 dark:text-orange-400',
  penalized: 'border-destructive/60 text-destructive',
}

/** 选择权重徽章。normal 档不显示，避免给干净的出口平添噪音 */
export function ProxyWeightBadge({ risk }: { risk: ProxyRiskAssessment | undefined }) {
  const tier = risk?.selectionTier ?? 'normal'
  if (!risk || tier === 'normal') return null
  return (
    <Badge
      variant="outline"
      className={cn('text-xs gap-1', TIER_CLASS[tier])}
      title={`选择权重 ${Math.round(risk.selectionWeight * 100)}%。权重相对池内封号率中位数计算；${
        tier === 'penalized'
          ? '该出口只在其他出口全部用尽时才会兜底，另保留 5% 探测流量供其恢复。'
          : '正常档出口用尽后才会轮到它。'
      }`}
    >
      <TrendingDown className="h-3 w-3" />
      {TIER_LABEL[tier]} {Math.round(risk.selectionWeight * 100)}%
    </Badge>
  )
}

/** 挂在代理池每一行上的紧凑封号徽章。
 *
 * 只有**统计上高于全池基线**的出口才用醒目配色。没超基线的一律灰显并在提示里说明
 * 「这些封号只是服役期间赶上过全池清扫」——否则一排红色的「烧号 4 个 · 44%」会让
 * 运营分不清哪个出口是真有问题，把精力耗在无辜的出口上。
 */
export function ProxyBanBadge({
  stats,
  risk,
}: {
  stats: ProxyBanSummary | undefined
  risk?: ProxyRiskAssessment
}) {
  if (!stats || stats.totalBans === 0) return null
  const noisy = risk != null && !risk.abovePoolBaseline
  const level = noisy ? 'ok' : riskLevelOf(risk)
  const rate = stats.banRate != null ? `${Math.round(stats.banRate * 100)}%` : null
  const baseline = risk != null ? `${Math.round(risk.pooledBanRate * 100)}%` : null
  return (
    <Badge
      variant="outline"
      className={cn('text-xs gap-1', LEVEL_CLASS[level], noisy && 'opacity-70')}
      title={[
        `历史封号 ${stats.totalBans} 个 / 曾绑定 ${stats.accountsSeen} 个`,
        rate ? `原始封号率 ${rate}` : null,
        risk ? `置信下界 ${Math.round(risk.banRateLowerBound * 100)}%` : null,
        baseline ? `全池基线 ${baseline}` : null,
        noisy
          ? '结论：未超全池基线，属清扫噪声，不能归咎于这个出口'
          : '结论：统计上高于全池基线，这个出口确实更容易烧号',
        stats.medianSurvivalSecs != null
          ? `存活中位数 ${formatSurvival(stats.medianSurvivalSecs)}`
          : null,
        stats.medianSuccessesBeforeBan != null
          ? `死前成功请求中位数 ${formatSuccesses(stats.medianSuccessesBeforeBan)}`
          : null,
        '',
        ...(risk?.reasons ?? []).map((r) => `· ${r}`),
        ...(risk?.blockers ?? []).map((b) => `× ${b}`),
      ]
        .filter((line) => line !== null)
        .join('\n')}
    >
      <Skull className="h-3 w-3" />
      烧号 {stats.totalBans}
      {rate ? ` · ${rate}` : ''}
      {noisy && baseline ? `（基线 ${baseline}）` : ''}
      {level === 'quarantineRecommended' ? ' · 建议隔离' : ''}
    </Badge>
  )
}

/** 判定结论 + 证据/阻断原因。建议模式的核心展示：为什么下结论，或为什么没下 */
function RiskVerdict({ risk }: { risk: ProxyRiskAssessment }) {
  if (risk.reasons.length === 0 && risk.blockers.length === 0) return null
  return (
    <div className="space-y-1">
      {risk.reasons.map((reason) => (
        <div key={reason} className="flex items-start gap-1.5 text-xs">
          <ShieldAlert className="h-3.5 w-3.5 shrink-0 mt-px text-orange-500" />
          <span>{reason}</span>
        </div>
      ))}
      {risk.blockers.map((blocker) => (
        <div key={blocker} className="flex items-start gap-1.5 text-xs text-muted-foreground">
          <Info className="h-3.5 w-3.5 shrink-0 mt-px" />
          <span>{blocker}</span>
        </div>
      ))}
    </div>
  )
}

function BanRow({ entry }: { entry: ProxyBanDetailEntry }) {
  const [expanded, setExpanded] = useState(false)
  const queryClient = useQueryClient()

  const resetMutation = useMutation({
    mutationFn: () => resetProxyBanStats(entry.proxyKey),
    onSuccess: () => {
      toast.success(`已清空 ${entry.proxyKey} 的封号台账`)
      queryClient.invalidateQueries({ queryKey: ['proxy-ban-stats'] })
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
    },
    onError: (err) => toast.error(`清空失败: ${extractErrorMessage(err)}`),
  })

  // 未超全池基线的出口一律按正常显示：它的封号是全池清扫摊到的，不是它的问题。
  // 之前统一用 riskLevelOf 上色，结果一屏红色徽章里分不出哪个才该动手。
  const noisy = !entry.risk.abovePoolBaseline
  const level = noisy ? 'ok' : riskLevelOf(entry.risk)

  return (
    <div className="text-sm">
      <div className="flex items-start gap-2 p-3">
        <button
          type="button"
          className="shrink-0 text-muted-foreground hover:text-foreground mt-0.5"
          onClick={() => setExpanded((v) => !v)}
          aria-label={expanded ? '收起封号明细' : '展开封号明细'}
        >
          {expanded ? <ChevronDown className="h-4 w-4" /> : <ChevronRight className="h-4 w-4" />}
        </button>
        <div className="flex-1 min-w-0 space-y-1">
          <div className="flex items-center gap-2 flex-wrap">
            <span className="font-mono text-xs truncate">{entry.proxyKey}</span>
            {!entry.inPool && (
              <Badge variant="outline" className="text-xs text-muted-foreground">
                已移出代理池
              </Badge>
            )}
            <Badge variant="outline" className={cn('text-xs gap-1', LEVEL_CLASS[level])}>
              <Flame className="h-3 w-3" />
              {entry.totalBans} 个号
            </Badge>
            <Badge variant="outline" className={cn('text-xs', LEVEL_CLASS[level])}>
              {noisy ? '清扫噪声' : LEVEL_LABEL[level]}
            </Badge>
            <ProxyWeightBadge risk={entry.risk} />
            {entry.banRate != null && (
              <span className="text-xs text-muted-foreground">
                {entry.totalBans}/{entry.accountsSeen} = {Math.round(entry.banRate * 100)}%
                <span className="ml-1">
                  （下界 {Math.round(entry.risk.banRateLowerBound * 100)}% vs 基线{' '}
                  {Math.round(entry.risk.pooledBanRate * 100)}%）
                </span>
              </span>
            )}
          </div>
          <div className="flex items-center gap-3 text-xs text-muted-foreground flex-wrap">
            {entry.bans24h > 0 && <span className="text-destructive">24h 内 {entry.bans24h}</span>}
            {entry.bans7d > 0 && <span>7 天内 {entry.bans7d}</span>}
            {entry.medianSurvivalSecs != null && (
              <span>存活中位 {formatSurvival(entry.medianSurvivalSecs)}</span>
            )}
            {entry.medianSuccessesBeforeBan != null && (
              <span>死前成功请求中位 {formatSuccesses(entry.medianSuccessesBeforeBan)}</span>
            )}
            <span>{entry.distinctBatchDays} 个批次</span>
            {entry.lastBanAt && <span>最近 {new Date(entry.lastBanAt).toLocaleString()}</span>}
          </div>
          <RiskVerdict risk={entry.risk} />
        </div>
        <Button
          size="sm"
          variant="ghost"
          className="h-7 text-xs shrink-0"
          onClick={() => resetMutation.mutate()}
          disabled={resetMutation.isPending}
          title="机场换了出口 IP 后清零，重新开始计数"
        >
          <RotateCcw className="h-3 w-3 mr-1" />
          清零
        </Button>
      </div>

      {expanded && (
        <div className="border-t bg-muted/30 px-3 py-2 space-y-1 max-h-64 overflow-y-auto">
          {entry.events.length === 0 && (
            <div className="text-xs text-muted-foreground py-1">无明细记录</div>
          )}
          {entry.events.map((event) => (
            <div
              key={`${event.credentialId}-${event.bannedAt}`}
              className="flex items-baseline gap-2 text-xs flex-wrap"
            >
              <span className="font-mono text-muted-foreground">#{event.credentialId}</span>
              <span className="truncate max-w-[200px]">
                {event.email ? maskEmailAddress(event.email) : '-'}
              </span>
              <span className="text-muted-foreground">
                {new Date(event.bannedAt).toLocaleString()}
              </span>
              {event.survivalSecs != null && (
                <Badge variant="outline" className="text-[10px] h-4 px-1">
                  存活 {formatSurvival(event.survivalSecs)}
                </Badge>
              )}
              {event.successesBeforeBan != null && (
                <Badge
                  variant="outline"
                  className="text-[10px] h-4 px-1"
                  title="死前成功请求数。接近 0 说明出口已被上游标记；很大说明号是被打死的"
                >
                  成功 {formatSuccesses(event.successesBeforeBan)}
                </Badge>
              )}
            </div>
          ))}
        </div>
      )}
    </div>
  )
}

export function ProxyBanStatsPanel() {
  const { data, isLoading } = useQuery({
    queryKey: ['proxy-ban-stats'],
    queryFn: () => getProxyBanStats(200),
  })

  const proxies = data?.proxies ?? []
  const withBans = proxies.filter((p) => p.totalBans > 0)
  const recommended = withBans.filter((p) => p.risk.recommendQuarantine)
  const suspects = withBans.filter(
    (p) => !p.risk.recommendQuarantine && p.risk.level === 'suspect'
  )

  if (isLoading) {
    return <div className="text-sm text-muted-foreground py-4 text-center">加载中...</div>
  }

  if (withBans.length === 0) {
    return (
      <div className="text-sm text-muted-foreground py-6 text-center">
        暂无封号记录。
        <div className="text-xs mt-1">
          账号被上游判死（403 封号）时会自动记入台账，删号也不会丢。
        </div>
      </div>
    )
  }

  return (
    <div className="space-y-3">
      <div className="rounded-md border p-3 space-y-2">
        <div className="flex items-center justify-between gap-3">
          <div className="text-sm font-medium">封号台账</div>
          <Badge variant="secondary" className="shrink-0">
            累计 {data?.totalBans ?? 0} 个号
          </Badge>
        </div>
        <p className="text-xs text-muted-foreground">
          按出口 <span className="font-mono">host:port</span> 归档，与账号生命周期解耦：死号被保留期清理掉之后，这里的历史计数依然保留。
          烧号明显高于池内中位数的出口会被<span className="font-medium">自动降权</span>，
          干净出口用尽前不会轮到它们；全池封号率一致时不降任何人（那说明根因在请求打法）。
          降权只影响选择顺序，<span className="font-medium">不会禁用代理</span>。
        </p>
        {recommended.length > 0 && (
          <div className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
            <AlertTriangle className="h-4 w-4 shrink-0 mt-0.5" />
            <span>
              建议隔离{' '}
              <span className="font-mono">
                {recommended.map((p) => p.proxyKey).join('、')}
              </span>
              ：各项检验均通过，证据支持问题出在这些出口本身。展开可看判定依据。
            </span>
          </div>
        )}
        {recommended.length === 0 && suspects.length > 0 && (
          <div className="flex items-start gap-2 rounded-md border border-orange-500/40 bg-orange-500/10 px-3 py-2 text-xs text-orange-700 dark:text-orange-300">
            <Info className="h-4 w-4 shrink-0 mt-0.5" />
            <span>
              有 {suspects.length} 个出口封号率偏高，但证据不足以归咎于它们（样本太小、
              全是同一批号，或全池封号率普遍都高）。展开看每一条的具体阻断原因——
              如果原因是「全池普遍偏高」，那根因在请求打法，换 IP 无用。
            </span>
          </div>
        )}
      </div>

      <div className="border rounded-md divide-y max-h-[360px] overflow-y-auto">
        {withBans.map((entry) => (
          <BanRow key={entry.proxyKey} entry={entry} />
        ))}
      </div>
    </div>
  )
}
