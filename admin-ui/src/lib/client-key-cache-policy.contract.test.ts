import { describe, expect, test } from 'bun:test'
import { readFile } from 'node:fs/promises'
import {
  ALLOWED_TTL_SECS,
  BILLING_MODES,
  INHERIT,
  buildClientCachePolicy,
  cachePolicyLabel,
  cachePolicyToForm,
} from './client-key-cache-policy'

async function readRustSource(path: string): Promise<string> {
  // Windows 下 `readFile` 保留 CRLF，而下方断言按 `\n` 写。归一化行尾，
  // 否则换行断言的 `\n` 永远匹配不上 `\r\n`，与 Rust 源码内容无关。
  const source = await readFile(new URL(`../../../src/${path}`, import.meta.url), 'utf8')
  return source.replace(/\r\n/g, '\n')
}

/**
 * 前端这两张表是 Rust 常量的副本。副本一旦脱节，UI 会给出一个必然被后端拒绝的
 * 选项，或者反过来藏掉一个后端已支持的取值——`kiro-drop` 供应商就是这么坏过一次
 * （后端加了区域支持，前端硬编码表还是 `['omit']`，功能等于没上）。
 * 所以这里直接读 Rust 源码比对，而不是再抄一份期望值。
 */
describe('per-key 缓存策略与 Rust 定义保持同步', () => {
  test('TTL 选项逐项等于 ALLOWED_TTL_SECS', async () => {
    const rust = await readRustSource('anthropic/cache_metering.rs')
    const match = rust.match(/pub const ALLOWED_TTL_SECS: \[u64; \d+\] = \[([^\]]+)\]/)
    expect(match).not.toBeNull()

    const fromRust = match![1].split(',').map((item) => Number(item.trim())).filter((n) => !Number.isNaN(n))
    expect(fromRust.length).toBeGreaterThan(0)
    expect([...ALLOWED_TTL_SECS]).toEqual(fromRust)
  })

  test('计费口径取值逐项等于 CacheBillingMode 的 serde 变体', async () => {
    const rust = await readRustSource('admin/client_keys.rs')
    const block = rust.match(/pub enum CacheBillingMode \{([\s\S]*?)\n\}/)
    expect(block).not.toBeNull()

    // 该 enum 标了 #[serde(rename_all = "camelCase")]，单词变体即小写形式。
    expect(rust).toMatch(/#\[serde\(rename_all = "camelCase"\)\]\npub enum CacheBillingMode/)
    const variants = [...block![1].matchAll(/^\s{4}([A-Z][A-Za-z0-9]*),/gm)].map(
      (m) => m[1][0].toLowerCase() + m[1].slice(1),
    )
    expect(variants).toEqual([...BILLING_MODES])
  })

  test('Rust 侧默认口径是 exclusive，前端说明文字不能反过来讲', async () => {
    const rust = await readRustSource('admin/client_keys.rs')
    // #[default] 紧跟在默认变体之前
    expect(rust).toMatch(/#\[default\]\n\s*Exclusive,/)
  })
})

describe('表单与请求体互转', () => {
  test('两项都继承时返回空对象，而不是 undefined', () => {
    // 空对象在后端是"两项都恢复继承全局"；undefined 是"不动"。
    // 用户从自定义改回继承后点保存，必须真的改回去。
    expect(buildClientCachePolicy({ billingMode: INHERIT, ttl: INHERIT })).toEqual({})
  })

  test('只改口径时不捎带 TTL', () => {
    expect(buildClientCachePolicy({ billingMode: 'legacy', ttl: INHERIT })).toEqual({
      billingMode: 'legacy',
    })
  })

  test('只改 TTL 时不捎带口径，且送出数字而非字符串', () => {
    const policy = buildClientCachePolicy({ billingMode: INHERIT, ttl: '300' })
    expect(policy).toEqual({ defaultTtlSecs: 300 })
    expect(typeof policy.defaultTtlSecs).toBe('number')
  })

  test('往返不丢字段', () => {
    const original = { billingMode: 'legacy' as const, defaultTtlSecs: 3600 }
    expect(buildClientCachePolicy(cachePolicyToForm(original))).toEqual(original)
  })

  test('后端省略 cachePolicy 时表单落在继承', () => {
    expect(cachePolicyToForm(undefined)).toEqual({ billingMode: INHERIT, ttl: INHERIT })
  })
})

describe('列表展示', () => {
  test('未配置显示继承全局', () => {
    expect(cachePolicyLabel(undefined)).toBe('继承全局')
    expect(cachePolicyLabel({})).toBe('继承全局')
  })

  test('部分配置只显示配了的那项', () => {
    expect(cachePolicyLabel({ billingMode: 'legacy' })).toBe('同行口径')
    expect(cachePolicyLabel({ defaultTtlSecs: 300 })).toBe('TTL 5 分钟')
  })

  test('两项都配置时并列显示', () => {
    expect(cachePolicyLabel({ billingMode: 'exclusive', defaultTtlSecs: 3600 })).toBe(
      '优化互斥 · TTL 1 小时',
    )
  })
})

/**
 * 语义正确但没接到界面上，用户就调不了——这正是本次要交付的东西。
 * 所以把"编辑对话框确实渲染了这两个选择器、提交时确实带上 cachePolicy"也测住。
 */
describe('编辑对话框接线', () => {
  test('渲染计费口径与 TTL 选择器，并在提交时带上 cachePolicy', async () => {
    const page = await readFile(
      new URL('../components/client-keys-page.tsx', import.meta.url),
      'utf8',
    )
    expect(page).toContain('aria-label="计费口径"')
    expect(page).toContain('aria-label="缓存 TTL"')
    expect(page).toContain('buildClientCachePolicy')
    expect(page).toContain('cachePolicyToForm(item.cachePolicy)')
    expect(page).toContain('cachePolicyLabel(k.cachePolicy)')
  })

  test('选项由常量渲染，不再抄一份硬编码列表', async () => {
    const page = await readFile(
      new URL('../components/client-keys-page.tsx', import.meta.url),
      'utf8',
    )
    expect(page).toContain('BILLING_MODES.map')
    expect(page).toContain('ALLOWED_TTL_SECS.map')
  })

  test('表头列数与数据行列数一致', async () => {
    const page = await readFile(
      new URL('../components/client-keys-page.tsx', import.meta.url),
      'utf8',
    )
    const headers = page.match(/<th\b/g)?.length ?? 0
    const cells = page.match(/<td\b/g)?.length ?? 0
    // 新增一列必须表头和单元格同时加，否则整张表往左错位。
    expect(headers).toBe(cells)
  })
})
