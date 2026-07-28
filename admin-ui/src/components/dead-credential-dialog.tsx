import { useEffect, useState } from 'react'
import { toast } from 'sonner'
import { Trash2 } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  useDeadCredentialConfig,
  useSetDeadCredentialConfig,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

const MAX_RETENTION_HOURS = 8760

interface DeadCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * 死号治理设置。
 *
 * 此前这个配置塞在「请求日志 → 治理设置」里，但它管的是凭据生命周期、不是日志落盘，
 * 放在那儿既不好找也归类错了。独立出来挂到凭据管理的工具栏。
 */
export function DeadCredentialDialog({ open, onOpenChange }: DeadCredentialDialogProps) {
  const { data: config, isLoading } = useDeadCredentialConfig()
  const { mutate: save, isPending } = useSetDeadCredentialConfig()
  const [hours, setHours] = useState('')

  // 弹窗打开时以服务端值为准，避免上次编辑的草稿残留
  useEffect(() => {
    if (open && config) setHours(String(config.retentionHours))
  }, [open, config])

  const toggleAutoDelete = (enabled: boolean) => {
    save(
      { autoDelete: enabled },
      {
        onSuccess: () => toast.success(enabled ? '已开启自动删除' : '已关闭自动删除，死号将永久保留'),
        onError: (error) => toast.error(`保存失败：${extractErrorMessage(error)}`),
      },
    )
  }

  const submitHours = (e: React.FormEvent) => {
    e.preventDefault()
    const parsed = Number.parseInt(hours, 10)
    if (!Number.isFinite(parsed) || parsed < 1 || parsed > MAX_RETENTION_HOURS) {
      toast.error(`保留小时数需在 1..=${MAX_RETENTION_HOURS}`)
      return
    }
    save(
      { retentionHours: parsed },
      {
        onSuccess: () => toast.success('保留时长已更新'),
        onError: (error) => toast.error(`保存失败：${extractErrorMessage(error)}`),
      },
    )
  }

  const autoDelete = config?.autoDelete ?? false
  const dirty = config != null && hours.trim() !== String(config.retentionHours)

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Trash2 className="h-4 w-4" />
            死号治理
          </DialogTitle>
          <DialogDescription>
            账号被上游封禁（403）后先禁用留档，便于查看存活时长与封号原因，之后再决定是否清理。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="py-6 text-center text-sm text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-4 py-2">
            <div className="flex items-start justify-between gap-3 rounded-lg bg-secondary/40 px-3 py-2.5">
              <div className="min-w-0 text-sm">
                <div className="font-medium text-foreground">保留期结束后自动删除</div>
                <div className="mt-0.5 leading-snug text-muted-foreground">
                  关闭后死号永久保留，只是被禁用。可用凭据列表的「含已禁用」开关决定是否显示。
                </div>
              </div>
              <Switch
                checked={autoDelete}
                onCheckedChange={toggleAutoDelete}
                disabled={isPending}
                aria-label="保留期结束后自动删除判死账号"
              />
            </div>

            <form onSubmit={submitHours} className="space-y-1.5">
              <label htmlFor="deadRetentionHours" className="text-sm font-medium">
                保留时长（小时）
              </label>
              <div className="flex items-center gap-2">
                <Input
                  id="deadRetentionHours"
                  type="number"
                  min={1}
                  max={MAX_RETENTION_HOURS}
                  value={hours}
                  onChange={(e) => setHours(e.target.value)}
                  disabled={isPending || !autoDelete}
                  className="h-9"
                />
                <Button
                  type="submit"
                  variant="outline"
                  disabled={isPending || !autoDelete || !dirty}
                >
                  保存
                </Button>
              </div>
              <p className="text-[11px] leading-snug text-muted-foreground">
                用小时而非天：封号往往整批发生，按天保留会在列表里积压上百条死号。
                {!autoDelete && ' 自动删除已关闭，该设置暂不生效。'}
              </p>
            </form>

            <div className="rounded-lg border border-border/60 px-3 py-2.5 text-[12px] leading-relaxed text-muted-foreground">
              <div>
                当前判死账号：
                <span className="font-medium text-foreground">{config?.deadCount ?? 0}</span> 个，
                其中
                <span className="font-medium text-foreground">
                  {config?.autoDeleteEligible ?? 0}
                </span>{' '}
                个会被自动清理。
              </div>
              <div className="mt-1">
                差额是手工添加的账号 —— 它们通常是你手上唯一一份，删掉不可恢复，因此只禁用、
                不参与自动删除。
              </div>
            </div>
          </div>
        )}

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
