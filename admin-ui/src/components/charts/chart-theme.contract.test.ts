import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import {
  CATEGORICAL_COLORS,
  CHART_FONT_SIZE,
  MAX_PIE_SLICES,
  OTHER_COLOR,
  SERIES_COLORS,
} from './chart-theme'

async function read(file: string): Promise<string> {
  return readFile(new URL(`./${file}`, import.meta.url), 'utf8')
}

/** 相对亮度 → WCAG 对比度，用于验证配色在明暗两种底色上都能看清。 */
function relativeLuminance(hex: string): number {
  const channel = (value: number) => {
    const v = value / 255
    return v <= 0.04045 ? v / 12.92 : ((v + 0.055) / 1.055) ** 2.4
  }
  const int = parseInt(hex.slice(1), 16)
  const r = channel((int >> 16) & 0xff)
  const g = channel((int >> 8) & 0xff)
  const b = channel(int & 0xff)
  return 0.2126 * r + 0.7152 * g + 0.0722 * b
}

function contrast(a: string, b: string): number {
  const [hi, lo] = [relativeLuminance(a), relativeLuminance(b)].sort((x, y) => y - x)
  return (hi + 0.05) / (lo + 0.05)
}

describe('chart theme contract', () => {
  test('series colours live in one place, not duplicated per chart file', async () => {
    const sources = await Promise.all([
      read('time-series-chart.tsx'),
      read('credential-bar-chart.tsx'),
      read('model-pie-chart.tsx'),
    ])

    for (const src of sources) {
      // 之前三个文件各自写死 '#3b82f6' / '#10b981'，「输入是蓝色」靠巧合维持。
      expect(src).not.toContain('#3b82f6')
      expect(src).not.toContain('#10b981')
      expect(src).toContain('./chart-theme')
    }
  })

  test('primary series matches the app accent instead of the Tailwind default', () => {
    // --primary 在 index.css 里是 hsl(211 100% 50%) = #007AFF。
    // 图表主序列必须同色，否则同一屏出现两种「主色蓝」。
    expect(SERIES_COLORS.input).toBe('#007AFF')
  })

  test('categorical palette stays within the pie-chart readability limit', () => {
    // ui-ux-pro-max 图表指引：饼图 5-6 色为上限，超过就该换堆叠条形图。
    expect(CATEGORICAL_COLORS.length).toBeLessThanOrEqual(6)
    expect(MAX_PIE_SLICES).toBe(CATEGORICAL_COLORS.length)
    // 聚合项用中性灰，不能占用任何语义色。
    expect(CATEGORICAL_COLORS).not.toContain(OTHER_COLOR)
  })

  test('every series colour is legible on both light and dark surfaces', () => {
    const lightCard = '#FFFFFF'
    const darkCard = '#1B1D23' // --card 深色态近似值
    for (const [name, hex] of Object.entries(SERIES_COLORS)) {
      // 3:1 是 WCAG 对图形/非文本元素的门槛（线条、色块，非正文）。
      expect(
        Math.min(contrast(hex, lightCard), contrast(hex, darkCard)),
        `${name} (${hex}) 在明暗底色上都要 ≥3:1`,
      ).toBeGreaterThanOrEqual(3)
    }
  })

  test('pie chart aggregates the long tail instead of rendering every model', async () => {
    const src = await read('model-pie-chart.tsx')

    expect(src).toContain('MAX_PIE_SLICES')
    expect(src).toContain('其他 ')
    expect(src).toContain('.sort((a, b) => b.calls - a.calls)')
  })

  test('bar chart sorts descending before truncating to top 12', async () => {
    const src = await read('credential-bar-chart.tsx')

    expect(src).toContain('.sort(')
    expect(src).toContain('.slice(0, 12)')
    // 排序必须在截断之前，否则截掉的不是最小的那些。
    expect(src.indexOf('.sort(')).toBeLessThan(src.indexOf('.slice(0, 12)'))
  })

  test('recharts JS animation honours reduced motion', async () => {
    const src = await read('time-series-chart.tsx')

    // recharts 是逐帧 JS 动画，index.css 的全局 CSS 兜底管不到它。
    expect(src).toContain('prefers-reduced-motion: reduce')
    expect(src).toContain('isAnimationActive={animate}')
  })

  test('hover cursor is visible on light cards', async () => {
    const { tooltipCursorStyle } = await import('./tooltip-style')

    // 原值是 rgba(255,255,255,0.06)，画在白卡片上等于没有悬停反馈。
    // 断言导出的实际值而不是源码文本——旧值会出现在解释性注释里，
    // 用 not.toContain 扫源码会把注释也算进去（第一版就是这么误判的）。
    expect(tooltipCursorStyle.fill).toBe('rgba(120,120,128,0.16)')
  })

  test('font scale is shared rather than 10/11/12 sprinkled per chart', async () => {
    expect(CHART_FONT_SIZE.axis).toBe(CHART_FONT_SIZE.legend)
    for (const file of ['time-series-chart.tsx', 'credential-bar-chart.tsx']) {
      const src = await read(file)
      expect(src).not.toContain('fontSize: 10')
      expect(src).not.toContain('fontSize: 11')
      expect(src).not.toContain('fontSize: 12')
    }
  })
})
