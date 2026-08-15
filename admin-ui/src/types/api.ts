// 凭据状态响应
export interface RpmSummary {
  windowSeconds: number
  current: number
  limitedCapacity: number
  remainingLimitedCapacity: number
  unlimitedAccounts: number
  saturatedAccounts: number
  enabledAccounts: number
}

export interface CredentialsStatusResponse {
  total: number
  available: number
  currentId: number
  rpmSummary: RpmSummary
  /**
   * 最近 60 秒的 credit 消耗速率（credits / 分钟）。
   *
   * 与 rpmSummary 同为实时窗口指标，会随流量跳动。不要用小时聚合去算这个数——
   * 整点刚过时分母只有几分钟，读数会失真。
   */
  creditsPerMinute: number
  credentials: CredentialStatusItem[]
}

// 单个凭据状态
export interface CredentialStatusItem {
  id: number
  priority: number
  inFlight: number
  firstByteEwmaMs?: number
  /** 每分钟请求数上限（0 = 不限速） */
  rpmLimit: number
  /** 每账号最大并发（in-flight 上限，0 = 不限并发） */
  maxConcurrency: number
  /** 当前滑动窗口内已用请求条数 */
  rpmCurrent: number
  disabled: boolean
  failureCount: number
  /** 累计失败次数（所有失败类型，只增不减，仅手动重置归零） */
  totalFailureCount: number
  isCurrent: boolean
  expiresAt: string | null
  authMethod: string | null
  provider?: string | null
  hasProfileArn: boolean
  nickname?: string
  email?: string
  authRegion?: string
  apiRegion?: string
  refreshTokenHash?: string
  apiKeyHash?: string
  maskedApiKey?: string
  successCount: number
  lastUsedAt: string | null
  /**
   * 加入号池的时间（RFC3339）。存活时长的计时起点。
   *
   * 升级前就存在的凭据是后端加载时的回填值，不是真实加入时间——展示时必须按
   * 「回填」口径提示，否则用户看到的是「升级后经过的时长」。
   */
  addedAt?: string
  /**
   * `addedAt` 是否为加载时回填值。
   *
   * 为真时不得把它当作真实加入时刻展示：本功能上线时所有存量凭据会拿到同一个
   * 回填时间戳，界面若直接算差值，会把「升级后经过的时长」显示成账号存活时长。
   */
  addedAtBackfilled?: boolean
  /**
   * 判死后是否参与保留期自动清理。
   *
   * false 表示只禁用、不会被后台删除 —— 手工添加的账号通常是唯一一份，删掉不可恢复。
   */
  deleteOnForbidden?: boolean
  /** 判死时间（RFC3339）。非空即代表该号已被上游封禁。 */
  diedAt?: string
  hasProxy: boolean
  proxyUrl?: string
  refreshFailureCount: number
  disabledReason?: string
  /** 账号级风控冷却剩余秒数（>0 表示冷却中） */
  throttledRemainingSecs?: number
  /** 普通 429 策略冷却剩余毫秒数（>0 表示冷却中） */
  rateLimitedRemainingMs?: number
  endpoint: string
  /** 账号所属分组（可属于多个分组） */
  groups?: string[]
  /** 账号来源渠道（纯备注） */
  sourceChannel?: string
  /** 后端缓存的最近一次余额（5 分钟内） */
  balance?: BalanceResponse
  /** 余额缓存的更新时间（Unix 秒） */
  balanceUpdatedAt?: number
}

// 余额响应
export interface BalanceResponse {
  id: number
  subscriptionTitle: string | null
  currentUsage: number
  usageLimit: number
  remaining: number
  usagePercentage: number
  nextResetAt: number | null
  /** 用户是否当前开启了超额 */
  overageEnabled?: boolean
  /** 账号订阅是否可以开启超额 */
  overageCapable?: boolean
  /** 上游 overageCapability 原始字符串，用于排查"未知"状态 */
  overageCapabilityRaw?: string
}

// 某凭据当前可用的模型列表响应
export interface AvailableModelsResponse {
  id: number
  models: AvailableModelItem[]
  resolvedApiRegion?: string
  resolvedHost?: string
  kiroVersion?: string
}

// 单个可用模型
export interface AvailableModelItem {
  modelId: string
  modelName?: string
  description?: string
  maxInputTokens?: number
}

// 凭据响应测试请求
export interface CredentialResponseTestRequest {
  model?: string
}

// 凭据响应测试响应
export interface CredentialResponseTestResponse {
  id: number
  model: string
  success: boolean
  latencyMs: number
  httpStatus?: number
  responseSnippet?: string
  error?: string
}

// 成功响应
export interface SuccessResponse {
  success: boolean
  message: string
}

// 错误响应
export interface AdminErrorResponse {
  error: {
    type: string
    message: string
  }
}

// NewAPI 收入与 RS Credits 成本利润报表
export interface ProfitConfigView {
  newapiBase?: string
  newapiUser?: string
  creditPrice: number
  quotaPerUnit: number
  tokenConfigured: boolean
}

export interface ProfitConfigUpdate {
  newapiBase?: string
  newapiToken?: string
  newapiUser?: string
  creditPrice?: number
  quotaPerUnit?: number
}

export interface ProfitBreakdownStat {
  name: string
  keyId?: number
  keyName?: string
  count: number
  revenue: number
  credits: number
  cost: number
  profit: number
  missingCost: number
}

export interface ProfitReport {
  startTimestamp: number
  endTimestamp: number
  minutes: number
  creditPrice: number
  quotaPerUnit: number
  rows: number
  matched: number
  unmatched: number
  missingCost: number
  revenue: number
  matchedRevenue: number
  unmatchedRevenue: number
  credits: number
  cost: number
  profit: number
  marginPct: number
  attributedCredits: number
  unattributedCredits: number
  attributedCost: number
  unattributedCost: number
  attributedRevenue: number
  unattributedRevenue: number
  observedChannelIds: number[]
  observedKeyIds: number[]
  ledgerScopeConfirmed: boolean
  byKey: ProfitBreakdownStat[]
  byGroup: ProfitBreakdownStat[]
  byModel: ProfitBreakdownStat[]
  byUser: ProfitBreakdownStat[]
}

