import { expect, test } from 'bun:test'

test('auto continue settings are persisted through endpoint configuration', async () => {
  const api = await Bun.file('src/api/credentials.ts').text()
  const dialog = await Bun.file('src/components/endpoint-chains-dialog.tsx').text()

  expect(api).toContain('autoContinueEnabled')
  expect(api).toContain('autoContinueMax')
  expect(api).toContain('partialStreamRecoveryEnabled')
  expect(api).toContain('partialStreamRecoveryWindowMs')
  expect(dialog).toContain('启用纯文本自动续写')
  expect(dialog).toContain('恢复可疑半截流')
  expect(dialog).toContain('可能增加上游调用次数、总耗时和费用')
  expect(dialog).toContain('不会续写工具调用、空流、复读熔断或显式错误')
  expect(dialog).toContain('sm:flex-row')
})
