import { describe, expect, test } from 'bun:test'
import {
  buildClientKeyCacheHitRatePatch,
  parseClientKeyCacheHitRate,
} from './client-key-cache-hit-rate'

describe('client key cache hit-rate policy', () => {
  test('继承全局时生成 inherit patch', () => {
    expect(buildClientKeyCacheHitRatePatch({ mode: 'inherit', minPct: '', maxPct: '' })).toEqual({
      mode: 'inherit',
    })
  })

  test('自定义模式保留 0/0 并生成 custom patch', () => {
    expect(buildClientKeyCacheHitRatePatch({ mode: 'custom', minPct: '0', maxPct: '90' })).toEqual({
      mode: 'custom',
      minPct: 0,
      maxPct: 90,
    })
  })

  test('拒绝越界、小数、空值和下限大于上限', () => {
    expect(parseClientKeyCacheHitRate('', '90')).toEqual({ ok: false, message: '请输入最小缓存命中率' })
    expect(parseClientKeyCacheHitRate('1.5', '90').ok).toBe(false)
    expect(parseClientKeyCacheHitRate('-1', '90').ok).toBe(false)
    expect(parseClientKeyCacheHitRate('101', '90').ok).toBe(false)
    expect(parseClientKeyCacheHitRate('90', '50')).toEqual({
      ok: false,
      message: '最小缓存命中率不能大于最大缓存命中率',
    })
  })
})
