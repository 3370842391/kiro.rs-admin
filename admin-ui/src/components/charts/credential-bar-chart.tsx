import { memo, useMemo } from 'react'
import { BarChart, Bar, XAxis, YAxis, CartesianGrid, Tooltip, ResponsiveContainer, Legend } from 'recharts'
import type { CredentialDistribution } from '@/types/api'
import { tooltipContentStyle, tooltipCursorStyle, tooltipItemStyle, tooltipLabelStyle } from './tooltip-style'
import { CHART_FONT_SIZE, SERIES_COLORS } from './chart-theme'
import { formatNumber } from '@/lib/utils'

interface Props {
  data: CredentialDistribution[]
}

interface ChartDatum {
  calls: number
  errors: number
  fullLabel: string
  inputTokens: number
  label: string
  outputTokens: number
}

function CredentialBarChartImpl({ data }: Props) {
  const formatted = useMemo(() => buildChartData(data), [data])

  if (data.length === 0) {
    return <EmptyCredentialChart />
  }

  return <CredentialChartContent data={formatted} />
}

function buildChartData(data: CredentialDistribution[]): ChartDatum[] {
  // 显式按总 Token 降序：条形图对比的前提是有序，依赖接口返回顺序不可靠，
  // 而且 slice(0,12) 截断的必须是「最小的那些」而不是「碰巧排在后面的」。
  return [...data]
    .sort((a, b) => b.inputTokens + b.outputTokens - (a.inputTokens + a.outputTokens))
    .slice(0, 12)
    .map((d) => {
      const fullLabel = d.email ?? `#${d.credentialId}`
      return {
        calls: d.calls,
        errors: d.errors,
        fullLabel,
        inputTokens: d.inputTokens,
        label: d.email ? truncateEmail(d.email) : fullLabel,
        outputTokens: d.outputTokens,
      }
    })
}

function EmptyCredentialChart() {
  return (
    <div className="flex h-[180px] items-center justify-center text-sm text-muted-foreground sm:h-[260px]">
      暂无数据
    </div>
  )
}

function CredentialChartContent({ data }: { data: ChartDatum[] }) {
  return (
    <div className="h-[280px] sm:h-[340px]">
      <ResponsiveContainer width="100%" height="100%">
        <BarChart data={data} margin={{ top: 8, right: 8, left: -10, bottom: 52 }}>
          {credentialChartAxes()}
          {credentialChartTooltip()}
          <Legend
        verticalAlign="top"
        align="right"
        height={28}
        wrapperStyle={{ fontSize: CHART_FONT_SIZE.legend }}
      />
          {credentialChartBars()}
        </BarChart>
      </ResponsiveContainer>
    </div>
  )
}

function credentialChartAxes() {
  return [
    <CartesianGrid key="grid" strokeDasharray="3 3" className="stroke-border/50" />,
    <XAxis
      key="x"
      dataKey="label"
      tick={{ fontSize: CHART_FONT_SIZE.axis }}
      angle={-30}
      textAnchor="end"
      interval={0}
      height={64}
    />,
    <YAxis
      key="y"
      tick={{ fontSize: CHART_FONT_SIZE.axis }}
      tickFormatter={(v: number) => formatNumber(v)}
      width={42}
    />,
  ]
}

function credentialChartTooltip() {
  return (
    <Tooltip
      contentStyle={tooltipContentStyle}
      labelStyle={tooltipLabelStyle}
      itemStyle={tooltipItemStyle}
      cursor={tooltipCursorStyle}
      formatter={(value: number) => formatNumber(value)}
      labelFormatter={formatTooltipLabel}
    />
  )
}

function formatTooltipLabel(label: string, payload?: ReadonlyArray<{ payload?: ChartDatum }>) {
  return payload?.[0]?.payload?.fullLabel ?? label
}

function credentialChartBars() {
  return [
    // 颜色取自共享主题：这里的「输入 / 输出」必须和趋势图里同名序列同色，
    // 否则同一页两张图对同一个概念用两种颜色。
    <Bar
      key="input"
      dataKey="inputTokens"
      name="输入"
      stackId="a"
      fill={SERIES_COLORS.input}
      isAnimationActive={false}
    />,
    <Bar
      key="output"
      dataKey="outputTokens"
      name="输出"
      stackId="a"
      fill={SERIES_COLORS.output}
      isAnimationActive={false}
    />,
  ]
}

export const CredentialBarChart = memo(CredentialBarChartImpl)

/** 仅用于 X 轴展示：保留 @ 后域名前 1-2 段，整体最长 22 字符 */
function truncateEmail(email: string): string {
  if (email.length <= 22) return email
  const at = email.indexOf('@')
  if (at < 0) return email.slice(0, 20) + '…'
  const name = email.slice(0, at)
  const domain = email.slice(at + 1)
  const shortName = name.length > 12 ? name.slice(0, 11) + '…' : name
  return `${shortName}@${domain}`
}
