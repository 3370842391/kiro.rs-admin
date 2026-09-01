import { useQuery } from '@tanstack/react-query'
import {
  Activity,
  AlertTriangle,
  CheckCircle2,
  ChevronDown,
  ChevronRight,
  Flame,
  Network,
  ShieldAlert,
} from 'lucide-react'
import { useState } from 'react'

import { getBanPostmortem, getPoolHealth } from '@/api/traces'
import { Badge } from '@/components/ui/badge'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { cn } from '@/lib/utils'
import type { BanWave, RiskFinding, RiskSeverity } from '@/types/api'

/**
 * 号池体检与封号复盘。
 *
 * 2026-08-31 那次成批封号，判断「会不会再来一次」用到的每个数都得 SSH 跑 SQL：
 * 429 占比、有多少分钟一个 429 都没有、52 个号挤在几个出口上。这里把它们摆出来，
 * 并直接给结论——面板上堆一排数字而不说「所以要做什么」，等于把分析工作又推回去。
 */

const SEVERITY_STYLE: Record<RiskSeverity, { row: string; Icon: typeof AlertTriangle }> = {
  critical: {
    row: 'border-destructive/50 bg-destructive/10 text-destructive',
    Icon: ShieldAlert,
  },
  warn: {
    row: 'border-amber-500/40 bg-amber-500/10 text-amber-700 dark:text-amber-300',
    Icon: AlertTriangle,
  },
}

function formatSecs(secs?: number): string {
  if (secs == null) return '未知'
  if (secs < 60) return `${secs} 秒`
  if (secs < 3600) return `${Math.round(secs / 60)} 分钟`
  return `${(secs / 3600).toFixed(1)} 小时`
}

function FindingRow({ finding }: { finding: RiskFinding }) {
  const { row, Icon } = SEVERITY_STYLE[finding.severity]
  return (
    <div className={cn('flex items-start gap-2 rounded-md border px-3 py-2', row)}>
      <Icon className="mt-0.5 h-4 w-4 shrink-0" />
      <div className="min-w-0 space-y-0.5">
        <div className="text-sm font-medium">{finding.title}</div>
        <p className="text-xs leading-relaxed opacity-90">{finding.detail}</p>
      </div>
    </div>
  )
}

function Metric({
  label,
  value,
  hint,
  tone,
}: {
  label: string
  value: string
  hint?: string
  tone?: 'bad' | 'good'
}) {
  return (
    <div className="min-w-0 rounded-md border p-2.5" title={hint}>
      <div className="truncate text-[11px] text-muted-foreground">{label}</div>
      <div
        className={cn(
          'mt-0.5 truncate text-lg font-semibold tabular-nums',
          tone === 'bad' && 'text-destructive',
          tone === 'good' && 'text-emerald-600 dark:text-emerald-400',
        )}
      >
        {value}
      </div>
    </div>
  )
}

function HealthTab({ windowMinutes }: { windowMinutes: number }) {
  const { data, isLoading } = useQuery({
    queryKey: ['pool-health', windowMinutes],
    queryFn: () => getPoolHealth(windowMinutes),
    refetchInterval: 60_000,
  })

  if (isLoading) {
    return <div className="py-8 text-center text-sm text-muted-foreground">加载中...</div>
  }
  if (!data) return null

  const { rateLimit: rl, exits } = data
  // 关键不在 429 总数，而在有没有喘息的时刻。线上成批掉号前是 0/181
  const quietTone = rl.minutesWithTraffic === 0 ? undefined : rl.quietMinutes === 0 ? 'bad' : 'good'

  return (
    <div className="space-y-4">
      {data.findings.length === 0 ? (
        <div className="flex items-center gap-2 rounded-md border border-emerald-500/40 bg-emerald-500/10 px-3 py-2 text-sm text-emerald-700 dark:text-emerald-300">
          <CheckCircle2 className="h-4 w-4 shrink-0" />
          未发现会导致成批掉号的风险。
        </div>
      ) : (
        <div className="space-y-2">
          {data.findings.map((finding) => (
            <FindingRow key={finding.code} finding={finding} />
          ))}
        </div>
      )}

      <div>
        <div className="mb-1.5 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <Activity className="h-3.5 w-3.5" />
          限流形态（近 {rl.windowMinutes} 分钟）
        </div>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Metric
            label="429 占比"
            value={`${rl.rateLimitedPct.toFixed(1)}%`}
            tone={rl.rateLimitedPct >= 5 ? 'bad' : undefined}
            hint={`${rl.attempts} 跳里有 ${rl.rateLimited} 个 429。5% 以上说明投递速率已高于上游天花板`}
          />
          <Metric
            label="安静分钟"
            value={
              rl.minutesWithTraffic === 0
                ? '无流量'
                : `${rl.quietMinutes}/${rl.minutesWithTraffic}`
            }
            tone={quietTone}
            hint={
              '一个 429 都没有的分钟数。关键不在 429 总数，而在有没有喘息的时刻——' +
              '一分钟都不断，在上游看来就是「已知超限仍持续投递」。'
            }
          />
          <Metric label="成功跳数" value={String(rl.success)} />
          <Metric
            label="成功/分钟"
            value={
              rl.minutesWithTraffic > 0
                ? (rl.success / rl.minutesWithTraffic).toFixed(1)
                : '-'
            }
          />
        </div>
      </div>

      <div>
        <div className="mb-1.5 flex items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <Network className="h-3.5 w-3.5" />
          出口集中度
        </div>
        <div className="grid grid-cols-2 gap-2 sm:grid-cols-4">
          <Metric label="启用账号" value={String(exits.accounts)} />
          <Metric label="使用中的出口" value={String(exits.exits)} />
          <Metric
            label="平均每出口"
            value={`${exits.avgAccountsPerExit.toFixed(1)} 个号`}
            tone={exits.avgAccountsPerExit > 3 ? 'bad' : undefined}
            hint="同出口的号会一起暴露，一个被标记其余容易连坐。目标是不超过 3 个"
          />
          <Metric
            label="直连账号"
            value={String(exits.directAccounts)}
            tone={exits.directAccounts > 0 ? 'bad' : 'good'}
            hint="走直连的号暴露的是服务器本机 IP"
          />
        </div>

        {exits.crowded.length > 0 && (
          <div className="mt-2 divide-y overflow-hidden rounded-md border">
            {exits.crowded.map((exit) => (
              <div
                key={exit.exit}
                className="flex items-center gap-2 px-2.5 py-1.5 text-xs"
              >
                <span className="min-w-0 flex-1 truncate font-mono">{exit.exit}</span>
                {exit.burned > 0 && (
                  <span
                    className="inline-flex shrink-0 items-center gap-0.5 text-destructive"
                    title={`这个出口历史上烧过 ${exit.burned} 个号`}
                  >
                    <Flame className="h-3 w-3" />
                    {exit.burned}
                  </span>
                )}
                <Badge variant="outline" className="shrink-0 text-[11px]">
                  {exit.accounts} 个号
                </Badge>
              </div>
            ))}
          </div>
        )}
      </div>
    </div>
  )
}

