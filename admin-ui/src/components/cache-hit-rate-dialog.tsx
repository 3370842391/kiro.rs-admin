import { useEffect, useState } from 'react'
import { Gauge, RotateCcw } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter,
} from '@/components/ui/dialog'
import { useCacheHitRate, useSetCacheHitRate } from '@/hooks/use-credentials'
import { extractErrorMessage } from '@/lib/utils'

interface CacheHitRateDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * 缓存命中率整形区间设置。
 *
 * 上游不下发真实缓存 token，中转层自行模拟；本旋钮把最终呈现（newapi 计费用量）的
 * 命中率 read/(input+read) **钳制**进 [min, max]：低于 min 提到 min，高于 max 压到 max，
 * 区间内保留真实模拟值。整形只在 input↔cache_read 之间挪，保持总量不变、creation 不动。
 * min=0 && max=0 = 关闭整形（默认，零行为变化）。
 */
export function CacheHitRateDialog({ open, onOpenChange }: CacheHitRateDialogProps) {
  const { data, isLoading } = useCacheHitRate()
  const { mutate: save, isPending: saving } = useSetCacheHitRate()

  const [minPct, setMinPct] = useState<number>(0)
  const [maxPct, setMaxPct] = useState<number>(0)

  useEffect(() => {
    if (!data) return
    setMinPct(data.minPct)
    setMaxPct(data.maxPct)
  }, [data])

  const clampPct = (v: number) => Math.min(100, Math.max(0, Math.round(v) || 0))

  const handleSave = () => {
    // 两值非零时校验 min<=max（与后端一致，前端先拦一道给即时反馈）
    if (minPct > 0 && maxPct > 0 && minPct > maxPct) {
      toast.error('下界不能大于上界')
      return
    }
    save(
      { minPct, maxPct },
      {
        onSuccess: (res) => {
          toast.success(
            res.enabled
              ? `缓存命中率区间已设为 [${res.minPct}%, ${res.maxPct}%]`
              : '缓存命中率整形已关闭',
          )
          onOpenChange(false)
        },
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      },
    )
  }

  const reset = () => {
    setMinPct(0)
    setMaxPct(0)
  }

  const enabled = minPct > 0 || maxPct > 0

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Gauge className="h-4 w-4" />
            缓存命中率整形
          </DialogTitle>
          <DialogDescription>
            把呈现给下游的命中率钳制进区间。低于下界提到下界（冷启动 0% 会被抬起），
            高于上界压到上界，区间内保留真实值。只改 input↔cache_read 配比，计费总量不变。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <p className="py-6 text-center text-sm text-muted-foreground">加载中…</p>
        ) : (
          <div className="space-y-4 py-2">
            <label className="flex items-center gap-3 text-sm">
              <span className="w-24 text-muted-foreground">最低命中率</span>
              <Input
                type="number"
                min={0}
                max={100}
                value={minPct}
                onChange={(e) => setMinPct(clampPct(Number(e.target.value)))}
                className="h-8 w-24"
              />
              <span className="text-xs text-muted-foreground">%</span>
            </label>

            <label className="flex items-center gap-3 text-sm">
              <span className="w-24 text-muted-foreground">最高命中率</span>
              <Input
                type="number"
                min={0}
                max={100}
                value={maxPct}
                onChange={(e) => setMaxPct(clampPct(Number(e.target.value)))}
                className="h-8 w-24"
              />
              <span className="text-xs text-muted-foreground">%</span>
            </label>

            <div className="rounded-md bg-muted/50 px-3 py-2 text-xs leading-relaxed text-muted-foreground">
              <p>
                当前状态：
                <span className={enabled ? 'font-medium text-foreground' : ''}>
                  {enabled ? ` 整形开启 [${minPct}%, ${maxPct}%]` : ' 已关闭（0 / 0，按真实模拟值呈现）'}
                </span>
              </p>
              <ul className="mt-1 list-disc pl-4">
                <li>两者都填 0 = 关闭整形（默认，零行为变化）</li>
                <li>只填最低（最高留 0）= 只抬低命中率，不压高的</li>
                <li>只填最高（最低留 0）= 只压高命中率，不抬低的</li>
                <li>常用区间：90 – 99</li>
              </ul>
            </div>
          </div>
        )}

        <DialogFooter className="gap-2 sm:justify-between">
          <Button type="button" variant="outline" size="sm" onClick={reset} disabled={saving}>
            <RotateCcw className="mr-1 h-3.5 w-3.5" />
            关闭整形
          </Button>
          <Button type="button" size="sm" onClick={handleSave} disabled={saving || isLoading}>
            {saving ? '保存中…' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
