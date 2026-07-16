import type { RpmSummary } from '@/types/api'
import {
  formatAvailableCreditSummary,
  type AvailableCreditSummary,
} from '@/lib/credential-summary'

interface RpmStatusBarProps {
  summary?: RpmSummary
  totalInFlight: number
  availableCreditSummary: AvailableCreditSummary
}

interface StatusItemProps {
  label: string
  value: string | number
  detail?: string
  tone?: 'default' | 'warning' | 'danger'
}

function StatusItem({ label, value, detail, tone = 'default' }: StatusItemProps) {
  const toneClass =
    tone === 'danger'
      ? 'text-destructive'
      : tone === 'warning'
        ? 'text-amber-600 dark:text-amber-400'
        : 'text-foreground'

  return (
    <div className="min-w-0 py-1">
      <div className="text-[11px] text-muted-foreground">{label}</div>
      <div className={`min-w-0 break-words text-sm font-semibold tabular-nums ${toneClass}`}>
        {value}
      </div>
      {detail ? (
        <div className="min-w-0 break-words text-[11px] tabular-nums text-muted-foreground">
          {detail}
        </div>
      ) : null}
    </div>
  )
}

export function RpmStatusBar({
  summary,
  totalInFlight,
  availableCreditSummary,
}: RpmStatusBarProps) {
  const current = summary?.current ?? 0
  const limitedCapacity = summary?.limitedCapacity ?? 0
  const remainingLimitedCapacity = summary?.remainingLimitedCapacity ?? 0
  const unlimitedAccounts = summary?.unlimitedAccounts ?? 0
  const saturatedAccounts = summary?.saturatedAccounts ?? 0
  const hasUnlimitedCapacity = unlimitedAccounts > 0
  const creditDisplay = formatAvailableCreditSummary(availableCreditSummary)

  return (
    <section
      aria-label="最近60秒 RPM 状态"
      className="mb-4 border-y border-border/70 bg-muted/40 px-3 py-2 sm:px-4"
    >
      <div className="grid grid-cols-2 gap-x-4 gap-y-1 sm:grid-cols-3 xl:grid-cols-6">
        <StatusItem label="最近60秒 RPM" value={current} />
        <StatusItem
          label={hasUnlimitedCapacity ? '总容量' : '有限容量'}
          value={hasUnlimitedCapacity ? '不限速' : limitedCapacity}
          detail={`有限账号容量 ${limitedCapacity} · 不限速账号 ${unlimitedAccounts}`}
        />
        <StatusItem
          label={hasUnlimitedCapacity ? '有限账号剩余' : '剩余'}
          value={remainingLimitedCapacity}
          tone={remainingLimitedCapacity === 0 && limitedCapacity > 0 ? 'warning' : 'default'}
        />
        <StatusItem
          label="满载账号"
          value={saturatedAccounts}
          tone={saturatedAccounts > 0 ? 'danger' : 'default'}
        />
        <StatusItem
          label="进行中请求"
          value={totalInFlight}
          tone={totalInFlight > 0 ? 'warning' : 'default'}
        />
        <StatusItem
          label="可用积分"
          value={creditDisplay.value}
          detail={creditDisplay.detail}
        />
      </div>
    </section>
  )
}
