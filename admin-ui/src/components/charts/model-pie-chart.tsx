import { memo, useMemo } from 'react'
import { PieChart, Pie, Cell, Tooltip, ResponsiveContainer, Legend } from 'recharts'
import type { ModelDistribution } from '@/types/api'
import { tooltipContentStyle, tooltipItemStyle, tooltipLabelStyle } from './tooltip-style'
import {
  CATEGORICAL_COLORS,
  CHART_FONT_SIZE,
  MAX_PIE_SLICES,
  OTHER_COLOR,
} from './chart-theme'
import { formatNumber } from '@/lib/utils'

interface Props {
  data: ModelDistribution[]
}

interface ChartDatum {
  inputTokens: number
  name: string
  outputTokens: number
  value: number
}

function ModelPieChartImpl({ data }: Props) {
  const { chartData, total } = useMemo(() => buildChartData(data), [data])

  if (data.length === 0) {
    return <EmptyModelChart />
  }

  return <ModelChartContent chartData={chartData} total={total} />
}

/**
 * 按调用量降序取前 N 个模型，其余合并成「其他」。
 *
 * 之前是全量渲染 + 10 色循环：号池里跑十几个模型时，环形图变成一圈分不清的细条，
 * 图例还会把卡片撑开。完整明细本来就在下方的 ModelTable 里，环形图只需要表达
 * 「谁占大头」。
 */
function buildChartData(data: ModelDistribution[]) {
  const total = data.reduce((s, d) => s + d.calls, 0) || 1
  const sorted = [...data].sort((a, b) => b.calls - a.calls)
  const head = sorted.slice(0, MAX_PIE_SLICES)
  const tail = sorted.slice(MAX_PIE_SLICES)

  const chartData: ChartDatum[] = head.map((d) => ({
    inputTokens: d.inputTokens,
    name: d.model,
    outputTokens: d.outputTokens,
    value: d.calls,
  }))

  if (tail.length > 0) {
    chartData.push({
      inputTokens: tail.reduce((s, d) => s + d.inputTokens, 0),
      name: `其他 ${tail.length} 个模型`,
      outputTokens: tail.reduce((s, d) => s + d.outputTokens, 0),
      value: tail.reduce((s, d) => s + d.calls, 0),
    })
  }

  return { chartData, total }
}

/** 「其他」聚合项固定用中性灰，不占用语义色。 */
function sliceColor(index: number, name: string): string {
  return name.startsWith('其他 ') ? OTHER_COLOR : CATEGORICAL_COLORS[index % CATEGORICAL_COLORS.length]
}

function EmptyModelChart() {
  return (
    <div className="flex h-[180px] items-center justify-center text-sm text-muted-foreground sm:h-[260px]">
      暂无数据
    </div>
  )
}

function ModelChartContent({
  chartData,
  total,
}: {
  chartData: ChartDatum[]
  total: number
}) {
  return (
    <div className="h-[220px] sm:h-[260px]">
      <ResponsiveContainer width="100%" height="100%">
        <PieChart>
          <Pie
            data={chartData}
            dataKey="value"
            nameKey="name"
            cx="50%"
            cy="50%"
            outerRadius="72%"
            innerRadius="40%"
            paddingAngle={2}
            isAnimationActive={false}
          >
          {chartData.map((d, i) => (
            <Cell key={d.name} fill={sliceColor(i, d.name)} />
          ))}
        </Pie>
          <Tooltip
            contentStyle={tooltipContentStyle}
            labelStyle={tooltipLabelStyle}
            itemStyle={tooltipItemStyle}
            cursor={false}
            formatter={(value: number, _name, item) =>
              formatTooltipValue({ item, total, value })}
          />
          <Legend wrapperStyle={{ fontSize: CHART_FONT_SIZE.legend }} iconSize={8} />
        </PieChart>
      </ResponsiveContainer>
    </div>
  )
}

function formatTooltipValue({
  item,
  total,
  value,
}: {
  item?: { payload?: ChartDatum }
  total: number
  value: number
}) {
  const pct = ((value / total) * 100).toFixed(1)
  const payload = item?.payload
  const input = formatNumber(payload?.inputTokens ?? 0)
  const output = formatNumber(payload?.outputTokens ?? 0)
  return [`${formatNumber(value)} 次（${pct}%）  in ${input} / out ${output}`, payload?.name ?? '']
}

export const ModelPieChart = memo(ModelPieChartImpl)