// 请求类型
export interface SetDisabledRequest {
  disabled: boolean
}

export interface SetPriorityRequest {
  priority: number
}

// 添加凭据请求
export interface AddCredentialRequest {
  refreshToken?: string
  accessToken?: string
  profileArn?: string
  expiresAt?: string
  authMethod?: 'social' | 'idc' | 'api_key' | 'external_idp'
  provider?: string
  clientId?: string
  clientSecret?: string
  startUrl?: string
  /** 企业 SSO (external_idp) 的 OAuth2 Token 端点（external_idp 必填） */
  tokenEndpoint?: string
  /** 企业 SSO 的 OIDC Issuer URL（可选） */
  issuerUrl?: string
  /** 企业 SSO 授予的 scopes（空格分隔，可选） */
  scopes?: string
  priority?: number
  /** 每分钟请求数上限（默认 10；0 表示不限速） */
  rpmLimit?: number
  /** 每账号最大并发（in-flight 上限，0 表示不限并发） */
  maxConcurrency?: number
  authRegion?: string
  apiRegion?: string
  machineId?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  kiroApiKey?: string
  endpoint?: string
  nickname?: string
  email?: string
  groups?: string[]
  sourceChannel?: string
}

// 添加凭据响应
export interface AddCredentialResponse {
  success: boolean
  message: string
  credentialId: number
  email?: string
}

// 更新凭据请求（字段为 undefined 表示不修改，空字符串表示清除）
export interface UpdateCredentialRequest {
  nickname?: string
  apiRegion?: string
  email?: string
  proxyUrl?: string
  proxyUsername?: string
  proxyPassword?: string
  /** 账号所属分组（undefined 表示不修改，数组表示整体替换） */
  groups?: string[]
  /** 账号来源渠道（undefined 表示不修改，空串表示清除） */
  sourceChannel?: string
  /** 每分钟请求数上限（undefined 表示不修改，0 表示不限速） */
  rpmLimit?: number
  /** 每账号最大并发（undefined 表示不修改，0 表示不限并发） */
  maxConcurrency?: number
}

export interface BatchCredentialGroupPatch {
  mode: 'replace' | 'add' | 'remove'
  values: string[]
}

export interface BatchUpdateCredentialsRequest {
  ids: number[]
  rpmLimit?: number
  maxConcurrency?: number
  groups?: BatchCredentialGroupPatch
  sourceChannel?: string
  priority?: number
  promotePriority?: boolean
}

export interface BatchUpdateCredentialsResponse {
  selected: number
  updated: number
  unchanged: number
  priorityAdjusted: number
  rpmSummary: RpmSummary
}

// 更新 refreshToken 请求
export interface UpdateRefreshTokenRequest {
  refreshToken: string
  accessToken?: string
  expiresAt?: string
}

// 代理健康状态
export type ProxyHealth = 'unknown' | 'healthy' | 'unhealthy'

// 代理历史封号统计。来自 proxy_ban_stats.json，死号被保留期清理后依然存在
export interface ProxyBanSummary {
  /** 历史累计封号数，永不回退 */
  totalBans: number
  bans24h: number
  bans7d: number
  /** 曾经绑定过这个代理的账号总数（封号率分母） */
  accountsSeen: number
  /** totalBans / accountsSeen，0~1 */
  banRate?: number
  /** 被封账号存活时长中位数（秒），越短说明这个出口 IP 越脏 */
  medianSurvivalSecs?: number
  /** 被封账号死前成功请求数的中位数。接近 0 = 出口被标记；很大 = 号是被打死的 */
  medianSuccessesBeforeBan?: number
  /** 被封的号分布在多少个不同加入日。1 = 全是同一批，不足以归咎于这个出口 */
  distinctBatchDays: number
  firstBanAt?: string
  lastBanAt?: string
}

export type ProxyRiskLevel = 'ok' | 'watch' | 'suspect' | 'quarantineRecommended'

/** 候选排序档位：正常参与 / 只在正常档用尽后轮到 / 基本不会被选中 */
export type ProxySelectionTier = 'normal' | 'degraded' | 'penalized'

// 风险研判结论。建议模式：只给结论和理由，不会自动改代理的启用状态
export interface ProxyRiskAssessment {
  level: ProxyRiskLevel
  /** 封号率的 Wilson 95% 置信下界 */
  banRateLowerBound: number
  /** 参照用的池内封号率中位数 */
  poolMedianBanRate: number
  recommendQuarantine: boolean
  /** 候选排序权重 0~1。相对池内中位数算，全池一样烂时所有人都是 1 */
  selectionWeight: number
  /** 由权重换算的档位，排序主键 */
  selectionTier: ProxySelectionTier
  /** 支持「这个出口有问题」的证据 */
  reasons: string[]
  /** 阻止下结论的原因：原始封号率高但未建议隔离时，这里说明为什么 */
  blockers: string[]
}

// 单次封号事件
export interface ProxyBanEvent {
  credentialId: number
  email?: string
  bannedAt: string
  addedAt?: string
  survivalSecs?: number
  /** 该号死前打过的成功请求数 */
  successesBeforeBan?: number
  requestsBeforeBan?: number
  reason?: string
  proxyUrl?: string
}

// 单个代理的封号档案（含明细）
export interface ProxyBanDetailEntry extends ProxyBanSummary {
  /** 归一化代理身份：host:port（或 "(direct)"） */
  proxyKey: string
  proxyId?: number
  /** 该代理当前是否仍在代理池中 */
  inPool: boolean
  risk: ProxyRiskAssessment
  events: ProxyBanEvent[]
}

// 封号时间线条目
export interface ProxyBanTimelineItem extends ProxyBanEvent {
  proxyKey: string
}

// 封号统计总览响应
export interface ProxyBanStatsResponse {
  proxies: ProxyBanDetailEntry[]
  totalBans: number
  recentEvents: ProxyBanTimelineItem[]
}

// 代理池条目
export interface ProxyPoolEntry {
  id: number
  url: string
  label?: string
  enabled: boolean
  credentialCount: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  consecutiveFailures: number
  autoDisabled: boolean
  banStats: ProxyBanSummary
  risk: ProxyRiskAssessment
}

