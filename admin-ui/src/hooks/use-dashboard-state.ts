import { useCallback, useMemo, useState } from 'react'
import { storage, type CredentialView } from '@/lib/storage'
import { detectTier, type Tier } from '@/components/subscription-badge'
import type { CredentialStatusItem } from '@/types/api'

/**
 * 凭据管理页的 UI 状态。
 *
 * dashboard.tsx 早期把约四十个 `useState` 平铺在同一个函数体里，读代码时无从判断
 * 哪几个是一组、改一个会牵动谁。这里按职责收拢成三块彼此不相干的状态：
 * 弹窗开关、持久化的展示偏好、筛选条件。
 *
 * 刻意**没有**收拢批量操作那一组（验证 / 探活 / 超额 / 导出的进行中标志与进度）：
 * 它们和各自的 mutation、toast、取消 ref 缠在一起，单独搬状态只会把一个函数拆成
 * 两个还互相依赖的半截，不如留在原地。
 */

/** 批量导入弹窗的初始模式：通用 JSON 或 API Key 逐行文本。 */
export type BatchImportMode = 'json' | 'api-key'

export interface DashboardDialogs {
  addOpen: boolean
  batchEditOpen: boolean
  batchImportMode: BatchImportMode
  batchImportOpen: boolean
  enterpriseLoginOpen: boolean
  idcLoginOpen: boolean
  /** 带模式打开批量导入——两个 state 必须一起改，单独暴露 setter 容易只改一半。 */
  openBatchImport: (mode: BatchImportMode) => void
  proxyPoolOpen: boolean
  setAddOpen: (open: boolean) => void
  setBatchEditOpen: (open: boolean) => void
  setBatchImportOpen: (open: boolean) => void
  setEnterpriseLoginOpen: (open: boolean) => void
  setIdcLoginOpen: (open: boolean) => void
  setProxyPoolOpen: (open: boolean) => void
  setSocialLoginOpen: (open: boolean) => void
  socialLoginOpen: boolean
}

export function useDashboardDialogs(): DashboardDialogs {
  const [addOpen, setAddOpen] = useState(false)
  const [batchImportOpen, setBatchImportOpen] = useState(false)
  const [batchImportMode, setBatchImportMode] = useState<BatchImportMode>('json')
  const [batchEditOpen, setBatchEditOpen] = useState(false)
  const [idcLoginOpen, setIdcLoginOpen] = useState(false)
  const [enterpriseLoginOpen, setEnterpriseLoginOpen] = useState(false)
  const [socialLoginOpen, setSocialLoginOpen] = useState(false)
  const [proxyPoolOpen, setProxyPoolOpen] = useState(false)

  const openBatchImport = useCallback((mode: BatchImportMode) => {
    setBatchImportMode(mode)
    setBatchImportOpen(true)
  }, [])

  return {
    addOpen,
    batchEditOpen,
    batchImportMode,
    batchImportOpen,
    enterpriseLoginOpen,
    idcLoginOpen,
    openBatchImport,
    proxyPoolOpen,
    setAddOpen,
    setBatchEditOpen,
    setBatchImportOpen,
    setEnterpriseLoginOpen,
    setIdcLoginOpen,
    setProxyPoolOpen,
    setSocialLoginOpen,
    socialLoginOpen,
  }
}

export interface CredentialViewPrefs {
  pageSize: number
  privacyMode: boolean
  setPageSize: (size: number) => void
  setPrivacyMode: (enabled: boolean) => void
  setViewMode: (view: CredentialView) => void
  viewMode: CredentialView
}

/**
 * 展示偏好，三项都持久化到 localStorage。
 *
 * 收拢的意义在于写盘和 setState 必须成对：散在组件里时每个调用点都得自己记得
 * 补一句 `storage.setX`，漏一处就表现为「刷新后设置丢了」这种很难联想到原因的 bug。
 */
export function useCredentialViewPrefs(): CredentialViewPrefs {
  const [viewMode, setViewModeState] = useState<CredentialView>(() =>
    storage.getCredentialView(),
  )
  const [pageSize, setPageSizeState] = useState<number>(() =>
    storage.getCredentialPageSize(),
  )
  const [privacyMode, setPrivacyModeState] = useState<boolean>(() =>
    storage.getPrivacyMode(),
  )

  const setViewMode = useCallback((view: CredentialView) => {
    setViewModeState(view)
    storage.setCredentialView(view)
  }, [])

  const setPageSize = useCallback((size: number) => {
    setPageSizeState(size)
    storage.setCredentialPageSize(size)
  }, [])

  const setPrivacyMode = useCallback((enabled: boolean) => {
    setPrivacyModeState(enabled)
    storage.setPrivacyMode(enabled)
  }, [])

  return { pageSize, privacyMode, setPageSize, setPrivacyMode, setViewMode, viewMode }
}

