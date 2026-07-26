/**
 * 三个 recharts 图共用的 Tooltip 样式
 *
 * 注意：recharts 的 Tooltip label 和 item 各有独立 style，
 * 不会继承 contentStyle.color；必须分别设 labelStyle / itemStyle。
 */
import type { CSSProperties } from 'react'
import { CHART_FONT_SIZE } from './chart-theme'

export const tooltipContentStyle: CSSProperties = {
  background: 'rgba(20,20,20,0.94)',
  border: '1px solid rgba(255,255,255,0.08)',
  borderRadius: 10,
  padding: '8px 12px',
  boxShadow: '0 8px 24px rgba(0,0,0,0.25)',
  fontSize: CHART_FONT_SIZE.tooltip,
  color: '#fff',
}

export const tooltipLabelStyle: CSSProperties = {
  color: 'rgba(255,255,255,0.85)',
  fontWeight: 500,
  marginBottom: 4,
}

export const tooltipItemStyle: CSSProperties = {
  color: '#fff',
  padding: '2px 0',
}

/**
 * 悬停时高亮当前列/区间的遮罩。
 *
 * 原值是 `rgba(255,255,255,0.06)`——白色半透明画在浅色卡片（`--card: 0 0% 100%`）上
 * 完全看不见，浅色模式下等于没有悬停反馈。改用中性灰：它在白底上压暗、在深底上提亮，
 * 两种主题下都可见。
 */
export const tooltipCursorStyle = {
  fill: 'rgba(120,120,128,0.16)',
}
