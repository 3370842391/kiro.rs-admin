import { expect, test } from 'bun:test'

test('cache policy dialog exposes rolling switch, limit and rollback warning', async () => {
  const source = await Bun.file('src/components/cache-policy-dialog.tsx').text()
  expect(source).toContain('cache-rolling-prefix-enabled')
  expect(source).toContain('cache-rolling-prefix-limit')
  expect(source).toContain('rollingPrefixEnabled')
  expect(source).toContain('rollingPrefixLimit')
  expect(source).toContain('已恢复旧的全历史前缀算法')
  expect(source).toContain('segmentLookups')
  expect(source).toContain('segmentHits')
  expect(source).toContain('evictions')
})