// 代理池列表响应
export interface ProxyPoolResponse {
  total: number
  proxies: ProxyPoolEntry[]
  /** 全池历史累计封号数，含已从池中删除的代理 */
  totalBans: number
}

// 添加代理请求
export interface AddProxyRequest {
  url: string
  label?: string
}

// 临时探测代理 URL 请求（不写入代理池）
export interface ProxyCheckUrlRequest {
  url: string
}

// 批量添加代理请求
export interface BatchAddProxyRequest {
  urls: string[]
}

// 分配代理给凭据请求
export interface AssignProxyRequest {
  proxyId?: number | null
}

// 批量添加代理响应
export interface BatchAddProxyResponse {
  added: number
  errors: number
  proxies: ProxyPoolEntry[]
  errorMessages: string[]
}

// 单个代理健康检查响应
export interface ProxyCheckResponse {
  id: number
  health: ProxyHealth
  latencyMs?: number
  lastCheckedAt?: string
  enabled: boolean
  autoDisabled: boolean
}

// 全量健康检查响应
export interface ProxyCheckAllResponse {
  healthy: number
  unhealthy: number
  autoDisabled: number
}

// 轮询批量分配请求
export interface AssignRoundRobinRequest {
  credentialIds?: number[] | null
}

// 轮询批量分配响应
export interface AssignRoundRobinResponse {
  assigned: number
  proxyCount: number
}

// 全局代理配置
export interface GlobalProxyResponse {
  proxyUrl: string | null
}

export interface SetGlobalProxyRequest {
  proxyUrl: string | null
}

// 在线更新配置
export interface UpdateConfigResponse {
  /** 上一次更新前正在运行的版本号（带 v 前缀）；存在时可调用回退接口 */
  previousVersion?: string
  /** 上一次成功完成在线更新的时间（RFC3339） */
  lastAppliedAt?: string
  /** 是否已配置 GitHub Token（仅返回布尔，不回明文） */
  githubTokenSet: boolean
  /** 是否开启无人值守自动更新 */
  autoApply: boolean
  /** 自动更新触发时间（本地时区，HH:MM 24 小时制） */
  autoApplyTime: string
}

export interface SetUpdateConfigRequest {
  /** GitHub Personal Access Token；空字符串表示清除 */
  githubToken?: string
  autoApply?: boolean
  autoApplyTime?: string
}

/** GitHub API 限流状态（含 token 验证结果） */
export interface GitHubRateLimitInfo {
  /** 提供的 token 是否有效（无 token 时为 false 但仍能查到匿名限额） */
  valid: boolean
  /** 是否带 token 调用（false = 匿名查询） */
  authenticated: boolean
  /** 限流上限（匿名 60，认证 5000） */
  limit: number
  /** 剩余可用次数 */
  remaining: number
  /** 已用次数 */
  used: number
  /** 限流窗口重置时间（Unix 秒） */
  reset: number
  /** token 对应的用户名（可能为空） */
  login?: string
  /** 失败时的提示信息 */
  warning?: string
}

export interface ImageUpdateResponse {
  success: boolean
  message: string
  output?: string
  applied: boolean
  needRestart: boolean
}

export interface UpdateCheckInfo {
  currentVersion: string
  latestVersion: string
  hasUpdate: boolean
  buildType: string
  releaseName?: string
  releaseNotes?: string
  releaseUrl?: string
  publishedAt?: string
  checkedAt: string
  cached: boolean
  warning?: string
}

// 登录API密钥修改（adminApiKey —— 管理面板登录密钥）
export interface UpdateAdminKeyRequest {
  newKey: string
}

// IdC 设备授权登录
export interface StartIdcLoginRequest {
  region: string
  startUrl?: string
  priority?: number
  email?: string
  proxyUrl?: string
}

export interface StartIdcLoginResponse {
  sessionId: string
  userCode: string
  verificationUri: string
  verificationUriComplete?: string
  expiresAt: string
  pollInterval: number
}

export type PollIdcLoginResponse =
  | { status: 'pending' }
  | { status: 'continue'; nextUrl: string }
  | { status: 'success'; credentialId: number; duplicate?: boolean }
  | { status: 'expired' }

// Social 登录（Portal PKCE OAuth）
export interface StartSocialLoginRequest {
  priority?: number
  email?: string
  proxyUrl?: string
  authEndpoint?: string
}

/** 远程访问时手动完成 Social 登录：从浏览器地址栏粘贴的回调 URL 中提取参数 */
export interface CompleteSocialLoginRequest {
  code?: string
  state?: string
  loginOption?: string
  path?: string
  issuerUrl?: string
  clientId?: string
  scopes?: string
  loginHint?: string
}

export interface StartSocialLoginResponse {
  sessionId: string
  portalUrl: string
  expiresAt: string
}

export type PollSocialLoginResponse = PollIdcLoginResponse

// ============ 客户端 API Key 分发 ============

export type ClientResponseMode = 'detection' | 'kiro_native'

export interface CacheHitRateBounds {
  minPct: number
  maxPct: number
}

export type CacheHitRatePatch =
  | { mode: 'inherit' }
  | { mode: 'custom'; minPct: number; maxPct: number }

/**
 * 对外上报 cache_creation / cache_read 的计费口径。
 *
 * - `exclusive`：互斥分摊，三桶之和 == input 总量（本项目的优化口径，给优质客户）
 * - `legacy`：被缓存覆盖的前缀同时计进 input 与 creation，三桶之和 > 总量（同行口径，给普通客户）
 *
 * 两者 total 相同，差别只在覆盖前缀算几次。命中率整形在两种口径下都照常生效。
 */
export type CacheBillingMode = 'exclusive' | 'legacy'

/** per-key 缓存策略。字段缺省 = 继承全局。 */
export interface ClientCachePolicy {
  billingMode?: CacheBillingMode
  defaultTtlSecs?: number
}