function WaveCard({ wave, defaultOpen }: { wave: BanWave; defaultOpen: boolean }) {
  const [open, setOpen] = useState(defaultOpen)
  const started = new Date(wave.startedAt)

  return (
    <div className="overflow-hidden rounded-md border">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        aria-expanded={open}
        className="flex w-full items-center gap-2 px-3 py-2.5 text-left transition-colors hover:bg-muted/40"
      >
        {open ? (
          <ChevronDown className="h-4 w-4 shrink-0 text-muted-foreground" />
        ) : (
          <ChevronRight className="h-4 w-4 shrink-0 text-muted-foreground" />
        )}
        <span className="shrink-0 text-sm tabular-nums">
          {started.toLocaleString()}
        </span>
        <span
          className={cn(
            'inline-flex shrink-0 items-center gap-1 text-base font-semibold tabular-nums',
            wave.looksLikeSweep ? 'text-destructive' : 'text-foreground',
          )}
        >
          <Flame className="h-4 w-4" />
          {wave.bans}
        </span>
        <span className="min-w-0 flex-1 truncate text-xs text-muted-foreground">
          {Math.max(Math.round(wave.spanSecs / 60), 1)} 分钟内 · {wave.distinctExits} 个出口
        </span>
        {wave.looksLikeSweep && (
          <Badge
            variant="outline"
            className="shrink-0 border-destructive/60 text-[11px] text-destructive"
          >
            疑似清扫
          </Badge>
        )}
      </button>

      {open && (
        <div className="space-y-3 border-t bg-muted/30 px-3 py-3">
          <p
            className={cn(
              'rounded-md border px-2.5 py-2 text-xs leading-relaxed',
              wave.looksLikeSweep
                ? 'border-destructive/40 bg-destructive/10 text-destructive'
                : 'border-border bg-background/60 text-muted-foreground',
            )}
          >
            {wave.verdict}
          </p>

          <div className="grid grid-cols-2 gap-x-4 gap-y-1.5 text-xs sm:grid-cols-4">
            <div>
              <div className="text-[11px] text-muted-foreground">存活最短</div>
              <div className="font-medium tabular-nums">
                {formatSecs(wave.survivalMinSecs)}
              </div>
            </div>
            <div>
              <div className="text-[11px] text-muted-foreground">存活最长</div>
              <div className="font-medium tabular-nums">
                {formatSecs(wave.survivalMaxSecs)}
              </div>
            </div>
            <div>
              <div className="text-[11px] text-muted-foreground">涉及出口</div>
              <div className="font-medium tabular-nums">{wave.distinctExits}</div>
            </div>
            <div>
              <div className="text-[11px] text-muted-foreground">持续</div>
              <div className="font-medium tabular-nums">{formatSecs(wave.spanSecs)}</div>
            </div>
          </div>

          <div>
            <div className="mb-1 text-xs font-medium">按出口分布</div>
            <div className="flex flex-wrap gap-1">
              {wave.byExit.map((exit) => (
                <span
                  key={exit.exit}
                  className="inline-flex items-center gap-1 rounded bg-secondary px-1.5 py-0.5 font-mono text-[11px]"
                >
                  {exit.exit}
                  <span className="font-semibold tabular-nums">{exit.bans}</span>
                </span>
              ))}
            </div>
          </div>

          <div>
            <div className="mb-1 text-xs font-medium">
              逐条明细
              <span className="ml-1 font-normal text-muted-foreground">
                死前请求量接近 0 = 出口脏；很大 = 号是被打死的
              </span>
            </div>
            <div className="max-h-56 space-y-1 overflow-y-auto">
              {wave.events.map((event) => (
                <div
                  key={`${event.credentialId}-${event.bannedAt}`}
                  className="flex flex-wrap items-baseline gap-2 text-[11px]"
                >
                  <span className="font-mono text-muted-foreground">
                    #{event.credentialId}
                  </span>
                  <span className="font-mono">{event.exit}</span>
                  <span className="text-muted-foreground">
                    {new Date(event.bannedAt).toLocaleTimeString()}
                  </span>
                  {event.survivalSecs != null && (
                    <span className="tabular-nums">存活 {formatSecs(event.survivalSecs)}</span>
                  )}
                  {event.successesBeforeBan != null && (
                    <span
                      className={cn(
                        'tabular-nums',
                        event.successesBeforeBan < 20
                          ? 'font-medium text-destructive'
                          : 'text-muted-foreground',
                      )}
                      title={
                        event.successesBeforeBan < 20
                          ? '死前几乎没发请求，说明出口已被上游标记'
                          : '死前打了不少请求，更像是被打死的'
                      }
                    >
                      成功 {event.successesBeforeBan}
                    </span>
                  )}
                </div>
              ))}
            </div>
          </div>
        </div>
      )}
    </div>
  )
}

