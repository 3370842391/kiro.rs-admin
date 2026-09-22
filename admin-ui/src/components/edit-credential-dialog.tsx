import { useState, useEffect } from 'react'
import { toast } from 'sonner'
import { useQuery } from '@tanstack/react-query'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import {
  Select,
  SelectGroup,
  SelectLabel,
  SelectTrigger,
  SelectValue,
  SelectContent,
  SelectItem,
} from '@/components/ui/select'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { useUpdateCredential } from '@/hooks/use-credentials'
import { useGroupOptions } from '@/hooks/use-groups'
import { getProxyPool } from '@/api/credentials'
import { extractErrorMessage, proxyDisplayHost } from '@/lib/utils'
import { GroupMultiSelect } from '@/components/group-select'
import type { CredentialStatusItem } from '@/types/api'

interface EditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credential: CredentialStatusItem
}

export function EditCredentialDialog({
  open,
  onOpenChange,
  credential,
}: EditCredentialDialogProps) {
  const [nickname, setNickname] = useState(credential.nickname ?? '')
  const [apiRegion, setApiRegion] = useState(credential.apiRegion ?? '')
  const [email, setEmail] = useState(credential.email ?? '')
  const [proxyUrl, setProxyUrl] = useState(credential.proxyUrl ?? '')
  const [proxyUsername, setProxyUsername] = useState('')
  const [proxyPassword, setProxyPassword] = useState('')
  const [groups, setGroups] = useState<string[]>(credential.groups ?? [])
  const [sourceChannel, setSourceChannel] = useState(credential.sourceChannel ?? '')
  const [rpmLimit, setRpmLimit] = useState(String(credential.rpmLimit ?? 10))
  const [maxConcurrency, setMaxConcurrency] = useState(String(credential.maxConcurrency ?? 0))
  const [costRmb, setCostRmb] = useState(String(credential.earnings?.costRmb ?? ''))
  // 只在「手填」时预填，避免把上游查到的额度写成手填值——那样上游额度变了也不会跟着更新
  const [quotaCredits, setQuotaCredits] = useState(
    credential.earnings?.quotaSource === 'manual'
      ? String(credential.earnings.quotaCredits ?? '')
      : ''
  )
  const [manualMode, setManualMode] = useState(false)
  const [endpoint, setEndpoint] = useState(
    credential.endpointPinned ? (credential.endpoint ?? '') : '',
  )

  const groupOptions = useGroupOptions()

  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  // 只在「打开弹窗」或「切换到另一个凭据 id」时重置表单。
  // 不能依赖整个 credential 对象：列表每 10s 轮询会换引用，会把用户已选分组/已填字段冲掉。
  useEffect(() => {
    if (!open) return
    setNickname(credential.nickname ?? '')
    setApiRegion(credential.apiRegion ?? '')
    setEmail(credential.email ?? '')
    setProxyUrl(credential.proxyUrl ?? '')
    setProxyUsername('')
    setProxyPassword('')
    setGroups(credential.groups ?? [])
    setSourceChannel(credential.sourceChannel ?? '')
    setRpmLimit(String(credential.rpmLimit ?? 10))
    setMaxConcurrency(String(credential.maxConcurrency ?? 0))
    setCostRmb(String(credential.earnings?.costRmb ?? ''))
    setQuotaCredits(
      credential.earnings?.quotaSource === 'manual'
        ? String(credential.earnings.quotaCredits ?? '')
        : ''
    )
    setManualMode(false)
    setEndpoint(credential.endpointPinned ? (credential.endpoint ?? '') : '')
  }, [open, credential.id])

  const { mutate, isPending } = useUpdateCredential()
  const isApiKey = credential.authMethod === 'api_key'
  const isEnterprise =
    credential.authMethod === 'idc' ||
    credential.authMethod === 'external_idp' ||
    credential.provider?.toLowerCase() === 'enterprise'

  const handleSubmit = (e: React.FormEvent) => {
    e.preventDefault()

    if (isApiKey && !apiRegion) {
      toast.error('请选择 API Region')
      return
    }

    mutate(
      {
        id: credential.id,
        req: {
          nickname: nickname.trim(),
          apiRegion: isApiKey ? apiRegion : undefined,
          email: email,
          proxyUrl: proxyUrl,
          proxyUsername: proxyUsername || undefined,
          proxyPassword: proxyPassword || undefined,
          groups: groups,
          sourceChannel: sourceChannel,
          rpmLimit: rpmLimit.trim() === '' ? undefined : Number(rpmLimit),
          maxConcurrency: maxConcurrency.trim() === '' ? undefined : Number(maxConcurrency),
          // 留空表示清除（后端把 0 当清除），不是"不修改"——否则填错了就再也改不回来
          costRmb: costRmb.trim() === '' ? 0 : Number(costRmb),
          quotaCredits: quotaCredits.trim() === '' ? 0 : Number(quotaCredits),
          endpoint,
        },
      },
      {
        onSuccess: (data) => {
          toast.success(data.message)
          onOpenChange(false)
        },
        onError: (error: unknown) => {
          toast.error(`更新失败: ${extractErrorMessage(error)}`)
        },
      }
    )
  }

  const enabledProxies = proxyPool?.proxies.filter(p => p.enabled) ?? []

  // 当前 proxyUrl 是否是自定义值（不匹配任何标准选项）
  const isCustomUrl = proxyUrl !== '' && proxyUrl !== 'direct' &&
    !enabledProxies.some(p => p.url === proxyUrl)

  // 显示手动输入框：明确进入手动模式，或当前值就是自定义值
  const showManualInput = manualMode || isCustomUrl

  const selectValue = showManualInput ? '__custom__' : proxyUrl

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="flex max-h-[min(88dvh,720px)] flex-col overflow-hidden p-4 sm:max-w-md sm:p-6">
        <DialogHeader className="shrink-0">
          <DialogTitle>
            编辑凭据 #{credential.id}
          </DialogTitle>
        </DialogHeader>

        <form onSubmit={handleSubmit} className="flex min-h-0 flex-1 flex-col">
          <div className="min-h-0 flex-1 space-y-3 overflow-y-auto py-3 pr-1">
            <div className="space-y-2">
              <label htmlFor="nickname" className="text-sm font-medium">
                Nickname（可选）
              </label>
              <Input
                id="nickname"
                value={nickname}
                onChange={(e) => setNickname(e.target.value)}
                maxLength={128}
                disabled={isPending}
              />
            </div>

            {isApiKey && (
              <div className="space-y-2">
                <label className="text-sm font-medium">API Key Region</label>
                <div className="grid gap-2 sm:grid-cols-2">
                  <div className="space-y-1.5">
                    <label htmlFor="editAuthRegion" className="text-xs text-muted-foreground">
                      Auth Region
                    </label>
                    <Input
                      id="editAuthRegion"
                      value="us-east-1"
                      readOnly
                      aria-readonly="true"
                      disabled={isPending}
                    />
                  </div>
                  <div className="space-y-1.5">
                    <label htmlFor="editApiRegion" className="text-xs text-muted-foreground">
                      API Region <span className="text-red-500">*</span>
                    </label>
                    <Select
                      value={apiRegion}
                      onValueChange={setApiRegion}
                      disabled={isPending}
                    >
                      <SelectTrigger id="editApiRegion" className="h-10 rounded-xl px-3.5">
                        <SelectValue placeholder="请选择 API Region" />
                      </SelectTrigger>
                      <SelectContent>
                        <SelectItem value="us-east-1">美国（us-east-1）</SelectItem>
                        <SelectItem value="eu-central-1">欧洲（eu-central-1）</SelectItem>
                      </SelectContent>
                    </Select>
                  </div>
                </div>
                <p className="text-xs text-muted-foreground">
                  修正 API Region 后，因 InvalidConfig 禁用的 API Key 会自动重新启用。
                </p>
              </div>
            )}

            {/* 邮箱 */}
            <div className="space-y-2">
              <label htmlFor="email" className="text-sm font-medium">
                邮箱（用于显示标识）
              </label>
              <Input
                id="email"
                type="email"
                placeholder="例: user@example.com"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                disabled={isPending}
              />
              <p className="text-xs text-muted-foreground">留空显示凭据 ID</p>
            </div>

            <div className="space-y-2">
              <p className="text-sm font-medium">端点</p>
              <div className="grid grid-cols-3 gap-2" role="group" aria-label="账号端点">
                {(
                  [
                    { value: '', label: '跟随默认' },
                    { value: 'ide', label: 'ide' },
                    { value: 'runtime', label: 'runtime' },
                  ] as const
                ).map((item) => (
                  <Button
                    key={item.value || 'default'}
                    type="button"
                    size="sm"
                    className="h-11 sm:h-8"
                    variant={endpoint === item.value ? 'default' : 'outline'}
                    aria-pressed={endpoint === item.value}
                    onClick={() => setEndpoint(item.value)}
                    disabled={isPending}
                  >
                    {item.label}
                  </Button>
                ))}
              </div>
              <p className="text-xs text-muted-foreground">
                跟随默认走全局协议。429 策略是全局的，不能按号改。
              </p>
            </div>

            {/* 账号分组 */}
            <div className="space-y-2">
              <label className="text-sm font-medium">账号分组</label>
              <GroupMultiSelect
                value={groups}
                options={groupOptions}
                onChange={setGroups}
                disabled={isPending}
              />
              <p className="text-xs text-muted-foreground">
                绑定了分组的客户端 Key 只调度到该组账号。
              </p>
            </div>

            {/* 账号来源渠道 */}
            <div className="space-y-2">
              <label htmlFor="sourceChannel" className="text-sm font-medium">
                账号来源渠道（备注）
              </label>
              <Input
                id="sourceChannel"
                placeholder="例: 官方, 转售商A, 采购平台X"
                value={sourceChannel}
                onChange={(e) => setSourceChannel(e.target.value)}
                maxLength={128}
                disabled={isPending}
              />
              <p className="text-xs text-muted-foreground">来源备注，留空清除</p>
            </div>

            {/* RPM 限速 */}
            <div className="space-y-2">
              <label htmlFor="rpmLimit" className="text-sm font-medium">
                每分钟请求上限（RPM）
              </label>
              <Input
                id="rpmLimit"
                type="number"
                min={0}
                placeholder="10"
                value={rpmLimit}
                onChange={(e) => setRpmLimit(e.target.value)}
                disabled={isPending}
              />
              <p className="text-xs text-muted-foreground">
                每分钟上限，默认 10；0 不限速。
                {credential.inferredRpm
                  ? credential.inferredRpm.kind === 'ceiling'
                    ? ` 近 ${credential.inferredRpm.sampleMinutes} 分钟已见 429，建议不超过 ${credential.inferredRpm.suggested}。`
                    : ` 近 ${credential.inferredRpm.sampleMinutes} 分钟未见 429，至少能到 ${credential.inferredRpm.suggested}。`
                  : ''}
              </p>
            </div>

            {/* 并发上限 */}
            <div className="space-y-2">
              <label htmlFor="maxConcurrency" className="text-sm font-medium">
                并发上限
              </label>
              <Input
                id="maxConcurrency"
                type="number"
                min={0}
                placeholder="0"
                value={maxConcurrency}
                onChange={(e) => setMaxConcurrency(e.target.value)}
                disabled={isPending}
              />
              <p className="text-xs text-muted-foreground">同时在途上限，0 不限</p>
            </div>

            {/* 收益核算：买入价与额度都手填，因为不同渠道、不同批次差别很大 */}
            <div className="grid grid-cols-2 gap-3">
              <div className="space-y-1.5">
                <label htmlFor="costRmb" className="text-sm font-medium">
                  买入价（¥）
                </label>
                <Input
                  id="costRmb"
                  type="number"
                  min={0}
                  step="0.01"
                  placeholder="留空只计收入"
                  value={costRmb}
                  onChange={(e) => setCostRmb(e.target.value)}
                  disabled={isPending}
                />
                <p className="text-xs text-muted-foreground">实际花费，不填不算利润</p>
              </div>
              <div className="space-y-1.5">
                <label htmlFor="quotaCredits" className="text-sm font-medium">
                  额度积分
                </label>
                <Input
                  id="quotaCredits"
                  type="number"
                  min={0}
                  placeholder={
                    credential.earnings?.quotaSource === 'upstream'
                      ? `上游 ${credential.earnings.quotaCredits}`
                      : '留空用上游额度'
                  }
                  value={quotaCredits}
                  onChange={(e) => setQuotaCredits(e.target.value)}
                  disabled={isPending}
                />
                <p className="text-xs text-muted-foreground">填了覆盖上游额度</p>
              </div>
            </div>

            {/* 代理配置 */}
            <div className="space-y-2">
              <label className="text-sm font-medium">代理配置</label>

              {/* 下拉选择代理 */}
              <Select
                value={selectValue === '' ? '__global__' : selectValue}
                onValueChange={(val) => {
                  if (val === '__custom__') {
                    setManualMode(true)
                    // 保留当前 proxyUrl 作为初始值让用户编辑
                  } else {
                    setManualMode(false)
                    setProxyUrl(val === '__global__' ? '' : val)
                  }
                }}
                disabled={isPending}
              >
                <SelectTrigger className="h-10 rounded-xl px-3.5" aria-label="代理设置">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {isEnterprise ? (
                    <SelectItem value="__global__">使用全局代理配置</SelectItem>
                  ) : (
                    <SelectItem value="__global__">自动分配个人独立 IP</SelectItem>
                  )}
                  {isEnterprise && <SelectItem value="direct">直连（不使用代理）</SelectItem>}
                  {enabledProxies.length > 0 && (
                    <SelectGroup>
                      <SelectLabel>代理池</SelectLabel>
                      {enabledProxies.map((p) => (
                        <SelectItem key={p.id} value={p.url}>
                          {p.label ? `${p.label} | ` : ''}{proxyDisplayHost(p.url)} · {p.enabledCredentialCount ?? p.credentialCount} 个启用账号
                        </SelectItem>
                      ))}
                    </SelectGroup>
                  )}
                  <SelectItem value="__custom__">手动输入...</SelectItem>
                </SelectContent>
              </Select>

              {/* 自定义 URL 手动输入框 */}
              {showManualInput && (
                <Textarea
                  placeholder='自定义代理 URL；多个用逗号/空格/换行分隔，可加入 direct'
                  value={proxyUrl}
                  onChange={(e) => setProxyUrl(e.target.value)}
                  disabled={isPending}
                  className="min-h-[76px] font-mono text-sm"
                />
              )}

              {/* 代理认证（仅在需要时显示） */}
              <div className="grid grid-cols-2 gap-2">
                <Input
                  id="proxyUsername"
                  placeholder="代理用户名（留空不修改）"
                  value={proxyUsername}
                  onChange={(e) => setProxyUsername(e.target.value)}
                  disabled={isPending}
                />
                <Input
                  id="proxyPassword"
                  type="password"
                  placeholder="代理密码（留空不修改）"
                  value={proxyPassword}
                  onChange={(e) => setProxyPassword(e.target.value)}
                  disabled={isPending}
                />
              </div>
              <p className="text-xs text-muted-foreground">
                用户名/密码留空不改；多代理会轮询
              </p>
            </div>
          </div>

          <DialogFooter className="shrink-0 border-t pt-3">
            <Button
              type="button"
              variant="outline"
              onClick={() => onOpenChange(false)}
              disabled={isPending}
            >
              取消
            </Button>
            <Button type="submit" disabled={isPending}>
              {isPending ? '保存中...' : '保存'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
