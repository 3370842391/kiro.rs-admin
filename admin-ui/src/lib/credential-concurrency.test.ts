import { describe, expect, test } from 'bun:test'
import {
  concurrencyFillRatio,
  concurrencyHint,
  concurrencyTone,
} from './credential-concurrency'

describe('concurrencyTone', () => {
  test('0 与非法值都是空闲', () => {
    expect(concurrencyTone(0)).toBe('idle')
    expect(concurrencyTone(-3)).toBe('idle')
    expect(concurrencyTone(Number.NaN)).toBe('idle')
  })

  test('亲和软上限内是 active，之上分级升温', () => {
    expect(concurrencyTone(1)).toBe('active')
    expect(concurrencyTone(2)).toBe('active')
    expect(concurrencyTone(3)).toBe('busy')
    expect(concurrencyTone(5)).toBe('busy')
    expect(concurrencyTone(6)).toBe('hot')
  })
})

describe('concurrencyHint', () => {
  test('空闲态说明无请求', () => {
    expect(concurrencyHint(0)).toContain('没有请求')
  })

  test('软上限内不加告警后缀', () => {
    expect(concurrencyHint(2)).toBe('当前有 2 个请求正在这个账号上执行')
  })

  test('超过软上限与明显被压给出不同提示', () => {
    expect(concurrencyHint(4)).toContain('会话亲和软上限')
    expect(concurrencyHint(9)).toContain('明显被压')
  })
})

describe('concurrencyFillRatio', () => {
  test('夹在 0–1 之间', () => {
    expect(concurrencyFillRatio(0)).toBe(0)
    expect(concurrencyFillRatio(-1)).toBe(0)
    expect(concurrencyFillRatio(4)).toBeCloseTo(0.5)
    expect(concurrencyFillRatio(8)).toBe(1)
    expect(concurrencyFillRatio(50)).toBe(1)
  })
})
