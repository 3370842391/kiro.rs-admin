/**
 * 单账号"当前被打了多少并发"的展示语义。
 *
 * 后端 `inFlight` 是该凭据此刻未结束的上游请求数（`in_flight_guard` 进出成对增减），
 * 是判断"这个号正被打多狠"的唯一实时信号，因此在账号列表里当一等公民展示。
 */
export type ConcurrencyTone = 'idle' | 'active' | 'busy' | 'hot'

/** 展示用：1–2 算轻载。调度不再按 in_flight=2 拆会话，刹车是 RPM / 429。 */
const LIGHT_LOAD = 2
const BUSY_LIMIT = 5

export function concurrencyTone(inFlight: number): ConcurrencyTone {
  if (!Number.isFinite(inFlight) || inFlight <= 0) return 'idle'
  if (inFlight <= LIGHT_LOAD) return 'active'
  if (inFlight <= BUSY_LIMIT) return 'busy'
  return 'hot'
}

/** 悬浮说明：把数字翻译成"这号现在什么处境"。 */
export function concurrencyHint(inFlight: number): string {
  const value = Number.isFinite(inFlight) && inFlight > 0 ? Math.floor(inFlight) : 0
  if (value === 0) return '当前没有请求打在这个账号上'
  const suffix =
    value > BUSY_LIMIT
      ? '，明显被压，考虑加号或降低该号优先级'
      : value > LIGHT_LOAD
        ? '，同一会话的 helper 仍会粘在这个号上'
        : ''
  return `当前有 ${value} 个请求正在这个账号上执行${suffix}`
}

/**
 * 并发条的填充比例（0–1）。没有硬上限可参照，用 8 并发作满格刻度，
 * 只为给眼睛一个相对量感，不代表真实上限。
 */
const GAUGE_FULL_SCALE = 8

export function concurrencyFillRatio(inFlight: number): number {
  if (!Number.isFinite(inFlight) || inFlight <= 0) return 0
  return Math.min(1, inFlight / GAUGE_FULL_SCALE)
}
