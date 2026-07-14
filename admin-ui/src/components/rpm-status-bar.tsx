import type { RpmSummary } from '@/types/api'

interface RpmStatusBarProps {
  summary?: RpmSummary
  totalInFlight: number
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

export function RpmStatusBar({ summary, totalInFlight }: RpmStatusBarProps) {
  const current = summary?.current ?? 0
  const limitedCapacity = summary?.limitedCapacity ?? 0
  const remainingLimitedCapacity = summary?.remainingLimitedCapacity ?? 0
  const unlimitedAccounts = summary?.unlimitedAccounts ?? 0
  const saturatedAccounts = summary?.saturatedAccounts ?? 0
  const hasUnlimitedCapacity = unlimitedAccounts > 0

  return (
    <section
      aria-label="最近60秒 RPM 状态"
      className="mb-4 border-y border-border/70 bg-muted/40 px-3 py-2 sm:px-4"
    >
      <div className="grid grid-cols-2 gap-x-4 gap-y-1 sm:grid-cols-5">
        <StatusItem label="最近60秒 RPM" value={current} />
        <StatusItem
          label="有限容量"
          value={hasUnlimitedCapacity ? '容量不限' : limitedCapacity}
          detail={
            hasUnlimitedCapacity
              ? `有限 ${limitedCapacity} · 不限速 ${unlimitedAccounts}`
              : undefined
          }
        />
        <StatusItem
          label="剩余"
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
      </div>
    </section>
  )
}