/** 分组筛选的哨兵值：只显示没有任何分组的凭据。 */
export const GROUP_FILTER_NONE = '__none__'

export interface CredentialFilters {
  /** 按当前条件过滤，供分页前使用。 */
  apply: (credentials: CredentialStatusItem[]) => CredentialStatusItem[]
  setShowDisabled: (show: boolean) => void
  showDisabled: boolean
  /** 是否有任一条件生效，用于决定是否展示「已筛选」提示。 */
  active: boolean
  /** 清空分级多选。比暴露原始 setter 更窄，调用方无法误传出非法集合。 */
  clearTiers: () => void
  groupFilter: string
  searchQuery: string
  setGroupFilter: (group: string) => void
  setSearchQuery: (query: string) => void
  tierFilter: Set<Tier>
  toggleTier: (tier: Tier) => void
}

/** 三个筛选维度的取值，与 UI 状态解耦以便直接测试。 */
export interface CredentialFilterCriteria {
  groupFilter: string
  /**
   * 是否在列表里显示已禁用账号。
   *
   * 默认显示。判死账号会在保留期内留在池子里（供查看存活时长与死因），按线上封号
   * 速率可能积压上百条；关掉这个开关能只看在服务的号。
   */
  showDisabled: boolean
  searchQuery: string
  tierFilter: Set<Tier>
}

/**
 * 纯过滤逻辑，不依赖任何 React 状态。
 *
 * 单独抽出来是为了能直接测：包在 hook 里的闭包只能靠渲染才跑得到，
 * 而这段是真正的业务规则（哪些凭据该出现在列表里），值得有独立用例。
 */
export function filterCredentials(
  credentials: CredentialStatusItem[],
  { groupFilter, searchQuery, showDisabled, tierFilter }: CredentialFilterCriteria,
): CredentialStatusItem[] {
  let out = credentials
  if (!showDisabled) {
    out = out.filter((c) => !c.disabled)
  }
  if (groupFilter) {
    out =
      groupFilter === GROUP_FILTER_NONE
        ? out.filter((c) => !c.groups || c.groups.length === 0)
        : out.filter((c) => c.groups?.includes(groupFilter))
  }
  if (tierFilter.size > 0) {
    out = out.filter((c) => tierFilter.has(detectTier(c.balance?.subscriptionTitle)))
  }
  const q = searchQuery.trim().toLowerCase()
  if (q) {
    out = out.filter(
      (c) =>
        (c.sourceChannel ?? '').toLowerCase().includes(q) ||
        (c.email ?? '').toLowerCase().includes(q),
    )
  }
  return out
}

/**
 * 分组 / 订阅分级 / 模糊搜索三个筛选条件，连同过滤逻辑本身。
 *
 * 过滤函数跟着状态一起放进来，是为了让「加一个筛选维度」只需要改这一个文件——
 * 之前状态在组件顶部、过滤在两百行开外，两处很容易改漏一边。
 */
export function useCredentialFilters(): CredentialFilters {
  const [groupFilter, setGroupFilter] = useState('')
  const [tierFilter, setTierFilter] = useState<Set<Tier>>(new Set())
  const [searchQuery, setSearchQuery] = useState('')
  // 默认显示已禁用：隐藏账号是「我主动想少看点」，不该是默认行为——
  // 否则号被判死后从列表里消失，会被误认为丢了。
  const [showDisabled, setShowDisabled] = useState(true)

  const clearTiers = useCallback(() => setTierFilter(new Set()), [])

  const toggleTier = useCallback((tier: Tier) => {
    setTierFilter((prev) => {
      const next = new Set(prev)
      if (next.has(tier)) next.delete(tier)
      else next.add(tier)
      return next
    })
  }, [])

  const apply = useCallback(
    (credentials: CredentialStatusItem[]) =>
      filterCredentials(credentials, { groupFilter, searchQuery, showDisabled, tierFilter }),
    [groupFilter, searchQuery, showDisabled, tierFilter],
  )

  const active = useMemo(
    () =>
      Boolean(groupFilter) || tierFilter.size > 0 || searchQuery.trim() !== '' || !showDisabled,
    [groupFilter, searchQuery, showDisabled, tierFilter],
  )

  return {
    active,
    apply,
    clearTiers,
    groupFilter,
    searchQuery,
    setGroupFilter,
    setSearchQuery,
    setShowDisabled,
    showDisabled,
    tierFilter,
    toggleTier,
  }
}