function PostmortemTab() {
  const { data, isLoading } = useQuery({
    queryKey: ['ban-postmortem'],
    queryFn: () => getBanPostmortem(200),
  })

  if (isLoading) {
    return <div className="py-8 text-center text-sm text-muted-foreground">加载中...</div>
  }
  if (!data || data.waves.length === 0) {
    return (
      <div className="py-8 text-center text-sm text-muted-foreground">
        暂无封号记录。
        <div className="mt-1 text-xs">账号被上游判死时会自动记入台账，删号也不会丢。</div>
      </div>
    )
  }

  return (
    <div className="space-y-2">
      <p className="text-xs leading-relaxed text-muted-foreground">
        按时间把封号切成一波一波。判据是<span className="font-medium text-foreground">同时性</span>
        而不是数量：跨多个出口、且存活时长差异大，说明这批号不是各自到寿命，
        而是被同一个墙钟事件一起收掉的——这种情况换代理没用。
      </p>
      {data.waves.map((wave, index) => (
        <WaveCard key={wave.startedAt} wave={wave} defaultOpen={index === 0} />
      ))}
    </div>
  )
}

export function PoolHealthDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const [tab, setTab] = useState<'health' | 'postmortem'>('health')
  const [windowMinutes, setWindowMinutes] = useState(60)

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[85vh] flex-col sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle>号池体检</DialogTitle>
        </DialogHeader>

        <div className="-mt-1 flex items-center gap-1 border-b">
          <button
            type="button"
            onClick={() => setTab('health')}
            className={cn(
              'border-b-2 px-3 py-2 text-sm transition-colors -mb-px',
              tab === 'health'
                ? 'border-primary font-medium'
                : 'border-transparent text-muted-foreground hover:text-foreground',
            )}
          >
            风险体检
          </button>
          <button
            type="button"
            onClick={() => setTab('postmortem')}
            className={cn(
              'border-b-2 px-3 py-2 text-sm transition-colors -mb-px',
              tab === 'postmortem'
                ? 'border-primary font-medium'
                : 'border-transparent text-muted-foreground hover:text-foreground',
            )}
          >
            封号复盘
          </button>
          {tab === 'health' && (
            <div className="ml-auto flex items-center gap-1 pb-1">
              {[15, 60, 360].map((minutes) => (
                <button
                  key={minutes}
                  type="button"
                  onClick={() => setWindowMinutes(minutes)}
                  className={cn(
                    'h-6 rounded-full border px-2 text-[11px] transition-colors',
                    windowMinutes === minutes
                      ? 'border-primary bg-primary/10 font-medium text-primary'
                      : 'border-border text-muted-foreground hover:text-foreground',
                  )}
                >
                  {minutes >= 60 ? `${minutes / 60}h` : `${minutes}m`}
                </button>
              ))}
            </div>
          )}
        </div>

        <div className="flex-1 overflow-y-auto py-2">
          {tab === 'health' ? <HealthTab windowMinutes={windowMinutes} /> : <PostmortemTab />}
        </div>
      </DialogContent>
    </Dialog>
  )
}
