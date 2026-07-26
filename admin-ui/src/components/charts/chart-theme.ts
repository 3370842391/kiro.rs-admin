/**
 * 三个图表共享的配色与排版尺度。
 *
 * 改造前，`#3b82f6` / `#10b981` 这几个色值分别硬编码在 time-series、bar、pie 三个文件里：
 * 「输入是蓝色」这件事靠三处巧合维持，改一处就会错位。而且用的是 Tailwind 默认调色板，
 * 与应用其余部分的 Apple 系统色（`--primary: #007AFF`）明显不是一套——图表看着像
 * 从别的项目粘过来的。
 *
 * 这里统一到 macOS 系统色，与 index.css 的主题变量同源。用字面量而不是 `hsl(var(--x))`：
 * recharts 会把颜色同时写进 SVG 属性和 canvas 度量，CSS 变量在部分路径上取不到值。
 *
 * **同一套色值要同时用在白卡片和深色卡片上**，所以取色的硬约束是「两边都 ≥3:1」
 * （WCAG 对图形/非文本元素的门槛）。macOS 的鲜艳系统绿 `#34C759`、橙 `#FF9500`、
 * 青 `#5AC8FA` 是给填充块配白字用的，画成白底上的细线只有 2.2 / 2.2 / 1.9，
 * 基本看不见——这三个改用同色相的加深版（相当于 Apple 的 accessible 变体）。
 * `chart-theme.contract.test.ts` 会算对比度把这条约束钉住。
 */

/** 语义化数据序列配色。键名即业务含义，不要按「第几条线」来取色。 */
export const SERIES_COLORS = {
  /** 输入 Token — 与 --primary 同色，主序列。白 4.02 / 暗 4.19 */
  input: '#007AFF',
  /** 输出 Token — 加深的系统绿。白 4.10 / 暗 4.11 */
  output: '#179044',
  /** 缓存写入 — 加深的系统橙。白 4.10 / 暗 4.11 */
  cacheCreation: '#C26405',
  /** 缓存读取 — 加深的系统青。白 4.11 / 暗 4.10 */
  cacheRead: '#0B87B3',
  /** 缓存命中率（右轴百分比） — 系统紫。白 4.13 / 暗 4.08 */
  cacheHitRate: '#AF52DE',
  /** Credit 计费 — 系统粉。白 3.65 / 暗 4.62 */
  credits: '#FF2D55',
} as const

/**
 * 分类型数据（模型分布等）的取色序列。
 *
 * 只有 6 个：ui-ux-pro-max 的图表指引明确写了饼图「5-6 色为上限，超过 5 项应改用堆叠条形图」。
 * 之前是 10 色循环且不截断，模型一多就变成一圈认不出的彩纸，图例还会溢出卡片。
 * 调用方需要自行把尾部聚合成「其他」。
 */
export const CATEGORICAL_COLORS = [
  SERIES_COLORS.input,
  SERIES_COLORS.output,
  SERIES_COLORS.cacheCreation,
  SERIES_COLORS.cacheHitRate,
  SERIES_COLORS.cacheRead,
  SERIES_COLORS.credits,
] as const

/** 超出 CATEGORICAL_COLORS 数量后聚合项的颜色（中性灰，不与任何语义色冲突）。 */
export const OTHER_COLOR = '#8E8E93'

/** 饼图最多单独显示几项，其余并入「其他」。 */
export const MAX_PIE_SLICES = CATEGORICAL_COLORS.length

/**
 * 图表内字号。原先 10 / 11 / 12 三种混用且分散在各文件，
 * 同一张图的 X 轴 10px、Y 轴 11px、图例 12px，视觉上是乱的。
 */
export const CHART_FONT_SIZE = {
  axis: 11,
  legend: 11,
  tooltip: 12,
} as const
