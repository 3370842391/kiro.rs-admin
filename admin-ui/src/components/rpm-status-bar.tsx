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

/**
 * 状态色。与 badge.tsx 的 variant 取值保持一致，避免同一语义在两处用不同的绿/黄。
 *
 * `ok` 是显式的「健康」而不是「无状态」：满载账号为 0、剩余容量充足这类好消息
 * 值得被看见，全用默认前景色会让好坏都长一个样。
 */
type Tone = 'neutral' | 'ok' | 'warning' | 'danger'

const TONE_CLASS: Record<Tone, string> = {
  neutral: 'text-foreground',
  ok: 'text-emerald-600 dark:text-emerald-400',
  warning: 'text-amber-600 dark:text-amber-400',
  danger: 'text-destructive',
}

interface StatusItemProps {
  label: string
  value: string | number
  detail?: string
  tone?: Tone
  /** 实时量（60 秒滑动窗口）加呼吸点，与静态容量快照区分开 */
  live?: boolean
  /** 该簇的主指标：字号更大，让每组有一个视觉落点 */
  primary?: boolean
}

function StatusItem({
  label,
  value,
  detail,
  tone = 'neutral',
  live = false,
  primary = false,
}: StatusItemProps) {
  return (
    <div className="min-w-0">
      <div className="flex items-center gap-1 text-[11px] leading-tight text-muted-foreground">
        {live && (
          // 呼吸点挪到标签行：原先放在数值前面，会把数字挤得左右不对齐，
          // 一组数字扫下来时基线是歪的。
          <span
            aria-hidden="true"
            className={`h-1.5 w-1.5 shrink-0 rounded-full bg-current motion-safe:animate-pulse ${TONE_CLASS[tone]}`}
          />
        )}
        <span className="truncate">{label}</span>
      </div>
      <div
        className={`min-w-0 truncate tabular-nums ${TONE_CLASS[tone]} ${
          primary ? 'text-lg font-semibold leading-snug' : 'text-sm font-semibold leading-snug'
        }`}
      >
        {value}
      </div>
      {detail ? (
        <div className="min-w-0 truncate text-[11px] leading-tight tabular-nums text-muted-foreground">
          {detail}
        </div>
      ) : null}
    </div>
  )
}

/**
 * 一组语义相关的指标。组间用竖线分隔——七个等宽列摆在一起时，
 * 「满载账号」和「可用积分」看上去是同一类东西，实际一个是限流、一个是钱。
 */
function StatusGroup({
  children,
  label,
}: {
  children: React.ReactNode
  label: string
}) {
  return (
    <div
      role="group"
      aria-label={label}
      className="flex min-w-0 items-start gap-x-5 gap-y-2 border-border/70 sm:gap-x-6 [&:not(:first-child)]:sm:border-l [&:not(:first-child)]:sm:pl-5 xl:[&:not(:first-child)]:pl-6"
    >
      {children}
    </div>
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
    <section
      aria-label="号池实时状态"
      className="mb-4 border-y border-border/70 bg-muted/40 px-3 py-2.5 sm:px-4"
    >
      {/* 三簇按语义分组，整簇换行——七个等宽列在窄屏会拆成毫无关系的碎片。 */}
      <div className="flex flex-wrap items-start gap-x-5 gap-y-3 sm:gap-x-6">
        <StatusGroup label="吞吐与容量">
          <StatusItem label="最近 60 秒 RPM" value={current} primary />
          <StatusItem
            label={hasUnlimitedCapacity ? '总容量' : '有限容量'}
            value={hasUnlimitedCapacity ? '不限速' : limitedCapacity}
            detail={`有限 ${limitedCapacity} · 不限速 ${unlimitedAccounts} 个账号`}
          />
          <StatusItem
            label={hasUnlimitedCapacity ? '有限账号剩余' : '剩余容量'}
            value={remainingLimitedCapacity}
            tone={capacityExhausted ? 'warning' : 'neutral'}
          />
          <StatusItem
            label="满载账号"
            value={saturatedAccounts}
            // 0 个满载是好消息，值得显式标绿，而不是和其它数字一样的黑色
            tone={saturatedAccounts > 0 ? 'danger' : 'ok'}
          />
        </StatusGroup>

        <StatusGroup label="实时负载">
          <StatusItem
            label="进行中请求"
            value={totalInFlight}
            tone={totalInFlight > 0 ? 'warning' : 'neutral'}
            live
            primary
            detail="全池当前在飞"
          />
        </StatusGroup>

        <StatusGroup label="额度">
          <StatusItem
            label="可用积分"
            value={creditDisplay.value}
            detail={creditDisplay.detail}
            primary
          />
          <StatusItem
            label="积分消耗"
            value={`${formatCredits(burnRate)} /分钟`}
            tone={burnRate > 0 ? 'warning' : 'neutral'}
            live
            detail="最近 60 秒实际"
          />
        </StatusGroup>
      </div>
    </section>
  )
}
