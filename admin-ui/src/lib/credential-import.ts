export function unwrapCredentialImportPayload(parsed: unknown): unknown[] {
  if (Array.isArray(parsed)) return parsed
  if (!parsed || typeof parsed !== 'object') {
    throw new Error('无法识别的 JSON 格式')
  }
  const obj = parsed as Record<string, unknown>
  if (Array.isArray(obj.accounts)) return obj.accounts
  if (Array.isArray(obj.credentials)) return obj.credentials
  if (
    (obj.credentials && typeof obj.credentials === 'object') ||
    typeof obj.refreshToken === 'string' ||
    typeof obj.refresh_token === 'string' ||
    typeof obj.kiroApiKey === 'string' ||
    typeof obj.kiro_api_key === 'string'
  ) {
    return [obj]
  }
  throw new Error(
    '无法识别的导入格式：请粘贴凭据对象、数组、完整 credentials JSON 或 Kiro Account Manager accounts JSON',
  )
}
