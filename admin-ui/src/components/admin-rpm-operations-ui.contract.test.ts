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

  test('batch dialog exposes fixed and promoted priority modes', async () => {
    const dialog = await readSource('src/components/batch-edit-credential-dialog.tsx')

    expect(dialog).toContain('editPriority')
    expect(dialog).toContain('priorityMode')
    expect(dialog).toContain('batch-priority-value')
    expect(dialog).toContain('指定数值')
    expect(dialog).toContain('最高优先池')
    expect(dialog).toContain('数字越小优先级越高')
    expect(dialog).toContain('可能承担全部新流量')
    expect(dialog).toContain('priorityAdjusted')
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
    // 断言「窄屏不会挤成一条」这个意图，不锁具体布局实现：
    // 指标已从七个等宽列改为两张卡片，窄屏整卡纵向堆叠而不是把相关指标拆散。
    expect(status).toMatch(/flex-col[\s\S]{0,40}lg:flex-row/)
  })

  test('status bar shows the live credit burn rate next to remaining credits', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    // 余量与速率必须一起看：可用积分 ÷ 每分钟消耗 = 还能撑多久。
    expect(status).toContain("label=\"积分消耗\"")
    expect(status).toContain('creditsPerMinute')
    // 与 RPM 同为 60 秒滑动窗口，因此标成实时量（带呼吸点）
    expect(status).toMatch(/label="积分消耗"[\s\S]{0,200}?live/)
    // 负数或 NaN 不能直接渲染出去
    expect(status).toContain('Math.max(0, creditsPerMinute)')
  })

  test('status bar labels unlimited aggregate capacity without contradicting finite capacity', async () => {
    const status = await readSource('src/components/rpm-status-bar.tsx')

    expect(status).toContain("hasUnlimitedCapacity ? '总容量' : '有限容量'")
    // 不锁类型转换的写法（可能是裸变量也可能是 String(...)），只钉住
    // 「有不限速账号时显示『不限速』，否则显示具体容量数」这个分支语义。
    expect(status).toMatch(/hasUnlimitedCapacity \? '不限速' : [^,\n]*limitedCapacity/)
    // 存在不限速账号时，「剩余」必须显式限定到有限账号，否则会读成「全池只剩这些」。
    // 只钉住 true 分支的限定语，else 分支的措辞可以改。
    expect(status).toMatch(/hasUnlimitedCapacity \? '有限账号剩余' : '[^']+'/)
    // 明细里两类账号都要出现，避免「不限速」把有限容量藏掉
    expect(status).toContain('${limitedCapacity}')
    expect(status).toContain('${unlimitedAccounts}')
  })

  test('credential cards show rolling RPM load and in-flight work', async () => {
    const card = await readSource('src/components/credential-card.tsx')

    expect(card).toContain('rpmLoadState')
    expect(card).toContain('credential.inFlight')
    expect(card).toContain('最近60秒滚动窗口')
    expect(card).toContain('已满载')
    expect(card).toContain('不限速')
    // 并发不再是一个可有可无的徽章，而是常驻计量表
    expect(card).toContain('并发')
    expect(card).toContain('ConcurrencyGauge')
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

  test('RPM load has a visual bar that stays empty for unlimited accounts', async () => {
    const card = await readSource('src/components/credential-card.tsx')

    expect(card).toContain('rpmBarClass')
    expect(card).toContain('rpmFillPercent')
    expect(card).toMatch(/rpmState === "unlimited" \|\| rpmLimit <= 0\s*\?\s*0/)
  })
})
