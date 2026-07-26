import { describe, expect, test } from 'bun:test'
import {
  GROUP_FILTER_NONE,
  filterCredentials,
  type CredentialFilterCriteria,
} from './use-dashboard-state'
import type { CredentialStatusItem } from '@/types/api'
import type { Tier } from '@/components/subscription-badge'

/** 只填测试关心的字段，其余走类型断言——CredentialStatusItem 有三十多个字段。 */
function credential(partial: Partial<CredentialStatusItem>): CredentialStatusItem {
  return { id: 0, ...partial } as CredentialStatusItem
}

const NO_FILTER: CredentialFilterCriteria = {
  groupFilter: '',
  searchQuery: '',
  tierFilter: new Set<Tier>(),
}

describe('filterCredentials', () => {
  const pool = [
    credential({ id: 1, email: 'Alice@Example.com', groups: ['新母号'], sourceChannel: '自动采购' }),
    credential({ id: 2, email: 'bob@mail.com', groups: ['兜底号池', '新母号'] }),
    credential({ id: 3, email: 'carol@mail.com', groups: [], sourceChannel: '手工导入' }),
    credential({ id: 4, email: 'dave@mail.com' }),
  ]

  const ids = (list: CredentialStatusItem[]) => list.map((c) => c.id)

  test('returns everything when no criteria are set', () => {
    expect(ids(filterCredentials(pool, NO_FILTER))).toEqual([1, 2, 3, 4])
  })

  test('filters by group membership', () => {
    expect(ids(filterCredentials(pool, { ...NO_FILTER, groupFilter: '新母号' }))).toEqual([1, 2])
    expect(ids(filterCredentials(pool, { ...NO_FILTER, groupFilter: '兜底号池' }))).toEqual([2])
  })

  test('the __none__ sentinel matches both empty array and missing field', () => {
    // 3 的 groups 是 []，4 干脆没有这个字段——两种「未分组」都要命中，
    // 只判 length === 0 会漏掉 undefined。
    expect(ids(filterCredentials(pool, { ...NO_FILTER, groupFilter: GROUP_FILTER_NONE }))).toEqual([
      3, 4,
    ])
  })

  test('search matches email and source channel, case-insensitively', () => {
    expect(ids(filterCredentials(pool, { ...NO_FILTER, searchQuery: 'alice' }))).toEqual([1])
    expect(ids(filterCredentials(pool, { ...NO_FILTER, searchQuery: '手工' }))).toEqual([3])
    expect(ids(filterCredentials(pool, { ...NO_FILTER, searchQuery: 'MAIL.COM' }))).toEqual([2, 3, 4])
  })

  test('whitespace-only search is treated as no filter', () => {
    expect(ids(filterCredentials(pool, { ...NO_FILTER, searchQuery: '   ' }))).toEqual([1, 2, 3, 4])
  })

  test('criteria compose with AND, not OR', () => {
    const out = filterCredentials(pool, {
      ...NO_FILTER,
      groupFilter: '新母号',
      searchQuery: 'bob',
    })
    expect(ids(out)).toEqual([2])
  })

  test('does not mutate the input array', () => {
    const input = [...pool]
    filterCredentials(input, { ...NO_FILTER, groupFilter: '新母号' })
    expect(ids(input)).toEqual([1, 2, 3, 4])
  })
})
