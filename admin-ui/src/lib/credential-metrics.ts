function finiteNonNegative(value: number): number | null {
  if (!Number.isFinite(value) || value < 0) return null
  return value
}

function wholeCount(value: number): number | null {
  const normalized = finiteNonNegative(value)
  return normalized === null ? null : Math.floor(normalized)
}

/** 最近 60 秒滚动窗口内的请求数；有限速时同时显示上限。 */
export function formatRpmMetric(current: number, limit: number): string {
  const currentValue = wholeCount(current)
  const limitValue = wholeCount(limit)
  if (currentValue === null || limitValue === null) return '未知'
  return limitValue === 0
    ? `${currentValue} 次/分钟`
    : `${currentValue} / ${limitValue} 次/分钟`
}

/** RPM 当前窗口使用率；0 表示不限速。 */
export function formatRpmUtilization(current: number, limit: number): string {
  const currentValue = finiteNonNegative(current)
  const limitValue = finiteNonNegative(limit)
  if (currentValue === null || limitValue === null) return '未知'
  if (limitValue === 0) return '不限速'
  return `${Math.min(100, Math.round((currentValue / limitValue) * 100))}%`
}

/** 失败数来自所有失败类型的累计值；没有请求时不显示虚假的 0%。 */
export function formatSuccessRate(success: number, failures: number): string {
  const successValue = wholeCount(success)
  const failureValue = wholeCount(failures)
  if (successValue === null || failureValue === null) return '未知'
  const total = successValue + failureValue
  if (total === 0) return '暂无数据'
  return `${((successValue / total) * 100).toFixed(1)}%`
}

function relativeUnit(ms: number): string {
  const minutes = Math.floor(ms / 60_000)
  if (minutes < 60) return `剩余 ${Math.max(1, minutes)}分钟`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `剩余 ${hours}小时`
  return `剩余 ${Math.floor(hours / 24)}天`
}

/** 将 Token 过期时间转成管理员可读的相对状态。 */
export function formatTokenState(expiresAt: string | null, nowMs = Date.now()): string {
  if (!expiresAt || !Number.isFinite(nowMs)) return '未知'
  const expiresMs = Date.parse(expiresAt)
  if (!Number.isFinite(expiresMs)) return '未知'
  const remaining = expiresMs - nowMs
  return remaining <= 0 ? '已过期' : relativeUnit(remaining)
}

/** `balanceUpdatedAt` 是 Unix 秒；按缓存年龄显示新鲜度。 */
export function formatBalanceFreshness(updatedAt: number | undefined, nowMs = Date.now()): string {
  if (!Number.isFinite(updatedAt) || !Number.isFinite(nowMs) || (updatedAt ?? 0) <= 0) {
    return '未查询'
  }
  const ageMs = Math.max(0, nowMs - (updatedAt as number) * 1000)
  const seconds = Math.floor(ageMs / 1000)
  if (seconds < 60) return `${seconds}秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes}分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours}小时前`
  return `${Math.floor(hours / 24)}天前`
}

/** 把毫秒时长写成「3 天 / 5 小时 / 42 分钟 / 30 秒」这种一眼能读的粒度。 */
function humanizeDuration(ms: number): string {
  const seconds = Math.floor(Math.max(0, ms) / 1000)
  if (seconds < 60) return `${seconds} 秒`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时`
  return `${Math.floor(hours / 24)} 天`
}

/** 账号存活状态。`unknown` 表示拿不到可信的加入时间，界面上不能编一个数出来。 */
export interface CredentialLifespan {
  label: string
  /**
   * - `alive` 正在服务，时长持续增长
   * - `dead` 403 判死，时长定格在死亡瞬间
   * - `stopped` 已禁用但不是判死（手动禁用 / 额度耗尽等），停止计时
   * - `unknown` 无可信加入时间，不展示时长
   */
  kind: 'alive' | 'dead' | 'stopped' | 'unknown'
}

export interface CredentialLifespanInput {
  addedAt?: string
  /** `addedAt` 是否为升级时回填值。为真时不能拿它算存活时长。 */
  addedAtBackfilled?: boolean
  diedAt?: string
  /** 账号是否被禁用。禁用的号不该继续累加存活时长。 */
  disabled?: boolean
}

/**
 * 计算账号存活时长。
 *
 * 三条容易搞错的规则，都是线上踩出来的：
 *
 * 1. **禁用的号不能继续计时。** 之前只看 `diedAt`，于是被手动禁用/额度耗尽禁用的
 *    账号仍显示「已存活 N 小时」并一直往上涨——号早就不服务了。
 * 2. **回填的 `addedAt` 不能当真实加入时间。** 本功能上线时所有存量凭据会拿到同一个
 *    回填时间戳，直接算差值等于展示「升级后经过的时长」，线上出现过所有账号都显示
 *    「已存活 10 小时」的情况。
 * 3. **判死时长要定格。** 用 `diedAt - addedAt` 而非 `now - addedAt`，否则死号的
 *    「存活时长」会随时间一直变大。
 */
export function formatCredentialLifespan(
  input: CredentialLifespanInput,
  nowMs = Date.now(),
): CredentialLifespan {
  const { addedAt, addedAtBackfilled, diedAt, disabled } = input
  const addedMs = addedAt ? Date.parse(addedAt) : NaN
  const diedMs = diedAt ? Date.parse(diedAt) : NaN
  const hasTrustedStart = Number.isFinite(addedMs) && !addedAtBackfilled

  // 判死优先：即使 addedAt 不可信，「已封号」这个事实本身也要说出来
  if (Number.isFinite(diedMs)) {
    if (!hasTrustedStart) return { kind: 'dead', label: '已封号' }
    // 时钟回拨或数据异常导致死亡早于加入时按 0 处理，不显示负数
    return { kind: 'dead', label: `存活 ${humanizeDuration(diedMs - addedMs)}后死亡` }
  }

  // 禁用但没有 diedAt：不是 403 判死，是手动禁用或额度耗尽。停止计时。
  if (disabled) return { kind: 'stopped', label: '已停用' }

  if (!hasTrustedStart || !Number.isFinite(nowMs)) {
    return { kind: 'unknown', label: '加入时间未知' }
  }
  return { kind: 'alive', label: `已存活 ${humanizeDuration(nowMs - addedMs)}` }
}

/**
 * 判死账号距离被自动清理还剩多久。
 *
 * 返回 `null` 表示不适用：没判死、或该账号不参与自动清理（手工添加的号只禁用不删）。
 */
export function formatCleanupCountdown(
  diedAt: string | undefined,
  retentionHours: number | undefined,
  autoDelete: boolean | undefined,
  nowMs = Date.now(),
): string | null {
  if (!diedAt || !autoDelete) return null
  if (!Number.isFinite(retentionHours) || (retentionHours ?? 0) <= 0) return null
  const diedMs = Date.parse(diedAt)
  if (!Number.isFinite(diedMs) || !Number.isFinite(nowMs)) return null

  const remaining = diedMs + (retentionHours as number) * 3_600_000 - nowMs
  if (remaining <= 0) return '待清理'
  return `${humanizeDuration(remaining)}后清理`
}

export function connectionLabel(hasProxy: boolean): string {
  return hasProxy ? '代理' : '直连'
}
