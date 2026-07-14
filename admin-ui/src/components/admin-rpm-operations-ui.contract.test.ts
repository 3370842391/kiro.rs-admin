import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'

async function readSource(path: string): Promise<string> {
  return readFile(path, 'utf8').catch(() => '')
}

describe('admin RPM operations UI wiring', () => {
  test('batch dialog submits one batch request with RPM editing enabled', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')

    expect(dialog).toContain('useBatchUpdateCredentials')
    expect(dialog).toContain('buildBatchUpdateRequest')
    expect(dialog).toContain('editRpm')
    expect(dialog).toContain('rpmLimitDraft')
    expect(dialog).toContain('.mutateAsync(')
    expect(dialog).toMatch(/<form[^>]*onSubmit=[^>]*noValidate/)
    expect(dialog).not.toMatch(/\bupdateCredential\b/)
    expect(dialog).not.toContain('computeGroups')
    expect(dialog).not.toMatch(/for\s*\([^)]*credentials\.length/)
  })

  test('batch dialog keeps selection and dialog open on failure', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')
    const catchBody = dialog.match(/catch\s*\([^)]*\)\s*\{([\s\S]*?)\n\s*\}\s*finally/)?.[1]

    expect(catchBody).toBeDefined()
    expect(catchBody).not.toContain('onDone')
    expect(catchBody).not.toContain('onOpenChange(false)')
  })

  test('dashboard derives selection and request totals from all current credentials', async () => {
    const dashboard = await readSource('src/components/dashboard.tsx')

    expect(dashboard).toContain('RpmStatusBar')
    expect(dashboard).toMatch(/totalInFlight\s*\(\s*data(?:\?)?\.credentials\s*\)/)
    expect(dashboard).toContain('data.rpmSummary')
    expect(dashboard).toContain('selectedCredentials')
    expect(dashboard).toMatch(/credentials[^;]*\.filter\s*\([^;]*selectedIds\.has/s)
    expect(dashboard).toContain('批量编辑')
  })

  test('status bar exposes finite and unlimited rolling-window capacity', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    expect(status).toContain('RpmSummary')
    expect(status).toContain('remainingLimitedCapacity')
    expect(status).toContain('unlimitedAccounts')
    expect(status).toContain('saturatedAccounts')
    expect(status).toContain('totalInFlight')
    expect(status).toContain('grid-cols-2')
    expect(status).toContain('sm:grid-cols-5')
  })

  test('credential cards show rolling RPM load and in-flight work', async () => {
    const card = await readSource('src/components/credential-card.tsx')

    expect(card).toContain('rpmLoadState')
    expect(card).toContain('credential.inFlight')
    expect(card).toContain('最近60秒滚动窗口')
    expect(card).toContain('已满载')
    expect(card).toContain('不限速')
    expect(card).toContain('进行中')
  })
})
