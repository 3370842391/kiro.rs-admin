import { MessageSquareWarning } from 'lucide-react'
import { toast } from 'sonner'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  useCompatibilityConfig,
  useSetCompatibilityConfig,
} from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

interface CompatibilitySettingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

export function CompatibilitySettingsDialog({
  open,
  onOpenChange,
}: CompatibilitySettingsDialogProps) {
  const { data, isLoading, error } = useCompatibilityConfig()
  const { mutate, isPending } = useSetCompatibilityConfig()

  const updateEmptyUserCompat = (enabled: boolean) => {
    mutate(
      { emptyUserMessageCompat: enabled },
      {
        onSuccess: () => toast.success(enabled ? '空 user 兼容已开启' : '空 user 兼容已关闭'),
        onError: (updateError) =>
          toast.error(`保存失败: ${extractErrorMessage(updateError)}`),
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !isPending && onOpenChange(next)}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <MessageSquareWarning className="h-4 w-4" />
            协议兼容设置
          </DialogTitle>
          <DialogDescription>
            仅控制已知的边界请求，不改变普通对话、工具、图片或文档请求。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <p className="py-6 text-center text-sm text-muted-foreground">加载配置中…</p>
        ) : error || !data ? (
          <p role="alert" className="rounded-lg border border-destructive/30 p-3 text-sm text-destructive">
            加载失败: {extractErrorMessage(error)}
          </p>
        ) : (
          <div className="flex items-start justify-between gap-4 rounded-xl border p-4">
            <div className="space-y-1">
              <label htmlFor="empty-user-message-compat" className="text-sm font-medium">
                空 user 请求兼容
              </label>
              <p className="text-xs leading-relaxed text-muted-foreground">
                默认关闭时，本地返回清晰的 400。开启后，仅当 system 非空、唯一 user 文本为空且无工具、图片或文档时，向上游补入 Continue.
              </p>
            </div>
            <Switch
              id="empty-user-message-compat"
              checked={data.emptyUserMessageCompat}
              disabled={isPending}
              onCheckedChange={updateEmptyUserCompat}
              aria-label="空 user 请求兼容"
            />
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}
