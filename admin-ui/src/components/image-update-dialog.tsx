import { useEffect, useState } from 'react'
import { CheckCircle2, RefreshCw, UploadCloud } from 'lucide-react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Progress } from '@/components/ui/progress'
import { applyImageUpdate, checkSystemUpdate } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface ImageUpdateDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * 极简在线更新：只保留「版本号 + 更新按钮 + 进度条」。
 * - 打开时自动查一次版本（后端 30 分钟缓存 + 前端 5 分钟 staleTime，不会打爆 GitHub 限流）。
 * - 「更新并重启」调用 applyImageUpdate：服务端解析最新版本→下载→校验 SHA256→替换 exe→退出，
 *   由容器 restart 策略拉起新版本。真实下载在服务端一次性完成，前端用平滑爬升的进度条表示进行中。
 */
export function ImageUpdateDialog({ open, onOpenChange }: ImageUpdateDialogProps) {
  const queryClient = useQueryClient()
  const [progress, setProgress] = useState(0)

  const { data: updateCheck, isFetching, refetch } = useQuery({
    queryKey: ['system-update-check'],
    queryFn: () => checkSystemUpdate(false),
    enabled: open,
    staleTime: 5 * 60 * 1000,
  })

  const applyMutation = useMutation({
    mutationFn: applyImageUpdate,
    onSuccess: (res) => {
      setProgress(100)
      toast.success(res.message)
      queryClient.invalidateQueries({ queryKey: ['system-update-check'] })
    },
    onError: (err) => {
      setProgress(0)
      toast.error(`更新失败: ${extractErrorMessage(err)}`)
    },
  })

  // 更新进行中：进度条平滑爬升到 ~90%，成功后跳 100%（下载在服务端，无法拿真实百分比）
  useEffect(() => {
    if (!applyMutation.isPending) return
    setProgress(8)
    const timer = setInterval(() => {
      setProgress((p) => (p < 90 ? p + Math.max(1, (90 - p) * 0.12) : p))
    }, 400)
    return () => clearInterval(timer)
  }, [applyMutation.isPending])

  // 关闭弹窗复位进度
  useEffect(() => {
    if (!open) setProgress(0)
  }, [open])

  const canUpdate = !!updateCheck?.hasUpdate && !applyMutation.isPending
  const showProgress = applyMutation.isPending || progress > 0

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent aria-describedby={undefined} className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <UploadCloud className="h-4 w-4" />
            在线更新
          </DialogTitle>
        </DialogHeader>

        <div className="space-y-4 py-2">
          {/* 版本号 */}
          <div className="flex items-center justify-between rounded-md border bg-muted/30 px-3 py-2.5">
            <div className="flex items-baseline gap-2 text-sm">
              <span className="text-muted-foreground">当前版本</span>
              <span className="font-mono font-medium">
                {updateCheck?.currentVersion ? `v${updateCheck.currentVersion}` : '…'}
              </span>
            </div>
            {updateCheck?.hasUpdate ? (
              <Badge variant="success">可更新 → v{updateCheck.latestVersion}</Badge>
            ) : updateCheck?.latestVersion ? (
              <Badge variant="secondary">已是最新</Badge>
            ) : null}
          </div>

          {/* 检查失败时的一行提示（如 GitHub 限流），否则不显示任何多余内容 */}
          {updateCheck?.warning && !showProgress && (
            <div className="text-xs text-destructive">{updateCheck.warning}</div>
          )}

          {/* 进度条：仅更新进行中/完成时出现 */}
          {showProgress && (
            <div className="space-y-1.5">
              <Progress value={progress} />
              <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
                {progress >= 100 ? (
                  <>
                    <CheckCircle2 className="h-3.5 w-3.5 text-emerald-600" />
                    更新完成，服务正在重启…
                  </>
                ) : (
                  <>
                    <RefreshCw className="h-3.5 w-3.5 animate-spin" />
                    正在下载并替换新版本…
                  </>
                )}
              </div>
            </div>
          )}
        </div>

        <DialogFooter className="sm:justify-between gap-2">
          <Button
            type="button"
            variant="ghost"
            size="sm"
            disabled={isFetching || applyMutation.isPending}
            onClick={() => refetch()}
            title="重新检查版本"
          >
            <RefreshCw className={`h-3.5 w-3.5 ${isFetching ? 'animate-spin' : ''}`} />
            <span className="ml-1.5">检查</span>
          </Button>
          <Button
            type="button"
            disabled={!canUpdate}
            onClick={() => applyMutation.mutate()}
            title={
              updateCheck?.hasUpdate
                ? `更新到 v${updateCheck.latestVersion} 并重启`
                : updateCheck?.currentVersion
                  ? '当前已是最新版本'
                  : '正在检查更新…'
            }
          >
            {applyMutation.isPending ? (
              <RefreshCw className="h-4 w-4 mr-2 animate-spin" />
            ) : (
              <UploadCloud className="h-4 w-4 mr-2" />
            )}
            更新并重启
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