export interface ClientKeyItem {
  id: number
  /** 脱敏后的 Key（仅展示） */
  maskedKey: string
  name: string
  description?: string
  disabled: boolean
  createdAt: string
  lastUsedAt?: string
  totalCalls: number
  totalInputTokens: number
  totalOutputTokens: number
  totalCacheCreationTokens: number
  totalCacheReadTokens: number
  /** 绑定的账号分组（未绑定时为 undefined） */
  group?: string
  responseMode: ClientResponseMode
  cacheHitRate?: CacheHitRateBounds
  /** per-key 缓存策略；后端在整体为空时不下发该字段 */
  cachePolicy?: ClientCachePolicy
  /** 是否系统密钥（config.json apiKey 导入，不可删除 / 不可轮换） */
  isSystem: boolean
}

export interface ClientKeysResponse {
  total: number
  keys: ClientKeyItem[]
}

export interface CreateClientKeyRequest {
  name: string
  description?: string
  group?: string
  responseMode?: ClientResponseMode
  cacheHitRate?: CacheHitRateBounds
}

/** 创建响应：明文 Key 仅在此处返回一次 */
export interface CreateClientKeyResponse {
  id: number
  key: string
  name: string
  createdAt: string
  responseMode: ClientResponseMode
  cacheHitRate?: CacheHitRateBounds
}

export interface UpdateClientKeyRequest {
  name?: string
  description?: string
  group?: string
  responseMode?: ClientResponseMode
  cacheHitRate?: CacheHitRatePatch
  /**
   * **整体替换**语义：缺省 = 不动；`{}` = 两项都恢复继承全局；
   * 给了字段 = 覆盖该字段。与 cacheHitRate 的判别式补丁不同。
   */
  cachePolicy?: ClientCachePolicy
}

export interface UpdateClientKeyResponse {
  success: boolean
  message: string
  id: number
  responseMode: ClientResponseMode
  cacheHitRate?: CacheHitRateBounds
  cachePolicy?: ClientCachePolicy
}

// ============ 用量统计 ============

export type StatsRange = '24h' | '7d' | '30d'
export type StatsGranularity = 'hour' | 'day'

export interface StatsTimeFilter {
  range?: StatsRange
  startDate?: string
  endDate?: string
  granularity: StatsGranularity
}

export interface StatsFilter {
  /** 不传 = 全部；其它值 = 客户端 Key id */
  keyId?: number
  /** 按账号分组筛选（仅影响 timeseries / by-credential，by-model 不支持） */
  group?: string
}

export interface OverviewStats {
  todayCalls: number
  todayInputTokens: number
  todayOutputTokens: number
  todayErrors: number
  todayCredits: number
  weekCalls: number
  weekInputTokens: number
  weekOutputTokens: number
  weekCredits: number
  activeClientKeys: number
  activeCredentials: number
}

export interface TimeSeriesPoint {
  ts: string
  inputTokens: number
  outputTokens: number
  cacheCreationTokens: number
  cacheReadTokens: number
  calls: number
  errors: number
  credits: number
}

export interface ModelDistribution {
  model: string
  calls: number
  inputTokens: number
  outputTokens: number
}

export interface CredentialDistribution {
  credentialId: number
  email?: string
  calls: number
  inputTokens: number
  outputTokens: number
  errors: number
}

// ============ 请求链路追踪 ============

/** 单次上游尝试 */
export interface TraceAttempt {
  attempt: number
  credentialId: number
  email?: string | null
  endpoint: string
  /** 上游 HTTP 状态码；null = 网络层失败 */
  httpStatus: number | null
  /** success / quota_exhausted / account_throttled / auth_failed / transient / network_error / bad_request / unknown */
  outcome: string
  /** 上游错误体片段（已截断） */
  errorSnippet: string | null
  durationMs: number
}

export type CompactionDiagnosis =
  | 'normal'
  | 'payload_limit_preempted'
  | 'client_disconnected_before_signal'
  | 'proxy_context_signal_not_exposed'
  | 'client_usage_signal_incomplete'
  | 'context_signal_enqueued'
  | 'upstream_context_unknown'
  | 'suspected_client_compaction_not_triggered'
  | 'suspected_compaction_insufficient'

export interface CompactionRequestShape {
  messageCount: number
  systemCount: number
  toolCount: number
  imageCount: number
  toolUseCount: number
  toolResultCount: number
  messageBytes: number
  systemBytes: number
  toolSchemaBytes: number
  imageBytes: number
  toolUseBytes: number
  toolResultBytes: number
}

/** 仅包含数字、布尔值和安全枚举，不含请求正文、工具参数、请求头或凭证。 */
export interface CompactionDiagnostics {
  schemaVersion: number
  currentDiagnosis: string
  knownThirdPartyAutocompactRegressionPossible: boolean
  requestShape: CompactionRequestShape
  requestBodyBytes: number
  upstreamRequestCount: number
  upstreamRequestFirstBytes: number | null
  upstreamRequestLastBytes: number | null
  upstreamRequestMinBytes: number | null
  upstreamRequestMaxBytes: number | null
  contextUsageEventCount: number
  meteringEventCount: number
  upstreamContextTokens: number | null
  upstreamContextPercentage: number | null
  upstreamContextLimitReached: boolean
  clientReportedTokens: number | null
  messageStartEnqueued: boolean
  messageDeltaEnqueued: boolean
  contextWindowExceededEnqueued: boolean
  messageStopEnqueued: boolean
  clientErrorEnqueued: boolean
  semanticOutputEnqueued: boolean
  probationSemanticOutputStarted: boolean
  probationRetryConsidered: boolean
  probationRetryStarted: boolean
  clientDisconnected: boolean
  payloadLimitObserved: boolean
  finalStatus: string
  finalErrorType: string | null
}

export interface CompactionTrace {
  sessionHash: string | null
  clientVersion: string | null
  diagnosis: CompactionDiagnosis | string
  requestBodyBytes: number
  upstreamContextTokens: number | null
  upstreamContextPercentage: number | null
  clientReportedTokens: number | null
  diagnostics: CompactionDiagnostics
}

