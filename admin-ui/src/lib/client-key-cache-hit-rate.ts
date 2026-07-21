export type ClientKeyCacheHitRateMode = 'inherit' | 'custom'

export interface ClientKeyCacheHitRateForm {
  mode: ClientKeyCacheHitRateMode
  minPct: string
  maxPct: string
}

export type ClientKeyCacheHitRatePatch =
  | { mode: 'inherit' }
  | { mode: 'custom'; minPct: number; maxPct: number }

export type CacheHitRateParseResult =
  | { ok: true; minPct: number; maxPct: number }
  | { ok: false; message: string }

export function parseClientKeyCacheHitRate(
  minDraft: string,
  maxDraft: string,
): CacheHitRateParseResult {
  if (minDraft.trim() === '') {
    return { ok: false, message: '请输入最小缓存命中率' }
  }
  if (maxDraft.trim() === '') {
    return { ok: false, message: '请输入最大缓存命中率' }
  }

  const isInteger = (value: string) => /^\d+$/.test(value.trim())
  if (!isInteger(minDraft) || !isInteger(maxDraft)) {
    return { ok: false, message: '缓存命中率必须是 0 到 100 的整数' }
  }

  const minPct = Number(minDraft)
  const maxPct = Number(maxDraft)
  if (!Number.isSafeInteger(minPct) || !Number.isSafeInteger(maxPct) || minPct > 100 || maxPct > 100) {
    return { ok: false, message: '缓存命中率必须是 0 到 100 的整数' }
  }
  if (minPct > 0 && maxPct > 0 && minPct > maxPct) {
    return { ok: false, message: '最小缓存命中率不能大于最大缓存命中率' }
  }
  return { ok: true, minPct, maxPct }
}

export function buildClientKeyCacheHitRatePatch(
  form: ClientKeyCacheHitRateForm,
): ClientKeyCacheHitRatePatch {
  if (form.mode === 'inherit') {
    return { mode: 'inherit' }
  }
  const parsed = parseClientKeyCacheHitRate(form.minPct, form.maxPct)
  if (!parsed.ok) {
    throw new Error(parsed.message)
  }
  return { mode: 'custom', minPct: parsed.minPct, maxPct: parsed.maxPct }
}

export function cacheHitRateLabel(
  policy: { minPct: number; maxPct: number } | undefined,
): string {
  return policy ? `${policy.minPct}%–${policy.maxPct}%` : '继承全局'
}
