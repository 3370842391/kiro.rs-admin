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
  showDisabled: true,
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

  test('hides disabled accounts when the toggle is off', () => {
    // 判死账号会在保留期内留在池子里（按线上封号速率可能上百条），
    // 关掉开关只看在服务的号。
    const withDisabled = [
      credential({ id: 1, email: 'live@mail.com' }),
      credential({ id: 2, email: 'dead@mail.com', disabled: true }),
    ]
    expect(ids(filterCredentials(withDisabled, NO_FILTER))).toEqual([1, 2])
    expect(ids(filterCredentials(withDisabled, { ...NO_FILTER, showDisabled: false }))).toEqual([1])
  })

  test('does not mutate the input array', () => {
    const input = [...pool]
    filterCredentials(input, { ...NO_FILTER, groupFilter: '新母号' })
    expect(ids(input)).toEqual([1, 2, 3, 4])
  })
})

describe('订阅分级筛选', () => {
  // 生产实测只出现过这三种标题；PRO+ / 裸 PRO 保留以兼容其它渠道
  const tiered = [
    credential({ id: 1, balance: { subscriptionTitle: 'KIRO FREE' } as never }),
    credential({ id: 2, balance: { subscriptionTitle: 'KIRO POWER' } as never }),
    credential({ id: 3, balance: { subscriptionTitle: 'KIRO PRO MAX' } as never }),
    credential({ id: 4, balance: { subscriptionTitle: 'KIRO PRO+' } as never }),
    credential({ id: 5, balance: { subscriptionTitle: 'KIRO PRO' } as never }),
    credential({ id: 6 }),
  ]
  const ids = (list: CredentialStatusItem[]) => list.map((c) => c.id)
  const byTier = (...tiers: Tier[]) =>
    ids(filterCredentials(tiered, { ...NO_FILTER, tierFilter: new Set(tiers) }))

  test('每个档位各自命中，PRO MAX 不再混进 PRO', () => {
    // 回归：PRO MAX 自身含 "PRO"，判定顺序放错时 3 和 5 会一起被选中
    expect(byTier('pro_max')).toEqual([3])
    expect(byTier('pro')).toEqual([5])
    expect(byTier('pro_plus')).toEqual([4])
    expect(byTier('power')).toEqual([2])
    expect(byTier('free')).toEqual([1])
    expect(byTier('unknown')).toEqual([6])
  })

  test('多选取并集，方便一次挑出高档位的号', () => {
    expect(byTier('power', 'pro_max')).toEqual([2, 3])
  })

  test('空集合等于不筛', () => {
    expect(byTier()).toEqual([1, 2, 3, 4, 5, 6])
  })
})
