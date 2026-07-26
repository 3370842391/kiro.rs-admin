import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readAppSource(): Promise<string> {
  return readFile(new URL('../App.tsx', import.meta.url), 'utf8')
}

describe('admin header responsive layout contract', () => {
  test('keeps the compact two-row header until the full controls fit', async () => {
    const app = await readAppSource()

    expect(app).toContain('max-w-[1400px] min-w-0 items-center gap-2 px-3 sm:h-16 sm:px-4 xl:px-8 2xl:max-w-[1600px]')
    expect(app).toContain('rounded-full border border-border/60 p-0.5 2xl:flex')
    expect(app).toContain('className="2xl:hidden"')
    expect(app).toContain('hidden items-center gap-1 2xl:flex')
    expect(app).toContain('bg-border/70 2xl:inline-block')
    expect(app).toContain('hidden 2xl:inline-flex')
    // overscroll-x-contain 是横向 tab 条的一部分：滑到头时不把手势交给浏览器，
    // 避免移动端误触「返回上一页」。断言里保留它，防止以后被顺手删掉。
    expect(app).toContain('overflow-x-auto overscroll-x-contain px-3 pb-2 2xl:hidden')
  })

  test('navigates with real anchors so modified clicks keep working', async () => {
    const app = await readAppSource()

    // tab 必须是 <a href="#/x">：换回 <button onClick> 会让 Cmd/中键点击、
    // 「在新标签页打开」和悬停预览目标地址全部失效。
    expect(app).toContain('<a href={`#/${tab.key}`}')
    expect(app).toContain('aria-current={active ? "page" : undefined}')
    expect(app).not.toContain('onSwitchTab')
  })

  test('exposes landmarks and a skip link for keyboard users', async () => {
    const app = await readAppSource()

    expect(app).toContain('href="#main-content"')
    expect(app).toContain('id="main-content"')
    expect(app).toContain('aria-label="主导航"')
  })
})
