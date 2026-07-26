import type { RpmSummary } from '@/types/api'
import {
  formatAvailableCreditSummary,
  type AvailableCreditSummary,
} from '@/lib/credential-summary'
import { formatCredits } from '@/lib/utils'

interface RpmStatusBarProps {
  summary?: RpmSummary
  totalInFlight: number
  availableCreditSummary: AvailableCreditSummary
  /** 最近 60 秒的 credit 消耗速率（credits / 分钟） */
  creditsPerMinute?: number
}

/** 与 badge.tsx 的 variant 同色，避免同一语义在两处用不同的绿/黄。 */
type Tone = 'neutral' | 'ok' | 'warning' | 'danger'

const VALUE_TONE: Record<Tone, string> = {
  neutral: 'text-foreground',
  ok: 'text-emerald-600 dark:text-emerald-400',
  warning: 'text-amber-600 dark:text-amber-400',
  danger: 'text-destructive',
}

/** 实时指标的呼吸点颜色。neutral 时用弱色，避免静止值也在闪。 */
const DOT_TONE: Record<Tone, string> = {
  neutral: 'bg-muted-foreground/40',
  ok: 'bg-emerald-500',
  warning: 'bg-amber-500',
  danger: 'bg-destructive',
}

interface MetricProps {
  label: string
  value: string
  /** 单位单独传，用更小更淡的字号排在数值后面 —— 混进 value 会和数字抢注意力。 */
  unit?: string
  detail?: string
  tone?: Tone
  /** 60 秒滑动窗口的实时量，加呼吸点与静态快照区分 */
  live?: boolean
}

function Metric({ label, value, unit, detail, tone = 'neutral', live = false }: MetricProps) {
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1.5">
        {live && (
          <span
            aria-hidden="true"
            className={`h-1.5 w-1.5 shrink-0 rounded-full ${DOT_TONE[tone]} motion-safe:animate-pulse`}
          />
        )}
        {/* 标签压到 uppercase + tracking：小字号下更像"字段名"而不是内容，
            视觉重量让给数值。 */}
        <span className="truncate text-[10px] font-medium uppercase tracking-wider text-muted-foreground">
          {label}
        </span>
      </div>
      <div className="mt-1 flex min-w-0 items-baseline gap-1">
        <span
          className={`truncate text-xl font-semibold leading-none tracking-tight tabular-nums ${VALUE_TONE[tone]}`}
        >
          {value}
        </span>
        {unit ? (
          <span className="shrink-0 text-[11px] font-normal text-muted-foreground">{unit}</span>
        ) : null}
      </div>
      {detail ? (
        <div className="mt-1 truncate text-[11px] leading-tight text-muted-foreground">{detail}</div>
      ) : null}
    </div>
  )
}

/**
 * 一张指标卡。
 *
 * 上一版用「细边框条 + 竖线分隔」，实际渲染下分隔线几乎看不见，七个指标读起来
 * 仍是一条平铺的流水线。改成独立卡片：靠 surface 与留白分组，比 1px 竖线可靠得多，
 * 也和页面上其它 Card 的语言一致。
 */
function MetricCard({
  children,
  columns,
  title,
}: {
  children: React.ReactNode
  /** 卡内指标数，决定内部网格列数 */
  columns: number
  title: string
}) {
  return (
    <section
      aria-label={title}
      className="min-w-0 flex-1 rounded-xl border border-border/60 bg-card/70 px-4 py-3 backdrop-blur-sm"
    >
      <div
        className="grid gap-x-5 gap-y-3"
        style={{ gridTemplateColumns: `repeat(${columns}, minmax(0, 1fr))` }}
      >
        {children}
      </div>
    </section>
  )
}

export function RpmStatusBar({
  summary,
  totalInFlight,
  availableCreditSummary,
  creditsPerMinute = 0,
}: RpmStatusBarProps) {
  const current = summary?.current ?? 0
  const limitedCapacity = summary?.limitedCapacity ?? 0
  const remainingLimitedCapacity = summary?.remainingLimitedCapacity ?? 0
  const unlimitedAccounts = summary?.unlimitedAccounts ?? 0
  const saturatedAccounts = summary?.saturatedAccounts ?? 0
  const hasUnlimitedCapacity = unlimitedAccounts > 0
  const creditDisplay = formatAvailableCreditSummary(availableCreditSummary)
  const burnRate = Number.isFinite(creditsPerMinute) ? Math.max(0, creditsPerMinute) : 0
  const capacityExhausted = remainingLimitedCapacity === 0 && limitedCapacity > 0

  return (
    // mb-5：与下方凭据列表拉开距离。上一版 mb-4 加上无边距的条状容器，
    // 整块和列表粘在一起，没有呼吸感。
    <div className="mb-5 flex flex-col gap-3 lg:flex-row">
      <MetricCard title="吞吐与容量" columns={4}>
        <Metric label="RPM" value={String(current)} unit="次/分" detail="最近 60 秒" live />
        <Metric
          label={hasUnlimitedCapacity ? '总容量' : '有限容量'}
          value={hasUnlimitedCapacity ? '不限速' : String(limitedCapacity)}
          detail={`有限 ${limitedCapacity} · 不限速 ${unlimitedAccounts}`}
        />
        <Metric
          label={hasUnlimitedCapacity ? '有限账号剩余' : '剩余容量'}
          value={String(remainingLimitedCapacity)}
          tone={capacityExhausted ? 'warning' : 'neutral'}
          detail={capacityExhausted ? '已打满' : '可继续接量'}
        />
        <Metric
          label="满载账号"
          value={String(saturatedAccounts)}
          // 0 个满载是好消息，显式标绿；全用默认色的话好坏长一个样
          tone={saturatedAccounts > 0 ? 'danger' : 'ok'}
          detail={saturatedAccounts > 0 ? '需要扩容' : '无满载'}
        />
      </MetricCard>

      <MetricCard title="实时负载与额度" columns={3}>
        <Metric
          label="进行中"
          value={String(totalInFlight)}
          unit="请求"
          tone={totalInFlight > 0 ? 'warning' : 'neutral'}
          detail="全池在飞"
          live
        />
        {/* 可用积分与消耗速率相邻：余量 ÷ 速率 = 还能撑多久 */}
        <Metric label="可用积分" value={creditDisplay.value} detail={creditDisplay.detail} />
        <Metric
          label="积分消耗"
          value={formatCredits(burnRate)}
          unit="/分钟"
          tone={burnRate > 0 ? 'warning' : 'neutral'}
          detail="最近 60 秒"
          live
        />
      </MetricCard>
    </div>
  )
}