/** 一个外部请求的完整链路 */
export interface TraceRecord {
  traceId: string
  ts: string
  keyId: number
  /** masterApiKey = 历史 master 调用（已下线）；clientKey = 客户端 Key */
  keySource: 'masterApiKey' | 'clientKey'
  /** 发起请求的客户端 Key 名称（master 表示主 apiKey；管理员业务 Key 可为 null） */
  keyName?: string | null
  responseMode: ClientResponseMode
  model: string
  isStream: boolean
  /** success / error / interrupted */
  finalStatus: string
  finalCredentialId: number
  finalEmail?: string | null
  errorType: string | null
  errorMessage: string | null
  /** 对应的持久化错误快照 */
  snapshotId?: string | null
  totalAttempts: number
  durationMs: number
  /** 流式中断时已发送字节数 */
  interruptedAfterBytes: number | null
  /** 输入 token */
  inputTokens?: number
  /** 输出 token */
  outputTokens?: number
  /** 缓存创建 token */
  cacheCreationTokens?: number
  /** 缓存读取 token */
  cacheReadTokens?: number
  /** 总 token = input + output + cache_creation + cache_read */
  totalTokens?: number
  /** 费用（credits） */
  credits?: number
  /** 首 Token 延迟（毫秒，仅流式有值） */
  firstTokenMs?: number | null
  /** Kiro 上游首个原始 body chunk 延迟（毫秒，仅流式有值） */
  upstreamFirstByteMs?: number | null
  /** 实际下发的思考档位（low/medium/high/xhigh/max）；未启用/不支持为 null */
  reasoningEffort?: string | null
  /** 是否声明 1M 扩展上下文（客户端带 anthropic-beta: context-1m-... 头） */
  context1m?: boolean
  /** 客户端是否请求了推理（thinking 启用 或 显式 effort）；与 reasoningEffort 独立 */
  thinking?: boolean
  /** 是否对精确空 user 请求应用了最小兼容文本 */
  emptyUserCompatApplied?: boolean
  /** 自动压缩安全诊断；旧记录或开关关闭时为 null。 */
  compaction?: CompactionTrace | null
  attempts: TraceAttempt[]
}

/** 链路查询参数 */
export interface TraceQuery {
  status?: string
  errorType?: string
  credentialId?: number
  /** 按发起请求的客户端 Key 筛选（0 = master apiKey） */
  keyId?: number
  /** 该凭据在某一跳失败过（即便 trace 最终成功）——用于凭据失败详情 */
  failedAttemptCredentialId?: number
  model?: string
  /** 按账号分组名筛选（只返回 final_credential_id 属于该分组的 trace） */
  group?: string
  compactionDiagnosis?: string
  sessionHash?: string
  highPressureOnly?: boolean
  onlyFailed?: boolean
  limit?: number
  offset?: number
}

/** 分页响应 */
export interface TracePage {
  records: TraceRecord[]
  total: number
}

export type SnapshotSeverity = 'critical' | 'error' | 'warning' | 'info'

export type SnapshotPayloadKind =
  | 'client_request'
  | 'kiro_request'
  | 'upstream_response'
  | 'tool_diagnostics'
  | 'stream_tail'
  | 'internal_error'

export interface ErrorSnapshotSummary {
  snapshotId: string
  traceId: string
  ts: string
  model: string
  isStream: boolean
  keyId: number
  keySource: 'masterApiKey' | 'clientKey'
  responseMode: ClientResponseMode
  finalCredentialId: number
  endpoint: string | null
  httpStatus: number | null
  finalStatus: string
  errorType: string
  severity: SnapshotSeverity
  errorMessage: string | null
  recovered: boolean
  pinned: boolean
  retentionExempt: boolean
  omittedDueToDiskPressure: boolean
  payloadCount: number
  originalBytes: number
  compressedBytes: number
  createdAt: number
  updatedAt: number
}

export interface ErrorSnapshotPayloadMeta {
  seq: number
  kind: SnapshotPayloadKind
  attempt: number | null
  contentType: string
  originalBytes: number
  compressedBytes: number
  sha256: string
  partCount: number
}

export interface ErrorSnapshotDetail extends ErrorSnapshotSummary {
  payloads: ErrorSnapshotPayloadMeta[]
}

export interface ErrorSnapshotPayload extends ErrorSnapshotPayloadMeta {
  content: unknown
}

export interface ErrorSnapshotQuery {
  traceId?: string
  model?: string
  errorType?: string
  httpStatus?: number
  credentialId?: number
  severity?: SnapshotSeverity | ''
  recovered?: boolean
  pinned?: boolean
  from?: string
  to?: string
  limit?: number
  offset?: number
}

export interface ErrorSnapshotPage {
  records: ErrorSnapshotSummary[]
  total: number
}

export interface ErrorSnapshotStorageStatus {
  dbBytes: number
  walBytes: number
  shmBytes: number
  fallbackBytes: number
  totalBytes: number
  allocatedBytes: number
  liveBytes: number
  reusableBytes: number
  availableBytes: number
  maxStorageBytes: number
  minFreeDiskBytes: number
  diskPressure: boolean
  records: number
  pinnedRecords: number
  criticalRecords: number
  skippedCapacity: number
  captureMode: 'full' | 'criticalOnly' | 'metadataOnly' | 'disabled'
}

/** 单凭据失败分类计数（鉴权 / 账号风控 / 其他） */
export interface FailureStats {
  auth: number
  throttle: number
  other: number
}

/** credentialId(字符串) → 失败分类计数 */
export type FailureStatsMap = Record<string, FailureStats>

// ============ 账号分组（独立实体）============

export interface GroupItem {
  name: string
  description?: string
  createdAt: string
  /** 引用计数：有多少个凭据带这个分组 */
  credentialCount: number
  /** 引用计数：有多少把客户端 Key 绑定这个分组 */
  clientKeyCount: number
}

export interface GroupsResponse {
  total: number
  groups: GroupItem[]
}

// ============ 模型映射（请求时模型名转发） ============

export interface ModelMapping {
  /** 源模型名（客户端请求里出现的名字，如 gpt-5.5） */
  source: string
  /** 目标模型名（转发到后端实际使用的名字，如 claude-opus-4.8） */
  target: string
}

export interface ModelMappingsResponse {
  total: number
  mappings: ModelMapping[]
}

export interface UpsertModelMappingRequest {
  source: string
  target: string
}

// ============ 模型能力与身份资料 ============

export type ModelProfileFieldName =
  | 'contextWindowTokens'
  | 'maxOutputTokens'
  | 'knowledgeCutoff'
  | 'releaseDate'

