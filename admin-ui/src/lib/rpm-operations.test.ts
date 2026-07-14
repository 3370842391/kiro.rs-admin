import { describe, expect, test } from 'bun:test'
import {
  buildBatchUpdateRequest,
  parseRpmLimit,
  rpmLoadState,
  totalInFlight,
} from './rpm-operations'

describe('parseRpmLimit', () => {
  test('空输入提示输入 RPM 上限', () => {
    expect(parseRpmLimit('   ')).toEqual({ ok: false, message: '请输入 RPM 上限' })
  })

  test('接受边界值并保留 0', () => {
    expect(parseRpmLimit(' 0 ')).toEqual({ ok: true, value: 0 })
    expect(parseRpmLimit('100000')).toEqual({ ok: true, value: 100000 })
  })

  test.each(['-1', '1.5', '1e3', 'NaN', '100001'])(
    '拒绝无效十进制整数 %s',
    (draft) => {
      expect(parseRpmLimit(draft)).toEqual({
        ok: false,
        message: 'RPM 上限必须是 0 到 100000 的整数',
      })
    },
  )
})

describe('rpmLoadState', () => {
  test('0 上限表示不限速', () => {
    expect(rpmLoadState(999, 0)).toBe('unlimited')
  })

  test('在 80% 和 100% 边界切换状态', () => {
    expect(rpmLoadState(79, 100)).toBe('normal')
    expect(rpmLoadState(80, 100)).toBe('warning')
    expect(rpmLoadState(99, 100)).toBe('warning')
    expect(rpmLoadState(100, 100)).toBe('saturated')
  })

  test('把无效当前值归一化为 0，把无效上限视为不限速', () => {
    expect(rpmLoadState(-1, 100)).toBe('normal')
    expect(rpmLoadState(Number.NaN, 100)).toBe('normal')
    expect(rpmLoadState(50, Number.NaN)).toBe('unlimited')
    expect(rpmLoadState(50, -1)).toBe('unlimited')
  })
})

describe('totalInFlight', () => {
  test('汇总全部凭据并把缺失或无效值按 0 处理', () => {
    expect(
      totalInFlight([
        { inFlight: 3 },
        {},
        { inFlight: -1 },
        { inFlight: Number.NaN },
        { inFlight: 4 },
      ]),
    ).toBe(7)
  })
})

describe('buildBatchUpdateRequest', () => {
  test('三个编辑开关都关闭时返回具体错误', () => {
    expect(
      buildBatchUpdateRequest({
        ids: [1],
        editRpm: false,
        rpmDraft: '',
        editGroups: false,
        groupMode: 'replace',
        groups: [],
        editSource: false,
        sourceChannel: '',
      }),
    ).toEqual({ ok: false, message: '请至少选择一项要修改的内容' })
  })

  test('RPM 编辑关闭时省略 rpmLimit 字段', () => {
    const result = buildBatchUpdateRequest({
      ids: [1, 2],
      editRpm: false,
      rpmDraft: '',
      editGroups: false,
      groupMode: 'replace',
      groups: [],
      editSource: true,
      sourceChannel: ' manual ',
    })

    expect(result).toEqual({
      ok: true,
      value: { ids: [1, 2], sourceChannel: 'manual' },
    })
    if (result.ok) {
      expect('rpmLimit' in result.value).toBe(false)
    }
  })

  test('RPM 编辑开启时保留 0', () => {
    expect(
      buildBatchUpdateRequest({
        ids: [3],
        editRpm: true,
        rpmDraft: '0',
        editGroups: false,
        groupMode: 'replace',
        groups: [],
        editSource: false,
        sourceChannel: '',
      }),
    ).toEqual({ ok: true, value: { ids: [3], rpmLimit: 0 } })
  })

  test.each(['replace', 'add', 'remove'] as const)('直接构建 %s 分组补丁', (mode) => {
    expect(
      buildBatchUpdateRequest({
        ids: [4, 5],
        editRpm: false,
        rpmDraft: '',
        editGroups: true,
        groupMode: mode,
        groups: ['alpha', 'beta'],
        editSource: false,
        sourceChannel: '',
      }),
    ).toEqual({
      ok: true,
      value: {
        ids: [4, 5],
        groups: { mode, values: ['alpha', 'beta'] },
      },
    })
  })

  test('来源渠道 trim 后的空串会显式发送以清除字段', () => {
    expect(
      buildBatchUpdateRequest({
        ids: [6],
        editRpm: false,
        rpmDraft: '',
        editGroups: false,
        groupMode: 'replace',
        groups: [],
        editSource: true,
        sourceChannel: '   ',
      }),
    ).toEqual({ ok: true, value: { ids: [6], sourceChannel: '' } })
  })
})
