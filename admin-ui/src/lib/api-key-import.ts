export const SUPPORTED_API_REGIONS = ['us-east-1', 'eu-central-1'] as const
const MAX_NICKNAME_CHARS = 128

export type SupportedApiRegion = (typeof SUPPORTED_API_REGIONS)[number]

export interface ParsedApiKeyLine {
  lineNumber: number
  nickname: string
  kiroApiKey: string
  maskedApiKey: string
  apiRegion: SupportedApiRegion
}

export interface ApiKeyLineError {
  lineNumber: number
  nickname: string
  maskedApiKey: string
  apiRegion: string
  maskedLine: string
  message: string
}

export interface ParseApiKeyLinesResult {
  entries: ParsedApiKeyLine[]
  errors: ApiKeyLineError[]
}

export function isSupportedApiRegion(value: string): value is SupportedApiRegion {
  return SUPPORTED_API_REGIONS.some((region) => region === value)
}

export function maskApiKey(value: string): string {
  const trimmed = value.trim()
  if (!trimmed) return '(空)'
  if (!trimmed.startsWith('ksk_')) return '••••'

  const secret = trimmed.slice(4)
  return `ksk_••••${secret.length > 4 ? secret.slice(-4) : ''}`
}

export function redactApiKeys(value: string): string {
  return value.replace(/ksk_[^\s|]+/g, (key) => maskApiKey(key))
}

function createError(
  lineNumber: number,
  columns: string[],
  defaultApiRegion: SupportedApiRegion | undefined,
  message: string,
): ApiKeyLineError {
  const nickname = redactApiKeys(columns[0]?.trim() || '(空)')
  const maskedApiKey = maskApiKey(columns[1] ?? '')
  const apiRegion = redactApiKeys(columns[2]?.trim() || defaultApiRegion || '(未指定)')

  return {
    lineNumber,
    nickname,
    maskedApiKey,
    apiRegion,
    maskedLine: `${nickname} | ${maskedApiKey} | ${apiRegion}`,
    message,
  }
}

export function parseApiKeyLines(
  input: string,
  defaultApiRegion?: SupportedApiRegion,
): ParseApiKeyLinesResult {
  const entries: ParsedApiKeyLine[] = []
  const errors: ApiKeyLineError[] = []
  const firstLineByKey = new Map<string, number>()

  input.split(/\r?\n/).forEach((rawLine, index) => {
    const lineNumber = index + 1
    const trimmedLine = rawLine.trim()
    if (!trimmedLine || trimmedLine.startsWith('#')) return

    const columns = rawLine.split('|').map((column) => column.trim())
    if (columns.length < 2 || columns.length > 3) {
      errors.push(createError(lineNumber, columns, defaultApiRegion, '列数必须为 2 或 3 列'))
      return
    }

    const [nickname, kiroApiKey, regionOverride] = columns
    const apiRegion = regionOverride || defaultApiRegion
    let supportedApiRegion: SupportedApiRegion | undefined
    const messages: string[] = []

    if (!nickname) {
      messages.push('nickname 不能为空')
    } else if (Array.from(nickname).length > MAX_NICKNAME_CHARS) {
      messages.push('nickname 最多 128 个字符')
    }
    if (!kiroApiKey) {
      messages.push('API Key 不能为空')
    } else if (!kiroApiKey.startsWith('ksk_')) {
      messages.push('API Key 必须以 ksk_ 开头')
    }

    if (!apiRegion) {
      messages.push('请选择批次 API Region或在第三列指定')
    } else if (!isSupportedApiRegion(apiRegion)) {
      messages.push('API Region 仅支持 us-east-1 或 eu-central-1')
    } else {
      supportedApiRegion = apiRegion
    }

    if (messages.length > 0) {
      errors.push(createError(lineNumber, columns, defaultApiRegion, messages.join('；')))
      return
    }
    if (!supportedApiRegion) return

    const firstLine = firstLineByKey.get(kiroApiKey)
    if (firstLine !== undefined) {
      errors.push(
        createError(lineNumber, columns, defaultApiRegion, `API Key 与第 ${firstLine} 行重复`),
      )
      return
    }

    firstLineByKey.set(kiroApiKey, lineNumber)
    entries.push({
      lineNumber,
      nickname,
      kiroApiKey,
      maskedApiKey: maskApiKey(kiroApiKey),
      apiRegion: supportedApiRegion,
    })
  })

  return { entries, errors }
}
