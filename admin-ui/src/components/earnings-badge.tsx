import { Badge } from '@/components/ui/badge'
import { cn } from '@/lib/utils'
import type { CredentialEarnings, EarningsSummary } from '@/types/api'

/** ¥ 金额。收益都是小额，保留两位。 */
export function money(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return '-'
  return `¥${v.toFixed(2)}`
}

function credits(v: number | undefined | null): string {
  if (v == null || !Number.isFinite(v)) return '-'
  return v >= 1000 ? `${(v / 1000).toFixed(1)}k` : v.toFixed(0)
}

const QUOTA_SOURCE_NOTE: Record<string, string> = {
  manual: '额度为手填值',
  upstream: '额度取自上游 getUsageLimits',
  unknown: '额度未知：既没手填也没查到上游额度',
}

/**
 * 挂在凭据行上的收益徽章。
 *
 * 三种状态刻意分开显示，因为它们要运营做的事不一样：
 * - 没实测出卖价 → 去跑一次利润报表
 * - 没填买入价   → 去补进价
 * - 都有         → 显示已产生 / 剩余可产生 / 回本进度
 */
export function EarningsBadge({ earnings }: { earnings: CredentialEarnings | undefined }) {
  if (!earnings) return null

  // 卖价缺失时只能报积分。此时给出金额是编数，宁可提示去跑报表。
  if (earnings.revenueRmb == null) {
    if (earnings.creditsUsed <= 0) return null
    return (
      <Badge
        variant="outline"
        className="text-xs text-muted-foreground border-dashed"
        title={
          '已消耗 ' +
          credits(earnings.creditsUsed) +
          ' credits，但还没实测出卖价（¥/credit），无法折人民币。\n' +
          '到「NewAPI 利润」页跑一次报表即可——卖价从已匹配的流水里实测，不需要手配。'
        }
      >
        {credits(earnings.creditsUsed)} 分 · 待实测卖价
      </Badge>
    )
  }

  const paid = earnings.paybackRatio != null && earnings.paybackRatio >= 1
  const hasCost = earnings.costRmb != null

  return (
    <Badge
      variant="outline"
      className={cn(
        'text-xs',
        hasCost
          ? paid
            ? 'border-green-500/50 text-green-600 dark:text-green-400'
            : 'border-yellow-500/50 text-yellow-600 dark:text-yellow-400'
          : 'text-muted-foreground'
      )}
      title={[
        `已产生 ${money(earnings.revenueRmb)}`,
        earnings.remainingRmb != null
          ? `剩余额度还能产生 ${money(earnings.remainingRmb)}`
          : null,
        `已消耗 ${credits(earnings.creditsUsed)} / 额度 ${credits(earnings.quotaCredits)} credits`,
        QUOTA_SOURCE_NOTE[earnings.quotaSource],
        hasCost ? `买入 ${money(earnings.costRmb)}` : '未填买入价，所以不算利润',
        earnings.profitRmb != null ? `净利润 ${money(earnings.profitRmb)}` : null,
        earnings.paybackRatio != null
          ? `回本进度 ${Math.round(earnings.paybackRatio * 100)}%`
          : null,
        earnings.revenuePerHour != null
          ? `时薪 ${money(earnings.revenuePerHour)}/小时（存活 ${earnings.aliveHours?.toFixed(1)} 小时）`
          : null,
      ]
        .filter((line) => line !== null)
        .join('\n')}
    >
      {money(earnings.revenueRmb)}
      {earnings.remainingRmb != null ? ` +${money(earnings.remainingRmb)} 待赚` : ''}
      {hasCost && earnings.paybackRatio != null
        ? ` · 回本 ${Math.round(earnings.paybackRatio * 100)}%`
        : ''}
    </Badge>
  )
}

/** 号池收益汇总卡片 */
export function EarningsSummaryCard({ summary }: { summary: EarningsSummary | undefined }) {
  if (!summary) return null
  const rate = summary.sellRate

  return (
    <div className="rounded-lg border p-3 space-y-2">
      <div className="flex items-center justify-between gap-2 flex-wrap">
        <span className="text-sm font-medium">号池收益</span>
        {rate ? (
          <span
            className="text-xs text-muted-foreground"
            title={[
              `实测卖价 ¥${rate.rmbPerCredit.toFixed(4)} / credit`,
              `取自 ${rate.windowMinutes} 分钟窗口：收入 ${money(rate.revenueRmb)} ÷ ${credits(rate.credits)} credits`,
              `样本 ${rate.samples} 条流水`,
              `实测时间 ${new Date(rate.measuredAt).toLocaleString()}`,
              '',
              '卖价随模型结构漂移：同样的额度，跑 opus 和跑 haiku 能卖的钱差好几倍。',
              '所以只认最近一次实测值，样本太小时不要当真。',
            ].join('\n')}
          >
            卖价 ¥{rate.rmbPerCredit.toFixed(4)}/分 · 样本 {rate.samples}
          </span>
        ) : (
          <span className="text-xs text-muted-foreground">
            还没实测出卖价，去「NewAPI 利润」跑一次报表
          </span>
        )}
      </div>

      <div className="grid grid-cols-2 sm:grid-cols-4 gap-2 text-xs">
        <Metric label="总投入" value={money(summary.totalCostRmb)} />
        <Metric label="已产生" value={money(summary.totalRevenueRmb)} />
        <Metric
          label="剩余可产生"
          value={money(summary.totalRemainingRmb)}
          hint="号池里还没用掉的额度折成人民币，相当于存货价值"
        />
        <Metric
          label="净利润"
          value={money(summary.profitRmb)}
          tone={summary.profitRmb >= 0 ? 'good' : 'bad'}
          hint={`毛利率 ${summary.marginPct.toFixed(1)}%`}
        />
      </div>

      <div className="flex items-center gap-3 flex-wrap text-xs text-muted-foreground">
        <span>
          {summary.paidBackAccounts}/{summary.accounts} 个号已回本
        </span>
        {summary.accountsWithCost < summary.accounts && (
          <span title="没填买入价的号只进收入、不进成本，否则毛利率会虚高">
            {summary.accounts - summary.accountsWithCost} 个号未填买入价
          </span>
        )}
        {summary.revenuePerHour != null && <span>平均时薪 {money(summary.revenuePerHour)}/h</span>}
        {summary.paybackHours != null && (
          <span
            className="font-medium text-foreground"
            title={
              '按平均成本与平均时薪算，号需要活这么久才够本。\n' +
              '把它和实测存活时长放在一起，就能判断这批号到底赚不赚钱。'
            }
          >
            回本需活 {summary.paybackHours.toFixed(1)} 小时
          </span>
        )}
      </div>
    </div>
  )
}

function Metric({
  label,
  value,
  tone,
  hint,
}: {
  label: string
  value: string
  tone?: 'good' | 'bad'
  hint?: string
}) {
  return (
    <div className="space-y-0.5" title={hint}>
      <div className="text-muted-foreground">{label}</div>
      <div
        className={cn(
          'font-medium tabular-nums',
          tone === 'good' && 'text-green-600 dark:text-green-400',
          tone === 'bad' && 'text-destructive'
        )}
      >
        {value}
      </div>
    </div>
  )
}
