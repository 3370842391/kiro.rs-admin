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

/** 账号存活状态。`unknown` 表示后端没给出加入时间，界面上不能编一个数出来。 */
export interface CredentialLifespan {
  /** 已判死则为死亡时刻的存活时长，否则为「至今」的存活时长。 */
  label: string
  kind: 'alive' | 'dead' | 'unknown'
}

/**
 * 计算账号存活时长。
 *
 * 判死的号用 `diedAt - addedAt`（定格在死亡瞬间，不再随时间增长）；活着的号用
 * `now - addedAt`。两者文案必须能区分，否则「存活 58 分钟」到底是还活着还是已经
 * 死了看不出来。
 *
 * `addedAt` 缺失时返回 `unknown` 而不是回退到 0——号池升级前的凭据由后端回填
 * 加入时间，那种情况下这里能拿到值；真的拿不到值时显示「未知」比显示一个假数字好。
 */
export function formatCredentialLifespan(
  addedAt: string | undefined,
  diedAt: string | undefined,
  nowMs = Date.now(),
): CredentialLifespan {
  const addedMs = addedAt ? Date.parse(addedAt) : NaN
  if (!Number.isFinite(addedMs)) return { kind: 'unknown', label: '未知' }

  const diedMs = diedAt ? Date.parse(diedAt) : NaN
  if (Number.isFinite(diedMs)) {
    // 时钟回拨或数据异常导致死亡早于加入时，按 0 处理而不是显示负数
    return {
      kind: 'dead',
      label: `存活 ${humanizeDuration(diedMs - addedMs)}后死亡`,
    }
  }

  if (!Number.isFinite(nowMs)) return { kind: 'unknown', label: '未知' }
  return { kind: 'alive', label: `已存活 ${humanizeDuration(nowMs - addedMs)}` }
}

export function connectionLabel(hasProxy: boolean): string {
  return hasProxy ? '代理' : '直连'
}
