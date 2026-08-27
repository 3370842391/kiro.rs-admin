import { describe, expect, test } from 'bun:test'
import {
  ERROR_TYPE_OPTIONS,
  interruptedStatusHint,
  isClientDisconnect,
  isUpstreamStreamBreak,
  outcomeStyle,
} from './trace-outcome'

describe('trace outcome labels', () => {
  test('does not treat client disconnect as an upstream stream break', () => {
    expect(isClientDisconnect('client_disconnected')).toBe(true)
    expect(isUpstreamStreamBreak('client_disconnected')).toBe(false)
    expect(isUpstreamStreamBreak('stream_idle_timeout')).toBe(true)
    expect(isUpstreamStreamBreak('stream_read_error')).toBe(true)
    expect(isUpstreamStreamBreak('stream_interrupted')).toBe(true)
    expect(interruptedStatusHint('client_disconnected')).toBe('客户端断开（不是上游断流）')
    expect(interruptedStatusHint('stream_idle_timeout')).toBe('上游断流')
    expect(interruptedStatusHint(undefined)).toContain('含客户端断开')
  })

  test('keeps final_status interrupted wording separate from error type', () => {
    expect(outcomeStyle('client_disconnected').label).toBe('客户端断开')
    expect(outcomeStyle('stream_interrupted').label).toBe('上游断流')
    expect(outcomeStyle('payload_limit_exceeded').label).toBe('请求体超限')
    expect(outcomeStyle('tool_schema_retry_exhausted').label).toBe('工具 schema 重试仍失败')
    expect(outcomeStyle('proxy_pool_empty').label).toBe('代理候选空')
  })

  test('filter list can isolate upstream breaks from disconnects', () => {
    const values = ERROR_TYPE_OPTIONS.map((option) => option.value)
    expect(values).toContain('client_disconnected')
    expect(values).toContain('stream_idle_timeout')
    expect(values).toContain('payload_limit_exceeded')
  })
})
