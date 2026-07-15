import { describe, expect, test } from 'bun:test'
import {
  DEFAULT_CLIENT_RESPONSE_MODE,
  responseModeDescription,
  responseModeLabel,
  responseModeSwitchWarning,
} from './client-key-response-mode'

describe('client key response mode', () => {
  test('defaults new keys to detection', () => {
    expect(DEFAULT_CLIENT_RESPONSE_MODE).toBe('detection')
  })

  test('renders stable labels and descriptions', () => {
    expect(responseModeLabel('detection')).toBe('Claude 兼容')
    expect(responseModeLabel('kiro_native')).toBe('Kiro 原生')
    expect(responseModeDescription('kiro_native')).toContain('保留工具')
    expect(responseModeDescription('kiro_native')).toContain('Kiro/AWS')
  })

  test('only warns when mode changes', () => {
    expect(responseModeSwitchWarning('detection', 'detection')).toBeNull()
    expect(responseModeSwitchWarning('detection', 'kiro_native')).toContain('检测站得分')
    expect(responseModeSwitchWarning('kiro_native', 'detection')).toContain('Claude/Anthropic')
  })
})
