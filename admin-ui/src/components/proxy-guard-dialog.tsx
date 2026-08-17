import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { AlertTriangle, ShieldAlert } from 'lucide-react'
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
import { getProxyGuardConfig, runProxyGuard, setProxyGuardConfig } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface ProxyGuardDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

function parseCount(raw: string, label: string, min: number): number {
  const trimmed = raw.trim()
  const n = trimmed === '' ? min : Number(trimmed)
  if (!Number.isInteger(n) || n < min) {
    throw new Error(`${label}必须是不小于 ${min} 的整数`)
  }
  if (n > 8760) {
    throw new Error(`${label}超出上限 8760`)
  }
  return n
}

export function ProxyGuardDialog({ open, onOpenChange }: ProxyGuardDialogProps) {
  const queryClient = useQueryClient()

  const { data, isLoading } = useQuery({
    queryKey: ['proxy-guard'],
    queryFn: getProxyGuardConfig,
    enabled: open,
  })

  const [enabled, setEnabled] = useState(true)
  const [banThreshold, setBanThreshold] = useState('2')
  const [windowHours, setWindowHours] = useState('24')
  const [minAssignable, setMinAssignable] = useState('3')
  const [migrateSurvivors, setMigrateSurvivors] = useState(true)
  const [autoReleaseHours, setAutoReleaseHours] = useState('0')

  useEffect(() => {
    if (!open || !data) return
    setEnabled(data.enabled)
    setBanThreshold(String(data.banThreshold))
    setWindowHours(String(data.windowHours))
    setMinAssignable(String(data.minAssignable))
    setMigrateSurvivors(data.migrateSurvivors)
    setAutoReleaseHours(String(data.autoReleaseHours))
  }, [open, data])

  const saveMutation = useMutation({
    mutationFn: setProxyGuardConfig,
    onSuccess: () => {
      toast.success('烧号隔离策略已保存')
      queryClient.invalidateQueries({ queryKey: ['proxy-guard'] })
      onOpenChange(false)
    },
    onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
  })

  const runMutation = useMutation({
    mutationFn: runProxyGuard,
    onSuccess: (res) => {
      const parts: string[] = []
      if (res.quarantined.length > 0) parts.push(`隔离 ${res.quarantined.length} 个出口`)
      if (res.migrated > 0) parts.push(`迁移 ${res.migrated} 个号`)
      if (res.released.length > 0) parts.push(`解除 ${res.released.length} 个`)
      if (res.skippedForCapacity.length > 0) {
        parts.push(`${res.skippedForCapacity.length} 个因出口不足跳过`)
      }
      toast.success(parts.length > 0 ? parts.join('，') : '没有出口达到隔离阈值')
      queryClient.invalidateQueries({ queryKey: ['proxy-pool'] })
      queryClient.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (err) => toast.error(`执行失败: ${extractErrorMessage(err)}`),
  })

  const handleSave = () => {
    try {
      saveMutation.mutate({
        enabled,
        banThreshold: parseCount(banThreshold, '封号阈值', 1),
        windowHours: parseCount(windowHours, '观察窗口', 1),
        minAssignable: parseCount(minAssignable, '保底出口数', 0),
        migrateSurvivors,
        autoReleaseHours: parseCount(autoReleaseHours, '自动解除时长', 0),
      })
    } catch (error) {
      toast.error(extractErrorMessage(error))
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ShieldAlert className="h-4 w-4" />
            烧号隔离
          </DialogTitle>
        </DialogHeader>

        <div className="flex-1 overflow-y-auto space-y-4 py-2">
          <p className="text-xs text-muted-foreground">
            窗口内封够指定数量的号，就直接停用这个出口，并把它上面还活着的号改绑到干净出口。
            与代理池的「降权」不同：降权只是排序靠后，钉死在某个出口上的号照样会走它。
          </p>

          {isLoading && (
            <div className="text-sm text-muted-foreground py-4 text-center">加载中...</div>
          )}

          <div className="flex items-center justify-between gap-3 rounded-md border p-3">
            <div className="space-y-0.5">
              <div className="text-sm font-medium">启用隔离守卫</div>
              <p className="text-xs text-muted-foreground">
                关闭后只保留统计与降权，不会自动停用任何出口。
              </p>
            </div>
            <Switch checked={enabled} onCheckedChange={setEnabled} />
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <label className="text-sm font-medium">封号阈值</label>
              <Input
                value={banThreshold}
                onChange={(e) => setBanThreshold(e.target.value)}
                placeholder="2"
                inputMode="numeric"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">
                窗口内封够几个号就隔离。取 2 是因为单个号被封可能是这个号自己的问题。
              </p>
            </div>
            <div className="space-y-1">
              <label className="text-sm font-medium">观察窗口（小时）</label>
              <Input
                value={windowHours}
                onChange={(e) => setWindowHours(e.target.value)}
                placeholder="24"
                inputMode="numeric"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">
                只看近期封号。机场的出口 IP 会轮换，上周脏过的线路今天可能已经换了。
              </p>
            </div>
          </div>

          <div className="grid grid-cols-2 gap-3">
            <div className="space-y-1">
              <label className="text-sm font-medium">保底可用出口数</label>
              <Input
                value={minAssignable}
                onChange={(e) => setMinAssignable(e.target.value)}
                placeholder="3"
                inputMode="numeric"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">
                隔离后池里至少要剩这么多可分配出口，否则跳过隔离只告警。
              </p>
            </div>
            <div className="space-y-1">
              <label className="text-sm font-medium">自动解除（小时）</label>
              <Input
                value={autoReleaseHours}
                onChange={(e) => setAutoReleaseHours(e.target.value)}
                placeholder="0"
                inputMode="numeric"
                disabled={!enabled}
              />
              <p className="text-xs text-muted-foreground">
                0 = 永不自动解除，只能手动重新启用。
              </p>
            </div>
          </div>

          <div className="flex items-center justify-between gap-3 rounded-md border p-3">
            <div className="space-y-0.5">
              <div className="text-sm font-medium">迁移幸存账号</div>
              <p className="text-xs text-muted-foreground">
                隔离时把该出口上还活着的号改绑到封号数低于阈值的出口，按当前负载最低的挑。
              </p>
            </div>
            <Switch
              checked={migrateSurvivors}
              onCheckedChange={setMigrateSurvivors}
              disabled={!enabled}
            />
          </div>

          {!migrateSurvivors && enabled && (
            <div className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              <AlertTriangle className="h-4 w-4 shrink-0 mt-0.5" />
              <span>
                关掉迁移后，被隔离出口上的号会因为找不到可用代理而请求失败（不会退化成直连），
                需要你手动改绑。除非你打算自己处理，否则保持开启。
              </span>
            </div>
          )}
        </div>

        <DialogFooter className="gap-2 sm:justify-between">
          <Button
            variant="outline"
            onClick={() => runMutation.mutate()}
            disabled={runMutation.isPending || isLoading}
            title="按当前已保存的策略立即执行一轮隔离与迁移"
          >
            {runMutation.isPending ? '执行中...' : '立即执行一轮'}
          </Button>
          <div className="flex gap-2">
            <Button variant="outline" onClick={() => onOpenChange(false)}>
              取消
            </Button>
            <Button onClick={handleSave} disabled={saveMutation.isPending || isLoading}>
              {saveMutation.isPending ? '保存中...' : '保存'}
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
