import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readAppSource(): Promise<string> {
  return readFile(new URL('../App.tsx', import.meta.url), 'utf8')
}

describe('admin header responsive layout contract', () => {
  test('keeps the compact two-row header until the full controls fit', async () => {
    const app = await readAppSource()

    expect(app).toContain('rounded-full border border-border/60 p-0.5 2xl:flex')
    expect(app).toContain('className="2xl:hidden"')
    expect(app).toContain('hidden items-center gap-1 2xl:flex')
    expect(app).toContain('bg-border/70 2xl:inline-block')
    expect(app).toContain('hidden 2xl:inline-flex')
    expect(app).toContain('overflow-x-auto px-3 pb-2 2xl:hidden')
  })
})
