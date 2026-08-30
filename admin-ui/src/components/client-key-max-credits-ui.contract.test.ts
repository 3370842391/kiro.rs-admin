import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(new URL(`../${path}`, import.meta.url), 'utf8')
}

describe('client key maxCredits UI wiring', () => {
  test('创建和编辑对话框都暴露积分上限输入', async () => {
    const page = await readSource('components/client-keys-page.tsx')
    expect(page).toContain('create-max-credits')
    expect(page).toContain('edit-max-credits')
    expect(page).toContain('parseMaxCredits')
    expect(page).toContain('useSetClientKeyMaxCredits')
    expect(page).toContain('maxCredits')
  })

  test('列表展示已用积分与上限', async () => {
    const page = await readSource('components/client-keys-page.tsx')
    expect(page).toContain('totalCredits')
    expect(page).toContain('formatCredits')
    expect(page).toContain('>积分<')
  })
})
