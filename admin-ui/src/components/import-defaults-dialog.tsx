import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { AlertTriangle } from 'lucide-react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { GroupMultiSelect } from '@/components/group-select'
import { useGroupOptions } from '@/hooks/use-groups'
import { getImportDefaults, setImportDefaults } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface ImportDefaultsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

function parseCount(raw: string, label: string): number {
  const trimmed = raw.trim()
  if (!trimmed) return 0
  const n = Number(trimmed)
  if (!Number.isInteger(n) || n < 0) {
    throw new Error(`${label}必须是不小于 0 的整数`)
  }
  if (n > 100_000) {
    throw new Error(`${label}超出上限 100000`)
  }
  return n
}

export function ImportDefaultsDialog({ open, onOpenChange }: ImportDefaultsDialogProps) {
  const queryClient = useQueryClient()
  const groupOptions = useGroupOptions()

  const { data, isLoading } = useQuery({
    queryKey: ['import-defaults'],
    queryFn: getImportDefaults,
    enabled: open,
  })

  const [rpmLimit, setRpmLimit] = useState('')
  const [maxConcurrency, setMaxConcurrency] = useState('')
  const [priority, setPriority] = useState('')
  const [groups, setGroups] = useState<string[]>([])
  const [sourceChannel, setSourceChannel] = useState('')
  const [autoAssignProxy, setAutoAssignProxy] = useState(true)
  const [avoidRiskyProxies, setAvoidRiskyProxies] = useState(true)

  useEffect(() => {
    if (!open || !data) return
    setRpmLimit(String(data.rpmLimit))
    setMaxConcurrency(String(data.maxConcurrency))
    setPriority(String(data.priority))
    setGroups(data.groups)
    setSourceChannel(data.sourceChannel)
    setAutoAssignProxy(data.autoAssignProxy)
    setAvoidRiskyProxies(data.avoidRiskyProxies)
  }, [open, data])

  const saveMutation = useMutation({
    mutationFn: setImportDefaults,
    onSuccess: () => {
      toast.success('导入默认值已保存')
      queryClient.invalidateQueries({ queryKey: ['import-defaults'] })
      onOpenChange(false)
    },
    onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
  })

  const handleSave = () => {
    try {
      saveMutation.mutate({
        rpmLimit: parseCount(rpmLimit, 'RPM'),
        maxConcurrency: parseCount(maxConcurrency, '最大并发'),
        priority: parseCount(priority, '优先级'),
        groups,
        sourceChannel: sourceChannel.trim(),
        autoAssignProxy,
        avoidRiskyProxies,
      })
    } catch (error) {
      toast.error(extractErrorMessage(error))
    }
  }

  const rpmIsUnlimited = rpmLimit.trim() === '0'

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>导入默认值</DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-2">
          <p className="text-xs text-muted-foreground">
            批量导入与单个添加打开时会预填这些值，当次仍可改。与 Key Supplier 的
            「公共导入设置」相互独立——那份只管 webhook 自动采购。
          </p>

          {isLoading && (
            <div className="text-sm text-muted-foreground py-4 text-center">加载中...</div>
          )}

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <label className="text-sm font-medium">默认 RPM</label>
              <Input
                value={rpmLimit}
                onChange={(e) => setRpmLimit(e.target.value)}
                placeholder="10"
                inputMode="numeric"
              />
              <p className="text-xs text-muted-foreground">每分钟请求数上限，0 = 不限速。</p>
            </div>
            <div className="space-y-1">
              <label className="text-sm font-medium">默认最大并发</label>
              <Input
                value={maxConcurrency}
                onChange={(e) => setMaxConcurrency(e.target.value)}
                placeholder="0"
                inputMode="numeric"
              />
              <p className="text-xs text-muted-foreground">
                单账号同时在途请求上限，0 = 不限并发。
              </p>
            </div>
          </div>

          {rpmIsUnlimited && (
            <div className="flex items-start gap-2 rounded-md border border-yellow-500/40 bg-yellow-500/10 px-3 py-2 text-xs text-yellow-700 dark:text-yellow-300">
              <AlertTriangle className="h-4 w-4 shrink-0 mt-0.5" />
              <span>
                RPM 设为 0 表示新导入的号完全不限速。账号级风控对高频请求敏感，
                裸奔的号容易很快被判死。
              </span>
            </div>
          )}

          <div className="space-y-1">
            <label className="text-sm font-medium">默认分组</label>
            <GroupMultiSelect value={groups} options={groupOptions} onChange={setGroups} />
            <p className="text-xs text-muted-foreground">
              导入时与 JSON 内自带的 groups 取并集。
            </p>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <label className="text-sm font-medium">默认优先级</label>
              <Input
                value={priority}
                onChange={(e) => setPriority(e.target.value)}
                placeholder="0"
                inputMode="numeric"
              />
              <p className="text-xs text-muted-foreground">数值越小越优先。</p>
            </div>
            <div className="space-y-1">
              <label className="text-sm font-medium">来源渠道</label>
              <Input
                value={sourceChannel}
                onChange={(e) => setSourceChannel(e.target.value)}
                placeholder="留空则不写"
              />
              <p className="text-xs text-muted-foreground">用于区分号的来路，便于对账。</p>
            </div>
          </div>

          <div className="rounded-md border p-3 space-y-3">
            <div className="flex items-center justify-between gap-3">
              <div className="space-y-0.5">
                <div className="text-sm font-medium">自动分配代理</div>
                <p className="text-xs text-muted-foreground">
                  导入时没指定代理的号，自动从代理池挑一个。
                </p>
              </div>
              <Switch checked={autoAssignProxy} onCheckedChange={setAutoAssignProxy} />
            </div>
            <div className="flex items-center justify-between gap-3">
              <div className="space-y-0.5">
                <div className="text-sm font-medium">跳过已降权出口</div>
                <p className="text-xs text-muted-foreground">
                  自动分配时避开烧号多的 IP。新号最经不起脏出口，刚导入就被判死连
                  观察窗口都没有。全池都被降权时不过滤，否则一个也分不出去。
                </p>
              </div>
              <Switch
                checked={avoidRiskyProxies}
                onCheckedChange={setAvoidRiskyProxies}
                disabled={!autoAssignProxy}
              />
            </div>
          </div>
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            取消
          </Button>
          <Button onClick={handleSave} disabled={saveMutation.isPending || isLoading}>
            {saveMutation.isPending ? '保存中...' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