export interface ModelProfileField<T extends string | number> {
  value: T
  source: string
  locked: boolean
  updatedAt: string
}

export interface ResolvedModelProfile {
  contextWindowTokens: ModelProfileField<number> | null
  maxOutputTokens: ModelProfileField<number> | null
  knowledgeCutoff: ModelProfileField<string> | null
  releaseDate: ModelProfileField<string> | null
}

export interface ModelProfileView {
  modelId: string
  contextWindowTokens: ModelProfileField<number> | null
  maxOutputTokens: ModelProfileField<number> | null
  knowledgeCutoff: ModelProfileField<string> | null
  releaseDate: ModelProfileField<string> | null
  resolved: ResolvedModelProfile
}

export interface ModelProfileFieldRef {
  modelId: string
  field: ModelProfileFieldName
  source: string
  reason: string | null
}

export interface ModelProfileSourceSummary {
  source: string
  ok: boolean
  models: number
  message: string | null
}

export interface ModelProfileSyncSummary {
  applied: ModelProfileFieldRef[]
  skipped: ModelProfileFieldRef[]
  warnings: string[]
  sources: ModelProfileSourceSummary[]
}

export interface ModelProfilesResponse {
  revision: number
  exactAnswersEnabled: boolean
  profiles: ModelProfileView[]
  lastSync: ModelProfileSyncSummary | null
}

export interface ManualModelProfileField<T extends string | number> {
  value: T
  locked: boolean
}

export interface PatchModelProfileRequest {
  baseRevision: number
  contextWindowTokens?: ManualModelProfileField<number> | null
  maxOutputTokens?: ManualModelProfileField<number> | null
  knowledgeCutoff?: ManualModelProfileField<string> | null
  releaseDate?: ManualModelProfileField<string> | null
}

export interface DeleteModelProfileRequest {
  baseRevision: number
}

export interface FetchModelProfileRequest {
  baseRevision: number
  credentialId: number | null
  forcePublic: boolean
}

export interface SyncModelProfilesRequest {
  baseRevision: number
  forcePublic: boolean
}

export interface ModelProfileSyncResponse {
  snapshot: ModelProfilesResponse
  summary: ModelProfileSyncSummary
}

export interface PreviewModelProfilesRequest {
  forcePublic: boolean
  modelId: string | null
  credentialId: number | null
}

export interface ModelProfilePreviewChange {
  id: string
  modelId: string
  field: ModelProfileFieldName
  value: string | number
  source: string
  currentValue: string | number | null
  currentSource: string | null
  locked: boolean
}

export interface ModelProfilePreviewResponse {
  previewId: string
  baseRevision: number
  expiresAt: string
  changes: ModelProfilePreviewChange[]
  warnings: string[]
}

export interface ApplyModelProfileChange {
  id: string
  modelId: string
  field: ModelProfileFieldName
  value: string | number
  source: string
  lock: boolean
}

export interface ApplyModelProfilesRequest {
  previewId: string
  baseRevision: number
  changes: ApplyModelProfileChange[]
}

export interface SetModelProfileSettingsRequest {
  enabled: boolean
}

export interface ModelProfileSettingsResponse {
  exactAnswersEnabled: boolean
}

export interface CreateGroupRequest {
  name: string
  description?: string
}

export interface UpdateGroupRequest {
  /** 新名字；不传或与原名一致则不改名 */
  newName?: string
  /** 新备注；空字符串清除；undefined 保留原值 */
  description?: string
}

// ============ 图片总预算治理 ============

export interface ImageBudgetConfig {
  enabled: boolean
  totalBase64BudgetBytes: number
  hardBase64LimitBytes: number
  historyMaxDimension: number
  historyJpegQuality: number
  retryHistoryMaxDimension: number
  retryHistoryJpegQuality: number
}

// ============ Supplier key automation ============

export type PurchaseRegionMode = 'omit' | 'fixed' | 'webhook' | 'bestAvailable' | 'batch'

export type SupplierRegion = 'us' | 'eu'

export interface SupplierCapabilities {
  regionModes: PurchaseRegionMode[]
  supportsWebhookRegistration: boolean
  purchaseIsIdempotent: boolean
  supportsPrice: boolean
}

export interface SupplierImportOverrides {
  sourceChannel?: string
  nicknameLabel?: string
  rpmLimit?: number
  maxConcurrency?: number
  priority?: number
  groups?: string[]
  autoDeleteForbidden?: boolean
}

export interface SupplierCommonConfig {
  sourceChannel: string
  nicknameLabel: string
  rpmLimit: number
  maxConcurrency: number
  priority: number
  groups: string[]
  autoDeleteForbidden: boolean
}

export interface SupplierConfigView {
  baseUrl: string
  publicBaseUrl: string
  autoPurchase: boolean
  autoDeleteForbidden: boolean
  minPurchase: number
  maxPurchase: number
  apiRegion: string
  purchaseRegionMode: PurchaseRegionMode
  purchaseRegion: SupplierRegion | null
  credentialApiRegionFallback: string
  rpmLimit: number
  maxConcurrency: number
  priority: number
  groups: string[]
  sourceChannel: string
  nicknamePrefix: string
  /**
   * Per-supplier watermark gate. On, an arrival webhook tops this supplier up to
   * `targetUsable`; off keeps the legacy behaviour of buying on every notification.
   * The global pool, when enabled, replaces this gate entirely.
   */
  restockOnlyWhenExhausted: boolean
  /**
   * Target stock: how many usable keys to keep on hand for this supplier. Same meaning
   * as the global pool's `targetCount` - an arrival buys `target - usable`, then stops.
   * So "one per supplier" is 1, and three suppliers at 1 each means three in total.
   * 0 means the switch was turned on without a number, and nothing is bought.
   */
  targetUsable: number
  /**
   * Remaining quota at or below this counts as *not* usable. 0 = ignore quota and only
   * treat bans and 402s as unusable. Absolute value, same unit as upstream `usageLimit`.
   */
  lowQuotaThreshold: number
  /**
   * Skip auto-purchase while the vendor's current unit price is above this. 0 = no cap.
   * The unit is whatever that vendor prices in (Drop quotes USD, the kiroapp family
   * quotes credits, kiro.ceo quotes per zone), so it is only ever compared against that
   * same vendor's quote and never used in cross-vendor arithmetic.
   *
   * With a cap set but no price available before ordering (kiro-rs only reports `max`),
   * the purchase is skipped. Treating "price unknown" as free would disable the cap
   * exactly when it matters most.
   */
  maxUnitPrice: number
  apiKeyConfigured: boolean
  webhookTokenConfigured: boolean
  /** HMAC signing key for `X-Kiro-Signature`. Blank means signatures are not checked. */
  webhookSecretConfigured: boolean
}

