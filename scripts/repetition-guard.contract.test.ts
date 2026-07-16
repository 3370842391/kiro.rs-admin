import { expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

test('realtime and buffered streams stop upstream after repetition guard trips', async () => {
  const source = await readFile('src/anthropic/handlers.rs', 'utf8')

  expect(source.match(/ctx\.repetition_guard_tripped\(\)/g)?.length ?? 0).toBeGreaterThanOrEqual(2)
  expect(source).toContain('upstream_repetition_guard')
  expect(source).toContain('upstream repetition guard ended realtime stream')
  expect(source).toContain('upstream repetition guard ended buffered stream')
  expect(source.match(/break AttemptTermination::Eof/g)?.length ?? 0).toBeGreaterThanOrEqual(2)
})
