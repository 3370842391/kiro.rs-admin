import type {
  BatchCredentialGroupPatch,
  BatchUpdateCredentialsRequest,
} from '@/types/api'

const MAX_RPM_LIMIT = 100_000
const INVALID_RPM_MESSAGE = `RPM 上限必须是 0 到 ${MAX_RPM_LIMIT} 的整数`

export type RpmLimitParseResult =
  | { ok: true; value: number }
  | { ok: false; message: string }

export type RpmLoadState = 'unlimited' | 'normal' | 'warning' | 'saturated'

export interface BatchUpdateInput {
  ids: number[]
  editRpm: boolean
  rpmDraft: string
  editGroups: boolean
  groupMode: BatchCredentialGroupPatch['mode']
  groups: string[]
  editSource: boolean
  sourceChannel: string
}

export type BatchUpdateRequestResult =
  | { ok: true; value: BatchUpdateCredentialsRequest }
  | { ok: false; message: string }

export function parseRpmLimit(draft: string): RpmLimitParseResult {
  const trimmed = draft.trim()
  if (trimmed === '') {
    return { ok: false, message: '请输入 RPM 上限' }
  }

  if (!/^\d+$/.test(trimmed)) {
    return { ok: false, message: INVALID_RPM_MESSAGE }
  }

  const value = Number(trimmed)
  if (!Number.isSafeInteger(value) || value > MAX_RPM_LIMIT) {
    return { ok: false, message: INVALID_RPM_MESSAGE }
  }

  return { ok: true, value }
}

export function rpmLoadState(current: number, limit: number): RpmLoadState {
  if (!Number.isFinite(limit) || limit <= 0) {
    return 'unlimited'
  }

  const safeCurrent = Number.isFinite(current) && current > 0 ? current : 0
  const load = safeCurrent / limit
  if (load >= 1) {
    return 'saturated'
  }
  if (load >= 0.8) {
    return 'warning'
  }
  return 'normal'
}

export function totalInFlight(
  credentials: ReadonlyArray<{ inFlight?: number | null }>,
): number {
  return credentials.reduce((total, credential) => {
    const inFlight = credential.inFlight ?? 0
    return total + (Number.isFinite(inFlight) && inFlight > 0 ? inFlight : 0)
  }, 0)
}

export function buildBatchUpdateRequest(
  input: BatchUpdateInput,
): BatchUpdateRequestResult {
  if (!input.editRpm && !input.editGroups && !input.editSource) {
    return { ok: false, message: '请至少选择一项要修改的内容' }
  }

  const request: BatchUpdateCredentialsRequest = { ids: input.ids }

  if (input.editRpm) {
    const rpmLimit = parseRpmLimit(input.rpmDraft)
    if (!rpmLimit.ok) {
      return rpmLimit
    }
    request.rpmLimit = rpmLimit.value
  }

  if (input.editGroups) {
    request.groups = { mode: input.groupMode, values: input.groups }
  }

  if (input.editSource) {
    request.sourceChannel = input.sourceChannel.trim()
  }

  return { ok: true, value: request }
}