/** Secrets are write-only and are never present in SupplierConfigView. */
export interface SupplierConfigUpdate {
  baseUrl: string
  publicBaseUrl: string
  autoPurchase: boolean
  autoDeleteForbidden: boolean
  minPurchase: number
  maxPurchase: number
  apiRegion: string
  purchaseRegionMode: PurchaseRegionMode
  purchaseRegion: SupplierRegion | null
  credentialApiRegionFallback: string
  rpmLimit: number
  maxConcurrency: number
  priority: number
  groups: string[]
  sourceChannel: string
  nicknamePrefix: string
  /**
   * Per-supplier watermark gate. On, an arrival webhook tops this supplier up to
   * `targetUsable`; off keeps the legacy behaviour of buying on every notification.
   * The global pool, when enabled, replaces this gate entirely.
   */
  restockOnlyWhenExhausted: boolean
  /**
   * Target stock: how many usable keys to keep on hand for this supplier. Same meaning
   * as the global pool's `targetCount` - an arrival buys `target - usable`, then stops.
   * So "one per supplier" is 1, and three suppliers at 1 each means three in total.
   * 0 means the switch was turned on without a number, and nothing is bought.
   */
  targetUsable: number
  /**
   * Remaining quota at or below this counts as *not* usable. 0 = ignore quota and only
   * treat bans and 402s as unusable. Absolute value, same unit as upstream `usageLimit`.
   */
  lowQuotaThreshold: number
  /** Skip auto-purchase above this unit price, in the vendor's own unit. 0 = no cap. */
  maxUnitPrice: number
  apiKey?: string
  webhookToken?: string
  webhookSecret?: string
}

export type SupplierConfigPayload = Omit<
  SupplierConfigUpdate,
  'apiKey' | 'webhookToken' | 'webhookSecret'
> &
  Partial<Pick<SupplierConfigUpdate, 'apiKey' | 'webhookToken' | 'webhookSecret'>>

/**
 * Supplier protocol. `kiro-rs` is the legacy vendor API; `kiro-app` is kiroapp.cc;
 * `kiroapp-io` is kiroapp.io (`/api/me/*`, Bearer `km_…`, idempotent purchases);
 * `kiro-drop` is Kiro Drop (`/api/my/*`, `X-API-Key: usr-…`, CNY amounts encoded
 * as strings, stock reported by `/api/status`); `kiro-ceo` is kiro.ceo (`/api/my/*`,
 * `X-API-Key`, credit-based pricing, plain-string `keys` array, no `/api/status`).
 */
export type SupplierKind = 'kiro-rs' | 'kiro-app' | 'kiroapp-io' | 'kiro-drop' | 'kiro-ceo'

/** One supplier in the multi-supplier list. Settings are flattened by the server. */
export interface SupplierEntryView extends SupplierConfigView {
  id: string
  name: string
  kind: SupplierKind
  enabled: boolean
  /** Neither kiroapp protocol can register callbacks remotely; the URL must be pasted manually. */
  supportsWebhookRegistration: boolean
  capabilities: SupplierCapabilities
  importOverrides: SupplierImportOverrides
}

export interface SupplierEntryUpdate extends SupplierConfigUpdate {
  /** Required when creating; ignored when editing (the path parameter wins). */
  id?: string
  name: string
  kind: SupplierKind
  enabled: boolean
  importOverrides: SupplierImportOverrides
}

export type SupplierEntryPayload = Omit<
  SupplierEntryUpdate,
  'apiKey' | 'webhookToken' | 'webhookSecret'
> &
  Partial<Pick<SupplierEntryUpdate, 'apiKey' | 'webhookToken' | 'webhookSecret'>>

export interface SupplierListResponse {
  items: SupplierEntryView[]
}

export interface SupplierOverview {
  supplierId: string
  kind: SupplierKind
  /** Synthesised for the kiroapp protocols, which have no profile endpoint. */
  profile: {
    name: string
    quota: number
    remaining: number
    usedQuota: number
  }
  stockMax: number
  /**
   * Price per key. For `kiroapp-io` this is the *lowest* tier — pricing is tiered by
   * each mother account's cumulative output, so a single order can mix prices.
   */
  keyPrice: number | null
  /** `kiroapp-io` only: highest tier price. Together with `keyPrice` it's the quoted range. */
  keyPriceMax: number | null
  /** Remaining quota/credits. */
  balance: number | null
  /**
   * Local pool health for keys bought from this supplier. The restock gate compares
   * `usable` against the watermark, so this is what explains "why didn't it buy".
   */
  credentialHealth: SupplierCredentialHealth
  webhookRegistered: boolean
  status: {
    keysActive: number
    keysDead: number
    keysStock: number
    generating: boolean
  }
}

export interface SupplierCallbackUrlResponse {
  callbackUrl: string
}

// ============ Global key pool ============

/**
 * Global key pool config. One per instance, shared by every supplier.
 *
 * `targetCount` is a **stock target**, not a per-arrival cap: the total number of usable
 * auto-purchased credentials must not exceed it. On each arrival notification the server
 * computes `targetCount - currentUsable` and buys that deficit from the notifying supplier
 * only. Suppliers have no priority order — whoever's event is processed first takes the
 * deficit, which the existing global FIFO event queue already gives us.
 *
 * Enabling this takes over restock decisions: each supplier's own
 * `restockOnlyWhenExhausted` / `targetUsable` / `lowQuotaThreshold` stop
 * participating, so there is never a second watermark to reason about.
 */
