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
import { extractErrorMessage, maskProxyUrl } from '@/lib/utils'
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
  const [manualMode, setManualMode] = useState(false)

  const groupOptions = useGroupOptions()

  const { data: proxyPool } = useQuery({
    queryKey: ['proxy-pool'],
    queryFn: getProxyPool,
    enabled: open,
  })

  // 每次打开时重置表单为当前凭据值
  useEffect(() => {
    if (open) {
      setNickname(credential.nickname ?? '')
      setApiRegion(credential.apiRegion ?? '')
      setEmail(credential.email ?? '')
      setProxyUrl(credential.proxyUrl ?? '')
      setProxyUsername('')
      setProxyPassword('')
      setGroups(credential.groups ?? [])
      setSourceChannel(credential.sourceChannel ?? '')
      setRpmLimit(String(credential.rpmLimit ?? 10))
      setManualMode(false)
    }
  }, [open, credential])

  const { mutate, isPending } = useUpdateCredential()
  const isApiKey = credential.authMethod === 'api_key'

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
      <DialogContent className="max-h-[85vh] overflow-y-auto sm:max-w-md">
        <DialogHeader>
          <DialogTitle>
            编辑凭据 #{credential.id}
          </DialogTitle>
        </DialogHeader>

        <form onSubmit={handleSubmit}>
          <div className="space-y-4 py-4">
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
              <p className="text-xs text-muted-foreground">
                留空则显示凭据 ID，清除请提交空值
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
                绑定了某分组的客户端 Key 只会调度到含该分组的账号。不选表示不属于任何分组。
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
              <p className="text-xs text-muted-foreground">
                纯备注，标记此账号的购买来源/渠道，便于追踪。留空表示清除。
              </p>
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
                滑动窗口每分钟最多请求数。默认 10；填 0 表示不限速。
              </p>
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
                <SelectTrigger className="h-10 rounded-xl px-3.5">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="__global__">使用全局代理配置</SelectItem>
                  <SelectItem value="direct">直连（不使用代理）</SelectItem>
                  {enabledProxies.length > 0 && (
                    <SelectGroup>
                      <SelectLabel>代理池</SelectLabel>
                      {enabledProxies.map((p) => (
                        <SelectItem key={p.id} value={p.url}>
                          {p.label ? `${p.label} | ${maskProxyUrl(p.url)}` : maskProxyUrl(p.url)}
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
                用户名/密码留空表示不修改；多个代理会随机轮询，失败时自动换下一个
              </p>
            </div>
          </div>

          <DialogFooter>
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
