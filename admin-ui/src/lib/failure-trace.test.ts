import { describe, expect, test } from 'bun:test'
import type { TraceAttempt, TraceRecord } from '@/types/api'
import {
  compactAttemptLabel,
  failureDisposition,
  failureDispositionLabel,
  sortTraceAttempts,
} from './failure-trace'

function attempt(
  index: number,
  credentialId: number,
  endpoint: string,
  outcome: string,
  httpStatus: number | null,
): TraceAttempt {
  return {
    attempt: index,
    credentialId,
    endpoint,
    outcome,
    httpStatus,
    email: null,
    errorSnippet: outcome === 'success' ? null : 'upstream error',
    durationMs: 20,
  }
}

function record(overrides: Partial<TraceRecord> = {}): TraceRecord {
  return {
    traceId: 'trace-1',
    ts: '2026-07-16T00:00:00Z',
    keyId: 1,
    keySource: 'clientKey',
    keyName: 'newapi',
    responseMode: 'detection',
    model: 'claude-opus-4-8',
    isStream: true,
    finalStatus: 'error',
    finalCredentialId: 202,
    errorType: 'account_throttled',
    errorMessage: null,
    totalAttempts: 0,
    durationMs: 40,
    interruptedAfterBytes: null,
    attempts: [],
    ...overrides,
  }
}

describe('sortTraceAttempts', () => {
  test('按 attempt 升序返回副本且不修改查询缓存数组', () => {
    const original = [
      attempt(2, 203, 'ide', 'success', 200),
      attempt(0, 202, 'ide', 'account_throttled', 429),
      attempt(1, 202, 'runtime', 'account_throttled', 429),
    ]

    expect(sortTraceAttempts(original).map((item) => item.attempt)).toEqual([0, 1, 2])
    expect(original.map((item) => item.attempt)).toEqual([2, 0, 1])
  })
})

describe('failureDisposition', () => {
  test('区分同账号换端点、同端点重试和换账号成功', () => {
    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 202,
          attempts: [
            attempt(0, 202, 'ide', 'account_throttled', 429),
            attempt(1, 202, 'runtime', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('switched_endpoint')

    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 202,
          attempts: [
            attempt(0, 202, 'ide', 'transient', 503),
            attempt(1, 202, 'ide', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('retried_same_endpoint')

    expect(
      failureDisposition(
        record({
          finalStatus: 'success',
          finalCredentialId: 203,
          attempts: [
            attempt(0, 202, 'ide', 'account_throttled', 429),
            attempt(1, 203, 'ide', 'success', 200),
          ],
        }),
        202,
      ),
    ).toBe('switched_credential')
  })

  test('区分最终失败、流式中断和未到达上游', () => {
    expect(
      failureDisposition(
        record({ attempts: [attempt(0, 202, 'ide', 'bad_request', 400)] }),
        202,
      ),
    ).toBe('failed')

    expect(
      failureDisposition(
        record({
          finalStatus: 'interrupted',
          attempts: [attempt(0, 202, 'ide', 'stream_interrupted', 200)],
        }),
        202,
      ),
    ).toBe('interrupted')

    expect(failureDisposition(record(), 202)).toBe('not_sent')
  })

  test('每种分类都有稳定中文结论', () => {
    expect(failureDispositionLabel('switched_endpoint')).toBe('同账号切换端点后成功')
    expect(failureDispositionLabel('retried_same_endpoint')).toBe('同账号重试后成功')
    expect(failureDispositionLabel('switched_credential')).toBe('切换其他账号后成功')
    expect(failureDispositionLabel('interrupted')).toBe('流式响应中断')
    expect(failureDispositionLabel('failed')).toBe('最终失败')
    expect(failureDispositionLabel('not_sent')).toBe('请求未到达上游')
  })
})

describe('compactAttemptLabel', () => {
  test('紧凑标签明确账号、端点和 HTTP/网络结果', () => {
    expect(compactAttemptLabel(attempt(0, 202, 'ide', 'account_throttled', 429)))
      .toBe('#202 / ide 429')
    expect(compactAttemptLabel(attempt(1, 202, '', 'network_error', null)))
      .toBe('#202 / 未知端点 网络错误')
  })
})
