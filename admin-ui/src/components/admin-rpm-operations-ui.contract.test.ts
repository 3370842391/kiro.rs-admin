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

  test('batch dialog exposes RPM validation inline and focuses the invalid input', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')

    expect(dialog).toContain('rpmError')
    expect(dialog).toContain('rpmInputRef')
    expect(dialog).toContain('aria-invalid')
    expect(dialog).toContain('aria-describedby')
    expect(dialog).toContain('batch-rpm-limit-error')
    expect(dialog).toContain('id="batch-rpm-limit-hint"')
    expect(dialog).toMatch(
      /aria-describedby=\{[\s\S]*?'batch-rpm-limit-hint batch-rpm-limit-error'[\s\S]*?'batch-rpm-limit-hint'[\s\S]*?\}/,
    )
    expect(dialog).toContain('rpmInputRef.current?.focus()')
  })

  test('batch dialog exposes group mode as one named and described pressed-button group', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')

    expect(dialog).toContain('id="batch-group-mode-label"')
    expect(dialog).toMatch(
      /<div[^>]*role="group"[^>]*aria-labelledby="batch-group-mode-label"[^>]*aria-describedby="batch-group-mode-description"/s,
    )
    expect(dialog).toContain('aria-pressed={mode === item.value}')
    expect(dialog).toContain('id="batch-group-mode-description"')
  })

  test('batch dialog separates HTTP failures from success callbacks', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')
    const catchIndex = dialog.indexOf('} catch (error) {')
    const finallyIndex = dialog.indexOf('} finally {', catchIndex)
    const successIndex = dialog.indexOf('toast.success', catchIndex)
    const closeIndex = dialog.indexOf('onOpenChange(false)', catchIndex)
    const doneIndex = dialog.indexOf('onDone()', catchIndex)

    expect(catchIndex).toBeGreaterThan(-1)
    expect(finallyIndex).toBeGreaterThan(catchIndex)
    expect(successIndex).toBeGreaterThan(finallyIndex)
    expect(closeIndex).toBeGreaterThan(finallyIndex)
    expect(doneIndex).toBeGreaterThan(finallyIndex)
  })

  test('batch dialog provides mobile touch targets and input metadata', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')

    expect(dialog.match(/min-h-11/g)?.length ?? 0).toBeGreaterThanOrEqual(3)
    expect(dialog).toContain('h-11 sm:h-8')
    expect(dialog.match(/h-11 sm:h-9/g)?.length ?? 0).toBeGreaterThanOrEqual(4)
    expect(dialog).toContain('min-h-11 [&_button]:h-11 sm:[&_button]:h-9')
    expect(dialog).toMatch(/name="rpmLimit"[^>]*autoComplete="off"/s)
    expect(dialog).toMatch(/name="sourceChannel"[^>]*autoComplete="off"/s)
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

  test('status bar labels unlimited aggregate capacity without contradicting finite capacity', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    expect(status).toContain("hasUnlimitedCapacity ? '总容量' : '有限容量'")
    expect(status).toContain("hasUnlimitedCapacity ? '不限速' : limitedCapacity")
    expect(status).toContain("hasUnlimitedCapacity ? '有限账号剩余' : '剩余'")
    expect(status).toContain('有限账号容量 ${limitedCapacity}')
    expect(status).toContain('不限速账号 ${unlimitedAccounts}')
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

  test('credential cards show warning text and reserve enough list width for maximum RPM', async () => {
    const card = await readSource('src/components/credential-card.tsx')
    const listRpm = card.match(
      /<div className="([^"]*)">\s*<div className="[^"]*">\s*RPM\s*<\/div>\s*<div\s*className=\{`([^`]*)`\}/,
    )

    expect(card).toContain('接近满载')
    expect(listRpm).not.toBeNull()
    expect(listRpm?.[1]).toMatch(/\bw-(24|28)\b/)
    expect(listRpm?.[1]).toContain('min-w-0')
    expect(listRpm?.[2]).toContain('text-xs')
    expect(listRpm?.[2]).toContain('break-words')
  })
})
