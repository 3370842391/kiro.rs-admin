import type { ClientResponseMode } from '@/types/api'

export const DEFAULT_CLIENT_RESPONSE_MODE: ClientResponseMode = 'detection'

export function responseModeLabel(mode: ClientResponseMode): string {
  return mode === 'kiro_native' ? 'Kiro 原生' : 'Claude 兼容'
}

export function responseModeDescription(mode: ClientResponseMode): string {
  return mode === 'kiro_native'
    ? '保留工具、重试、SSE、缓存与计费兼容，助手保留 Kiro/AWS 原始身份。'
    : '启用 Claude/Anthropic 身份归一化和检测型确定性回复。'
}

export function responseModeSwitchWarning(
  before: ClientResponseMode,
  after: ClientResponseMode,
): string | null {
  if (before === after) return null
  return after === 'kiro_native'
    ? '后续回复可能出现 Kiro/AWS 身份，检测站得分可能下降；正在进行的请求不受影响。'
    : '后续助手文本可能归一化为 Claude/Anthropic；正在进行的请求不受影响。'
}
