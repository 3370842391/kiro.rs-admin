import { extractErrorMessage } from '@/lib/utils'
import type {
  ApplyModelProfilesRequest,
  FetchModelProfileRequest,
  ModelProfileFieldName,
  ModelProfilePreviewResponse,
  PatchModelProfileRequest,
  PreviewModelProfilesRequest,
} from '@/types/api'

export interface ModelProfileDraft {
  contextWindowTokens: string
  maxOutputTokens: string
  knowledgeCutoff: string
  releaseDate: string
  locks: Record<ModelProfileFieldName, boolean>
}

export type ModelProfileDraftValues = Omit<ModelProfileDraft, 'locks'>

export interface ModelProfileErrorInfo {
  message: string
  status?: number
  previewExpired: boolean
}

export class ModelProfileRequestError extends Error {
  readonly status?: number
  readonly previewExpired: boolean

  constructor(info: ModelProfileErrorInfo) {
    super(info.message)
    this.name = 'ModelProfileRequestError'
    this.status = info.status
    this.previewExpired = info.previewExpired
  }
}

export const MAX_MODEL_PROFILE_TOKENS = 10_000_000

const TOKEN_FIELDS: Array<keyof Pick<
  ModelProfileDraftValues,
  'contextWindowTokens' | 'maxOutputTokens'
>> = ['contextWindowTokens', 'maxOutputTokens']

export function validateProfileDraft(draft: ModelProfileDraftValues): string | null {
  for (const field of TOKEN_FIELDS) {
    const raw = draft[field].trim()
    if (!raw) continue
    const value = Number(raw)
    if (
      !/^\d+$/.test(raw) ||
      !Number.isSafeInteger(value) ||
      value <= 0 ||
      value > MAX_MODEL_PROFILE_TOKENS
    ) {
      return field === 'contextWindowTokens'
        ? '上下文窗口必须是 1 到 10000000 之间的整数'
        : '最大输出必须是 1 到 10000000 之间的整数'
    }
  }

  if (draft.knowledgeCutoff.trim() && !isValidProfileDate(draft.knowledgeCutoff.trim())) {
    return '知识截止日期必须是有效的 YYYY-MM 或 YYYY-MM-DD'
  }
  if (draft.releaseDate.trim() && !isValidProfileDate(draft.releaseDate.trim())) {
    return '发布日期必须是有效的 YYYY-MM 或 YYYY-MM-DD'
  }
  return null
}

export function hasProfileDraftValue(draft: ModelProfileDraftValues): boolean {
  return TOKEN_FIELDS.some((field) => draft[field].trim().length > 0)
    || draft.knowledgeCutoff.trim().length > 0
    || draft.releaseDate.trim().length > 0
}

export function buildProfilePatch(
  baseRevision: number,
  draft: ModelProfileDraft,
): PatchModelProfileRequest {
  return {
    baseRevision,
    contextWindowTokens: numericField(draft.contextWindowTokens, draft.locks.contextWindowTokens),
    maxOutputTokens: numericField(draft.maxOutputTokens, draft.locks.maxOutputTokens),
    knowledgeCutoff: stringField(draft.knowledgeCutoff, draft.locks.knowledgeCutoff),
    releaseDate: stringField(draft.releaseDate, draft.locks.releaseDate),
  }
}

export function buildApplyRequest(
  preview: ModelProfilePreviewResponse,
  selectedIds: string[],
): ApplyModelProfilesRequest {
  const selected = new Set(selectedIds)
  return {
    previewId: preview.previewId,
    baseRevision: preview.baseRevision,
    changes: preview.changes
      .filter((change) => selected.has(change.id) && !change.locked)
      .map((change) => ({
        id: change.id,
        modelId: change.modelId,
        field: change.field,
        value: change.value,
        source: change.source,
        lock: false,
      })),
  }
}

export function buildFetchProfileRequest(
  baseRevision: number,
  credentialId: number | null = null,
  forcePublic = false,
): FetchModelProfileRequest {
  return { baseRevision, credentialId, forcePublic }
}

export function buildPreviewProfileRequest(
  modelId: string | null = null,
  credentialId: number | null = null,
): PreviewModelProfilesRequest {
  return { forcePublic: true, modelId, credentialId }
}

export function modelProfileError(error: unknown): ModelProfileErrorInfo {
  const status = readStatus(error)
  if (status === 409) {
    return {
      message: '资料已被其他操作更新，请刷新后重试',
      status,
      previewExpired: false,
    }
  }
  if (status === 410) {
    return {
      message: '预览已过期，请重新获取差异',
      status,
      previewExpired: true,
    }
  }
  return {
    message: extractErrorMessage(error),
    status,
    previewExpired: false,
  }
}

function numericField(raw: string, locked: boolean) {
  const value = raw.trim()
  return value ? { value: Number(value), locked } : null
}

function stringField(raw: string, locked: boolean) {
  const value = raw.trim()
  return value ? { value, locked } : null
}

function isValidProfileDate(value: string): boolean {
  const match = /^(\d{4})-(\d{2})(?:-(\d{2}))?$/.exec(value)
  if (!match) return false
  const year = Number(match[1])
  const month = Number(match[2])
  const day = match[3] ? Number(match[3]) : 1
  if (year < 1 || month < 1 || month > 12 || day < 1 || day > 31) return false
  const parsed = new Date(Date.UTC(year, month - 1, day))
  return (
    parsed.getUTCFullYear() === year &&
    parsed.getUTCMonth() === month - 1 &&
    parsed.getUTCDate() === day
  )
}

function readStatus(error: unknown): number | undefined {
  if (!error || typeof error !== 'object') return undefined
  const response = (error as { response?: { status?: unknown } }).response
  return typeof response?.status === 'number' ? response.status : undefined
}
