import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

/**
 * 核心页面的无障碍不变量。
 *
 * 这些约束单看每一条都很容易在后续改版里被顺手删掉（一个 aria-label、一个 scope），
 * 删掉后功能完全正常、肉眼也看不出区别，只有读屏和键盘用户会受影响——正因为不会
 * 报错才需要用测试钉住。
 */
async function read(file: string): Promise<string> {
  return readFile(new URL(`./${file}`, import.meta.url), 'utf8')
}

describe('overview page accessibility contract', () => {
  test('every filter control carries an accessible name', async () => {
    const src = await read('overview-page.tsx')

    // 三个下拉都只显示当前值、没有可见 <label>，缺 aria-label 就只会被念成「组合框」。
    expect(src).toContain('aria-label="按入口 Key 筛选"')
    expect(src).toContain('aria-label="按账号分组筛选"')
    expect(src).toContain('aria-label="统计粒度"')
    // 两个日期输入同理，只有一个日历图标。
    expect(src).toContain('label="开始日期"')
    expect(src).toContain('label="结束日期"')
    expect(src).toContain('aria-label={label}')
  })

  test('toggle groups expose state beyond colour alone', async () => {
    const src = await read('overview-page.tsx')

    expect(src).toContain('role="group"')
    expect(src).toContain('aria-label="时间区间"')
    expect(src).toContain('aria-pressed={currentRange === r.value}')
  })

  test('model table is navigable and scoped', async () => {
    const src = await read('overview-page.tsx')

    expect(src).toContain('<caption className="sr-only">')
    expect(src).toContain('scope="col"')
    expect(src).toContain('scope="row"')
    // 可滚动区域要能用键盘进入，否则超出 max-h-32 的行只有鼠标够得着。
    expect(src).toContain('aria-label="按模型分布明细"')
  })

  test('date range validation is announced, not just implied by a disabled button', async () => {
    const src = await read('overview-page.tsx')

    expect(src).toContain('role="alert"')
    expect(src).toContain('结束日期不能早于开始日期')
  })
})

describe('trace log page accessibility contract', () => {
  test('row expansion is a real button with expanded state', async () => {
    const src = await read('trace-log-page.tsx')

    // <tr onClick> 不可聚焦、不响应 Enter/Space；真正的控件必须是 <button>。
    expect(src).toContain('aria-expanded={open}')
    expect(src).toContain('aria-controls={open ? detailId : undefined}')
    // 行点击与按钮点击都会冒泡到 <tr>，不 stopPropagation 会触发两次等于没展开。
    expect(src).toContain('e.stopPropagation()')
  })

  test('every filter select carries an accessible name', async () => {
    const src = await read('trace-log-page.tsx')

    expect(src).toContain('label="按入口 Key 筛选"')
    expect(src).toContain('label="按账号分组筛选"')
    expect(src).toContain('label="按状态筛选"')
    expect(src).toContain('label="按错误类型筛选"')
    expect(src).toContain('aria-label={label}')
    expect(src).toContain('aria-pressed={onlyFailed}')
  })

  test('table header is scoped, captioned and sticky', async () => {
    const src = await read('trace-log-page.tsx')

    expect(src).toContain('scope="col"')
    expect(src).toContain('<caption className="sr-only">')
    expect(src).toContain('sticky top-0')
    // 首列只有一个展开图标，需要给读屏一个列名。
    expect(src).toContain('<span className="sr-only">展开详情</span>')
  })

  test('loading shows a skeleton that mirrors the table, not bare text', async () => {
    const src = await read('trace-log-page.tsx')

    expect(src).toContain('<TraceTableSkeleton />')
    expect(src).toContain('animate-pulse')
    expect(src).toContain('正在加载请求日志…')
    expect(src).not.toContain('<div className="p-6 text-sm text-muted-foreground">加载中…</div>')
  })
})

describe('global theming contract', () => {
  test('color-scheme is declared so native controls follow the theme', async () => {
    const css = await readFile(new URL('../index.css', import.meta.url), 'utf8')

    // 缺 color-scheme 时暗色下的滚动条和 <input type="date"> 仍是亮色。
    expect(css).toContain('color-scheme: light')
    expect(css).toContain('color-scheme: dark')
  })

  test('reduced motion has a global fallback, not just per-animation opt-ins', async () => {
    const css = await readFile(new URL('../index.css', import.meta.url), 'utf8')

    expect(css).toContain('@media (prefers-reduced-motion: reduce)')
    expect(css).toContain('animation-duration: 0.01ms !important')
    expect(css).toContain('transition-duration: 0.01ms !important')
  })

  test('shared primitives list transition properties instead of using transition-all', async () => {
    const button = await read('ui/button.tsx')
    const input = await read('ui/input.tsx')
    const card = await read('ui/card.tsx')

    for (const src of [button, input, card]) {
      expect(src).toContain('transition-[')
    }
    // 注释里会提到这个词，所以只断言不存在作为 class 出现的形式。
    expect(button).not.toContain(' transition-all ')
    expect(input).not.toContain(' transition-all ')
    expect(card).not.toContain(' transition-all ')
  })
})