export interface SupplierPoolConfig {
  /** Off (default) means the whole feature is inert and per-supplier buying is unchanged. */
  enabled: boolean
  /**
   * Stock target. `0` is the "not configured" sentinel, not a business default — enabling
   * without setting a number must result in *not buying*, never in guessing a value.
   */
  targetCount: number
  /** Remaining quota at or below this counts as not usable. 0 = ignore quota. */
  lowQuotaThreshold: number
}

/** Per-credential health split, reused from supplier and global pool overviews. */
export interface SupplierCredentialHealth {
  total: number
  /** Compatibility alias for targetCredited. */
  usable: number
  /** Credentials that can be scheduled immediately. */
  ready: number
  /** Credentials credited toward the stock target. */
  targetCredited: number
  /** Manually paused credentials retained by operations. */
  manualReserved: number
  /** Credentials temporarily cooling down after rate-limit or risk responses. */
  cooling: number
  /** Automatically disabled credentials excluded from the stock target. */
  systemDisabled: number
  /** Banned. Still in the pool until the retention window expires, but never counted usable. */
  dead: number
  quotaExhausted: number
  lowQuota: number
}

export type SupplierPoolHealth = SupplierCredentialHealth

export interface SupplierPoolStatus {
  enabled: boolean
  targetCount: number
  lowQuotaThreshold: number
  /** Currently usable auto-purchased credentials. Equals `health.usable`. */
  globalUsable: number
  /** How many more to buy. `0` means the pool is full and arrivals will be skipped. */
  deficit: number
  /**
   * The four-way split. Answers "there are 10 keys in the pool, why is usable only 3" —
   * usually because several are banned or out of quota.
   */
  health: SupplierPoolHealth
  /** Credentials recognised via `supplierId` (written by current purchases). */
  bySupplierId: number
  /**
   * Credentials recognised only by their `sourceChannel` note — bought before `supplierId`
   * existed. Dropping to 0 unexpectedly usually means someone edited a supplier's
   * `sourceChannel`, which silently stops those keys counting toward the watermark.
   */
  byLegacyChannel: number
  /** The `sourceChannel` values currently used for note matching, sorted. */
  matchedChannels: string[]
}

export interface SupplierDeleteResponse {
  deleted: boolean
}

export type SupplierEventStatus = 'received' | 'processing' | 'succeeded' | 'skipped' | 'failed'

export type SupplierRegionSource =
  | 'purchaseResponse'
  | 'webhook'
  | 'request'
  | 'configFallback'

export interface SupplierDecisionSnapshot {
  version: number
  outcome: string
  reason: string | null
  trigger: {
    eventType: string
    quantity: number
    attempt: number
  }
  supplier: {
    id: string
    kind: SupplierKind | null
    enabled: boolean | null
    autoPurchase: boolean | null
    minPurchase: number | null
    maxPurchase: number | null
  }
  target: {
    scope: string | null
    configured: number | null
    creditedAtDecision: number | null
    deficit: number | null
    requested: number | null
    reached: boolean | null
    health: SupplierCredentialHealth | null
    globalPoolEnabled: boolean
  }
  quote: {
    vendorStock: number | null
    unitPrice: number | null
    maxUnitPrice: number | null
  }
  region: {
    mode: PurchaseRegionMode | null
    configuredPurchaseRegion: SupplierRegion | null
    webhookRegion: SupplierRegion | null
    requestedRegion: SupplierRegion | null
    requestedRegionSource: SupplierRegionSource | null
    actualRegion: SupplierRegion | null
    actualRegionSource: SupplierRegionSource | null
    credentialApiRegionFallback: string | null
  }
  result: {
    purchased: number
    imported: number
    duplicate: number
    failed: number
    totalDebit: number | null
    supplierOrderId: string | null
    replayed: boolean
  }
}

export interface SupplierEvent {
  id: number
  supplierId: string
  eventId: string
  eventType: string
  purchaseOrderId: string | null
  /** Vendor-side batch id, for reconciling against their console. `kiroapp-io` only. */
  supplierBatchId: string | null
  message: string | null
  quantity: number
  receivedAt: string
  status: SupplierEventStatus
  attempts: number
  lastError: string | null
  purchasedCount: number
  importedCount: number
  duplicateCount: number
  webhookDuplicateCount: number
  failedCount: number
  readAt: string | null
  /** Actual amount charged for this order, in the supplier's credits. Tiered pricing makes this the only authoritative figure. */
  totalDebit: number | null
  /** Average unit price for this order = `totalDebit / purchasedCount`. */
  unitPrice: number | null
  /** Vendor-side order id, for reconciling against their order history. Not the same as `supplierBatchId`. */
  supplierOrderId: string | null
  /** The vendor replayed an earlier settled order, meaning the previous attempt actually succeeded. */
  replayed: boolean
  /**
   * Earliest time this event may be picked up again. Non-null means it hit a transient
   * vendor failure (5xx / network / 429) and is queued for an automatic retry, which is
   * a different situation from a `received` event that simply has not been reached yet.
   */
  retryAfter: string | null
  /** Purchase count already sent upstream. A retry replays this exact count to hit the vendor's idempotency. */
  purchaseCount: number | null
  /** Safe, structured evidence captured at the time the purchase decision was made. */
  decisionSnapshot: SupplierDecisionSnapshot | null
}

export interface SupplierEventPage {
  items: SupplierEvent[]
  unreadCount: number
}

export interface SupplierEventQuery {
  limit?: number
  before?: number
  /** Restrict to one supplier; omit to see every supplier's events. */
  supplierId?: string
}

export interface PurchaseResponse {
  supplierId: string
  orderId: string
  /** Points spent. `kiro-app` reports `pointsCost`; `kiroapp-io` reports `total_debit`. */
  pointsCost?: number | null
  requested: number
  purchased: number
  imported: number
  duplicate: number
  failed: number
}

export interface SupplierWebhookRegisterResponse {
  callbackUrl: string
}

export interface SupplierWebhookTestResponse {
  success: boolean
}

export type SupplierMarkEventsReadRequest =
  | { ids: number[]; markAll?: false; supplierId?: string }
  | { markAll: true; ids?: never; supplierId?: string }

export interface SupplierMarkEventsReadResponse {
  updated: number
}

export interface SupplierRetryEventResponse {
  retried: boolean
}
