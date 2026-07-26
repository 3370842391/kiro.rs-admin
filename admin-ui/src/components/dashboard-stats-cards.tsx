import { Card, CardContent } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { formatNumber } from '@/lib/utils'

/**
 * 凭据管理页顶部的三张统计卡片。
 *
 * 从 dashboard.tsx 里抽出来：这块只依赖 `total / available / currentId` 三个值，
 * 不碰那近四十个 useState，所以拆出来不需要透传一堆 prop——是这个巨型组件里
 * 少数几处能干净切开的边界之一。
 */
interface DashboardStatsCardsProps {
  available?: number
  currentId?: number
  total?: number
}

export function DashboardStatsCards({
  available,
  currentId,
  total,
}: DashboardStatsCardsProps) {
  return (
    <div className="mb-5 grid grid-cols-3 gap-2 sm:mb-6 sm:gap-4">
      <StatCard label="凭据总数" value={formatNumber(total)} />
      <StatCard
        label="可用凭据"
        value={formatNumber(available)}
        // 与 badge.tsx 的 success variant 同色，保持「绿=可用」在全站一致
        valueClassName="text-emerald-600 dark:text-emerald-400"
      />
      <Card className="hover:border-border">
        <CardContent className="p-3 sm:p-5">
          <StatLabel>当前活跃</StatLabel>
          <div className="mt-1.5 flex min-w-0 flex-wrap items-center gap-1.5 sm:mt-2 sm:gap-2">
            <span className="truncate text-2xl font-semibold tracking-tight tabular-nums sm:text-3xl">
              #{currentId || '-'}
            </span>
            {currentId ? <Badge variant="success">活跃</Badge> : null}
          </div>
        </CardContent>
      </Card>
    </div>
  )
}

function StatCard({
  label,
  value,
  valueClassName = '',
}: {
  label: string
  value: string
  valueClassName?: string
}) {
  return (
    // 不做 hover 抬升：卡片不可点击，上浮 + 加重投影会被读成「这里能点」。
    <Card className="hover:border-border">
      <CardContent className="p-3 sm:p-5">
        <StatLabel>{label}</StatLabel>
        <div
          className={`mt-1.5 text-2xl font-semibold tracking-tight tabular-nums sm:mt-2 sm:text-3xl ${valueClassName}`}
        >
          {value}
        </div>
      </CardContent>
    </Card>
  )
}

function StatLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="text-[11px] font-medium text-muted-foreground sm:text-[13px]">
      {children}
    </div>
  )
}
