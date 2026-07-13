import { describe, expect, test } from 'bun:test'
import {
  buildApplyRequest,
  buildFetchProfileRequest,
  buildPreviewProfileRequest,
  buildProfilePatch,
  hasProfileDraftValue,
  modelProfileError,
  validateProfileDraft,
} from './model-profiles'

const previewFixture = {
  previewId: 'preview_01KTEST',
  baseRevision: 12,
  expiresAt: '2026-07-13T08:05:00Z',
  warnings: [],
  changes: [
    {
      id: 'claude-opus-4-8:contextWindowTokens',
      modelId: 'claude-opus-4-8',
      field: 'contextWindowTokens' as const,
      value: 1_000_000,
      source: 'kiro:list-available-models',
      currentValue: 200_000,
      currentSource: 'models.dev:anthropic',
      locked: false,
    },
    {
      id: 'claude-opus-4-8:knowledgeCutoff',
      modelId: 'claude-opus-4-8',
      field: 'knowledgeCutoff' as const,
      value: '2026-01',
      source: 'models.dev:anthropic',
      currentValue: '2025-05',
      currentSource: 'manual',
      locked: true,
    },
  ],
}

describe('model profiles', () => {
  test('validates token and cutoff fields', () => {
    expect(
      validateProfileDraft({
        contextWindowTokens: '0',
        maxOutputTokens: '128000',
        knowledgeCutoff: '2026-13',
        releaseDate: '2026-05-28',
      }) === null,
    ).toBe(false)
    expect(
      validateProfileDraft({
        contextWindowTokens: '1000000',
        maxOutputTokens: '128000',
        knowledgeCutoff: '2026-01',
        releaseDate: '2026-05-28',
      }),
    ).toBeNull()
  })

  test('allows empty fields so an override can be cleared', () => {
    expect(
      validateProfileDraft({
        contextWindowTokens: '',
        maxOutputTokens: '',
        knowledgeCutoff: '',
        releaseDate: '',
      }),
    ).toBeNull()
  })

  test('rejects token values above the backend profile limit', () => {
    expect(
      validateProfileDraft({
        contextWindowTokens: '10000001',
        maxOutputTokens: '128000',
        knowledgeCutoff: '',
        releaseDate: '',
      }),
    ).toBe('上下文窗口必须是 1 到 10000000 之间的整数')
    expect(
      validateProfileDraft({
        contextWindowTokens: '1000000',
        maxOutputTokens: '10000001',
        knowledgeCutoff: '',
        releaseDate: '',
      }),
    ).toBe('最大输出必须是 1 到 10000000 之间的整数')
  })

  test('requires a new manual profile to contain at least one value', () => {
    expect(
      hasProfileDraftValue({
        contextWindowTokens: '',
        maxOutputTokens: '',
        knowledgeCutoff: '',
        releaseDate: '',
      }),
    ).toBe(false)
    expect(
      hasProfileDraftValue({
        contextWindowTokens: '1000000',
        maxOutputTokens: '',
        knowledgeCutoff: '',
        releaseDate: '',
      }),
    ).toBe(true)
  })

  test('ignores lock metadata when checking whether a new profile has values', () => {
    const draft = {
      contextWindowTokens: '',
      maxOutputTokens: '',
      knowledgeCutoff: '',
      releaseDate: '',
      locks: {
        contextWindowTokens: true,
        maxOutputTokens: true,
        knowledgeCutoff: true,
        releaseDate: true,
      },
    }

    expect(hasProfileDraftValue(draft)).toBe(false)
  })

  test('buildProfilePatch locks manual values and emits null for cleared overrides', () => {
    expect(
      JSON.stringify(buildProfilePatch(7, {
        contextWindowTokens: '1000000',
        maxOutputTokens: '',
        knowledgeCutoff: '2026-01',
        releaseDate: '',
        locks: {
          contextWindowTokens: true,
          maxOutputTokens: true,
          knowledgeCutoff: false,
          releaseDate: true,
        },
      })),
    ).toBe(JSON.stringify({
      baseRevision: 7,
      contextWindowTokens: { value: 1_000_000, locked: true },
      maxOutputTokens: null,
      knowledgeCutoff: { value: '2026-01', locked: false },
      releaseDate: null,
    }))
  })

  test('buildApplyRequest carries preview revision and selected exact values', () => {
    const request = buildApplyRequest(previewFixture, [
      'claude-opus-4-8:contextWindowTokens',
    ])

    expect(request.previewId).toBe(previewFixture.previewId)
    expect(request.baseRevision).toBe(previewFixture.baseRevision)
    expect(JSON.stringify(request.changes)).toBe(JSON.stringify([
      {
        id: previewFixture.changes[0].id,
        modelId: previewFixture.changes[0].modelId,
        field: previewFixture.changes[0].field,
        value: previewFixture.changes[0].value,
        source: previewFixture.changes[0].source,
        lock: false,
      },
    ]))
  })

  test('does not submit locked preview changes even when selected', () => {
    const request = buildApplyRequest(previewFixture, [
      'claude-opus-4-8:knowledgeCutoff',
    ])

    expect(JSON.stringify(request.changes)).toBe('[]')
  })

  test('builds credential selectors as number or null for Option<u64>', () => {
    expect(JSON.stringify(buildFetchProfileRequest(7))).toBe(JSON.stringify({
      baseRevision: 7,
      credentialId: null,
      forcePublic: false,
    }))
    expect(JSON.stringify(buildFetchProfileRequest(8, 42, true))).toBe(JSON.stringify({
      baseRevision: 8,
      credentialId: 42,
      forcePublic: true,
    }))
    expect(JSON.stringify(buildPreviewProfileRequest())).toBe(JSON.stringify({
      forcePublic: true,
      modelId: null,
      credentialId: null,
    }))
    expect(JSON.stringify(buildPreviewProfileRequest('claude-opus-4-8', 42))).toBe(JSON.stringify({
      forcePublic: true,
      modelId: 'claude-opus-4-8',
      credentialId: 42,
    }))
  })

  test('maps revision conflicts and expired previews to actionable messages', () => {
    const revision = modelProfileError({ response: { status: 409 } })
    const expired = modelProfileError({ response: { status: 410 } })

    expect(revision.message).toBe('资料已被其他操作更新，请刷新后重试')
    expect(revision.previewExpired).toBe(false)
    expect(expired.message).toBe('预览已过期，请重新获取差异')
    expect(expired.previewExpired).toBe(true)
  })
})
