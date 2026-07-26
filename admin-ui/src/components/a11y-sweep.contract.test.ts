import { describe, expect, test } from 'bun:test'
import { readdir, readFile } from 'node:fs/promises'
import { fileURLToPath } from 'node:url'
import { join } from 'node:path'

/**
 * 全仓扫描型契约：单点断言挡不住「新加一个按钮忘了写 aria-label」。
 *
 * 这里按 JSX 元素边界切分再判断，而不是「匹配行往后数 N 行」——后者会漏掉
 * 把 aria-label 写在第 6 行之后的写法，第一版扫描就因此误报了 model-profiles-dialog。
 */
const COMPONENTS_DIR = fileURLToPath(new URL('.', import.meta.url))

async function collectTsx(dir: string): Promise<string[]> {
  const out: string[] = []
  for (const entry of await readdir(dir, { withFileTypes: true })) {
    const full = join(dir, entry.name)
    if (entry.isDirectory()) out.push(...(await collectTsx(full)))
    else if (entry.name.endsWith('.tsx')) out.push(full)
  }
  return out
}

/** 取出起始标签的完整文本（到第一个非 `=>` 的 `>` 为止）。 */
function openingTag(source: string, start: number): string {
  let i = start
  while (i < source.length && !(source[i] === '>' && source[i - 1] !== '=')) i += 1
  return source.slice(start, i + 1)
}

interface Finding {
  file: string
  kind: string
  line: number
}

async function findUnnamedControls(): Promise<Finding[]> {
  const findings: Finding[] = []
  for (const file of await collectTsx(COMPONENTS_DIR)) {
    if (file.endsWith('.test.tsx')) continue
    const source = await readFile(file, 'utf8')
    // <label htmlFor="x"> 能给 <button id="x"> 命名——button 是 labelable 元素。
    const labelledIds = new Set(
      [...source.matchAll(/htmlFor="([^"]+)"/g)].map((m) => m[1]),
    )
    for (const match of source.matchAll(/<(Button|SelectTrigger)\b/g)) {
      const tag = openingTag(source, match.index)
      const kind = match[1]
      // 只管纯图标按钮：带可见文字的按钮自带无障碍名称。
      if (kind === 'Button' && !tag.includes('size="icon"')) continue
      if (tag.includes('aria-label')) continue
      const id = /id="([^"]+)"/.exec(tag)?.[1]
      if (id && labelledIds.has(id)) continue
      findings.push({
        file: file.slice(COMPONENTS_DIR.length),
        kind,
        line: source.slice(0, match.index).split('\n').length,
      })
    }
  }
  return findings
}

describe('repo-wide accessibility sweep', () => {
  test('no icon-only button or select trigger ships without an accessible name', async () => {
    const findings = await findUnnamedControls()
    const report = findings
      .map((f) => `${f.file}:${f.line} <${f.kind}>`)
      .join('\n')
    expect(findings.length, `以下控件缺无障碍名称：\n${report}`).toBe(0)
  })

  test('no component animates with transition-all', async () => {
    const offenders: string[] = []
    for (const file of await collectTsx(COMPONENTS_DIR)) {
      const source = await readFile(file, 'utf8')
      for (const [index, line] of source.split('\n').entries()) {
        // 只看 className 里的实际用法，注释里提到这个词是允许的。
        if (line.includes('transition-all') && line.includes('className')) {
          offenders.push(`${file.slice(COMPONENTS_DIR.length)}:${index + 1}`)
        }
      }
    }
    expect(
      offenders.length,
      `transition-all 会把宽高/位移一起动画化，请显式列出属性：\n${offenders.join('\n')}`,
    ).toBe(0)
  })

  test('no text glyphs used where the icon set should be', async () => {
    const offenders: string[] = []
    for (const file of await collectTsx(COMPONENTS_DIR)) {
      const source = await readFile(file, 'utf8')
      for (const [index, line] of source.split('\n').entries()) {
        // ✓ / ✕ 之类的符号不属于 lucide 这套图标集，也拿不到 Button 的 [&_svg] 尺寸约束。
        if (/^\s*[✓✔✕✖×]\s*$/.test(line)) {
          offenders.push(`${file.slice(COMPONENTS_DIR.length)}:${index + 1}`)
        }
      }
    }
    expect(offenders.length, `请改用 lucide 图标：\n${offenders.join('\n')}`).toBe(0)
  })
})
