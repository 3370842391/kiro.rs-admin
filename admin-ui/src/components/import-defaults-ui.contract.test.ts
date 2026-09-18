import { expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

test('批量 JSON 的 priority 统一采用导入默认值', async () => {
  const source = await readFile('src/components/batch-import-dialog.tsx', 'utf8')

  expect(source).toContain('priority: defaultPriority')
  expect(source).not.toContain('priority: cred.priority ?? defaultPriority')
})

test('Hosted 登录明确提示会继承导入默认值', async () => {
  const source = await readFile('src/components/social-login-dialog.tsx', 'utf8')

  expect(source).toContain('导入默认值')
  expect(source).toContain('优先级、RPM、并发、分组和代理设置')
})

test('企业账号选择策略提供遵循优先级选项', async () => {
  const source = await readFile('src/components/endpoint-chains-dialog.tsx', 'utf8')

  expect(source).toContain('enterpriseSelectionPolicy')
  expect(source).toContain('遵循账号优先级')
  expect(source).toContain('企业账号优先')
})
