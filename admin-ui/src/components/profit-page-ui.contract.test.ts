import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8')
}

describe('NewAPI 利润报表 UI 接线', () => {
  test('页面包含脱敏配置、时间范围、利润指标和亏损警示', async () => {
    const page = await readSource('components/profit-page.tsx')
    expect(page).toContain('tokenConfigured')
    expect(page).toContain('0.0225')
    expect(page).toContain('30 分钟')
    expect(page).toContain('2 小时')
    expect(page).toContain('24 小时')
    expect(page).toContain('7 天')
    expect(page).toContain('收入')
    expect(page).toContain('上游 Credits')
    expect(page).toContain('成本')
    expect(page).toContain('利润')
    expect(page).toContain('毛利率')
    expect(page).toContain('匹配率')
    expect(page).toContain('text-destructive')
    expect(page).toContain('未匹配收入')
    expect(page).toContain('缺失成本')
  })

  test('API 使用专用端点且空 Token 不会覆盖服务端密钥', async () => {
    const source = await readSource('api/profit.ts')
    expect(source).toContain('/config/profit')
    expect(source).toContain('/profit/report')
    expect(source).toContain('newapiToken.trim()')
    expect(source).toContain('undefined')
  })

  test('应用注册利润导航页', async () => {
    const app = await readSource('App.tsx')
    expect(app).toContain('key: "profit"')
    expect(app).toContain('<ProfitPage />')
  })
})
