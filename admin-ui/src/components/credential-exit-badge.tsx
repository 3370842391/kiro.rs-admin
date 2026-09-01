import { Activity, Globe, Network, ShieldAlert, Users } from 'lucide-react'
import { cn, maskProxyUrl } from '@/lib/utils'
import type { RecentActivity } from '@/types/api'

/**
 * 出口与近期活跃度徽章。
 *
 * 为什么把「出口 IP」「共用账号数」「近 1h 成功/429」摆在一起：排查封号时这三个
 * 数要放在同一视线里才有意义。2026-08-31 那次，同一批新号里绑到脏出口的 22 分钟
 * 就死、绑到零封号出口的活了下来——光看账号本身的失败计数完全看不出这个差别。
 */

/** 从代理 URL 取出 `host:port`，丢掉 scheme 与认证信息 */
export function proxyExitHost(url: string | null | undefined): string | null {
  if (!url) return null
  const raw = url.trim()
  if (!raw) return null
  if (raw.toLowerCase() === 'direct') return 'direct'
  // 多候选时只认第一个：号池里一号一出口，多候选是例外
  const first = raw.split(/[,;\s]+/).find(Boolean)
  if (!first) return null
  if (first.toLowerCase() === 'direct') return 'direct'
  // 分步而不是一条正则：认证信息里可能带 @（密码含 @ 是允许的），
  // 用 `[^@]*@` 只会切到第一个 @，host 就变成密码的一截了。取最后一个 @ 之后。
  const withoutScheme = first.replace(/^\w+:\/\//, '')
  const afterAuth = withoutScheme.slice(withoutScheme.lastIndexOf('@') + 1)
  const host = afterAuth.split('/')[0].trim()
  return host ? host.toLowerCase() : null
}

/**
 * 统计每个出口挂了多少个**启用中**的账号。
 *
 * 只数启用的：判死的号还留在列表里（保留期内），把它们算进「正在使用」会让
 * 一个早就没人用的出口显示成热门。
 */
export function countExitUsage(
  credentials: { proxyUrl?: string | null; disabled?: boolean }[],
): Map<string, number> {
  const out = new Map<string, number>()
  for (const credential of credentials) {
    if (credential.disabled) continue
    const host = proxyExitHost(credential.proxyUrl)
    if (!host) continue
    out.set(host, (out.get(host) ?? 0) + 1)
  }
  return out
}

interface ExitBadgeProps {
  proxyUrl?: string | null
  /** 同一出口上启用中的账号数（含自己） */
  peers?: number
  /** 该出口历史累计烧号数；未知时不显示 */
  burnedAccounts?: number
  className?: string
}

/** 出口 IP 徽章：直接显示 host:port，悬停给完整信息 */
export function CredentialExitBadge({
  proxyUrl,
  peers,
  burnedAccounts,
  className,
}: ExitBadgeProps) {
  const host = proxyExitHost(proxyUrl)
  const direct = host === 'direct' || !host

  const hint = direct
    ? '不经代理，直接用本机 IP 打上游。服务器 IP 一旦被上游标记，从它出去过的号会接连被判死，且封号会记在各自的代理头上，很难查。'
    : [
        `出口 ${maskProxyUrl(proxyUrl ?? '')}`,
        peers != null
          ? peers > 1
            ? `${peers} 个启用中的号共用这个出口——同出口的号会一起暴露，一个被标记容易连坐`
            : '当前只有这一个号在用'
          : null,
        burnedAccounts != null && burnedAccounts > 0
          ? `这个出口历史上烧过 ${burnedAccounts} 个号`
          : null,
      ]
        .filter(Boolean)
        .join('\n')

  return (
    <span
      className={cn(
        'inline-flex min-w-0 items-center gap-1 rounded px-1 font-mono text-[11px]',
        direct
          ? 'bg-destructive/10 text-destructive'
          : burnedAccounts && burnedAccounts > 0
            ? 'bg-amber-500/10 text-amber-700 dark:text-amber-400'
            : 'bg-secondary text-muted-foreground',
        className,
      )}
      title={hint}
    >
      {direct ? (
        <ShieldAlert className="h-3 w-3 shrink-0" />
      ) : (
        <Network className="h-3 w-3 shrink-0" />
      )}
      <span className="truncate">{direct ? '直连·暴露本机 IP' : host}</span>
      {!direct && peers != null && peers > 1 && (
        <span
          className="inline-flex shrink-0 items-center gap-0.5 text-[10px] opacity-80"
          aria-label={`${peers} 个号共用`}
        >
          <Users className="h-2.5 w-2.5" />
          {peers}
        </span>
      )}
      {burnedAccounts != null && burnedAccounts > 0 && (
        <span className="shrink-0 text-[10px] font-semibold" aria-label={`烧过 ${burnedAccounts} 个号`}>
          🔥{burnedAccounts}
        </span>
      )}
    </span>
  )
}

interface ActivityBadgeProps {
  activity?: RecentActivity
  windowMinutes?: number
  className?: string
}

/**
 * 近 N 分钟的请求形态：成功 / 429。
 *
 * 429 单独标出来而不是并进失败：它是「打得太狠」的直接证据。判断一个号是不是
 * 被打爆，看的就是这个比例——线上被批量判死的那批，单号 429 占比在 8%~12%，
 * 而且没有一分钟是 0。
 */
export function CredentialActivityBadge({
  activity,
  windowMinutes = 60,
  className,
}: ActivityBadgeProps) {
  if (!activity || activity.attempts === 0) {
    return (
      <span
        className={cn('inline-flex items-center gap-1 text-[11px] text-muted-foreground/70', className)}
        title={`最近 ${windowMinutes} 分钟没有请求`}
      >
        <Activity className="h-3 w-3" />
        闲置
      </span>
    )
  }

  const { success, rateLimited, attempts } = activity
  const ratePct = attempts > 0 ? Math.round((rateLimited / attempts) * 100) : 0
  // 5% 是经验线：线上被成批判死的号都在 8% 以上，健康的号基本见不到 429
  const hot = ratePct >= 5

  return (
    <span
      className={cn('inline-flex items-center gap-1 text-[11px]', className)}
      title={[
        `最近 ${windowMinutes} 分钟：成功 ${success} 次`,
        `被限流(429) ${rateLimited} 次，占 ${ratePct}%`,
        activity.otherFailures > 0 ? `其它失败 ${activity.otherFailures} 次` : null,
        `总跳数 ${attempts}`,
        '',
        hot
          ? '429 占比偏高，说明投递速率已经超过上游给这个号的天花板。持续超限投递是账号被判死的主要信号之一，建议下调该号的 RPM 上限。'
          : '429 占比正常。',
      ]
        .filter((line) => line !== null)
        .join('\n')}
    >
      <Activity className={cn('h-3 w-3', hot && 'text-destructive')} />
      <span className="tabular-nums text-muted-foreground">
        {windowMinutes >= 60 ? `${windowMinutes / 60}h` : `${windowMinutes}m`}
      </span>
      <span className="tabular-nums text-emerald-600 dark:text-emerald-400">{success}</span>
      {rateLimited > 0 && (
        <>
          <span className="text-muted-foreground/50">/</span>
          <span
            className={cn(
              'tabular-nums',
              hot ? 'font-semibold text-destructive' : 'text-amber-600 dark:text-amber-400',
            )}
          >
            429·{rateLimited}
          </span>
        </>
      )}
    </span>
  )
}

/** 区域徽章：把 apiRegion 从一堆灰字里拎出来，扫一眼能看出号是不是散在多个区 */
export function CredentialRegionBadge({
  region,
  className,
}: {
  region?: string | null
  className?: string
}) {
  if (!region) return null
  return (
    <span
      className={cn(
        'inline-flex items-center gap-1 rounded bg-secondary px-1 font-mono text-[11px] text-muted-foreground',
        className,
      )}
      title={`数据面区域 ${region}`}
    >
      <Globe className="h-3 w-3 shrink-0" />
      {region}
    </span>
  )
}
