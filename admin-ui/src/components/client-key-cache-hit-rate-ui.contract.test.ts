import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8')
}

describe('client key cache hit-rate UI wiring', () => {
  test('创建和编辑对话框都暴露继承/自定义与 min/max 输入', async () => {
    const page = await readSource('components/client-keys-page.tsx')
    expect(page).toContain('createCacheHitRateMode')
    expect(page).toContain('editCacheHitRateMode')
    expect(page).toContain('create-cache-hit-rate-min')
    expect(page).toContain('create-cache-hit-rate-max')
    expect(page).toContain('edit-cache-hit-rate-min')
    expect(page).toContain('edit-cache-hit-rate-max')
    expect(page).toContain('buildClientKeyCacheHitRatePatch')
  })

  test('列表展示继承全局或自定义范围', async () => {
    const page = await readSource('components/client-keys-page.tsx')
    expect(page).toContain('cacheHitRateLabel')
    expect(page).toContain('继承全局')
  })
})
