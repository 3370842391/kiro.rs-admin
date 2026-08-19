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
    expect(page).toContain('归属率')
    expect(page).toContain('text-destructive')
    expect(page).toContain('未匹配收入')
    expect(page).toContain('未归属收入')
    expect(page).toContain('未归属 Credits')
    expect(page).toContain('未归属成本')
    expect(page).toContain('顶部总成本来自 RS 实际 metering 账本')
    expect(page).toContain('范围未确认')
    expect(page).toContain('ledgerScopeConfirmed')
    expect(await readSource('types/api.ts')).toContain('unattributedCost')
  })

  test('API 使用专用端点且空 Token 不会覆盖服务端密钥', async () => {
    const source = await readSource('api/profit.ts')
    expect(source).toContain('/config/profit')
    expect(source).toContain('/profit/report')
    expect(source).toContain('/pricing/coefficients')
    expect(source).toContain('/pricing/simulate')
    expect(source).toContain('newapiToken.trim()')
    expect(source).toContain('undefined')
  })

  test('进价测算弹窗能正算倍率、反算毛利，并在系数缺失时给出提示', async () => {
    const page = await readSource('components/profit-page.tsx')
    const dialog = await readSource('components/pricing-calculator-dialog.tsx')
    expect(page).toContain('PricingCalculatorDialog')
    expect(page).toContain('pricing-coefficients')
    expect(dialog).toContain('进价测算')
    expect(dialog).toContain('目标毛利率')
    expect(dialog).toContain('额度能跑到')
    expect(dialog).toContain('回本倍率')
    expect(dialog).toContain('可产出 token')
    expect(dialog).toContain('还没有实测系数')
    expect(await readSource('types/api.ts')).toContain('breakevenGroupRatio')
  })

  test('应用注册利润导航页', async () => {
    const app = await readSource('App.tsx')
    expect(app).toContain('key: "profit"')
    expect(app).toContain('<ProfitPage />')
  })
})
