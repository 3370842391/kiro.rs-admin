import { useEffect, useMemo, useState } from 'react'
import { Network, ArrowUp, ArrowDown, RotateCcw } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription, DialogFooter,
} from '@/components/ui/dialog'
import { useEndpointChains, useSetEndpointChains } from '@/hooks/use-credentials'
import type { EndpointBucketOption } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'

interface EndpointChainsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/** 主端点中文标签 */
const PRIMARY_LABEL: Record<string, string> = {
  ide: 'Kiro IDE 协议',
  cli: 'CLI 协议',
}

/** 备用桶说明（把 Kiro-Go 生产实测结论写进 UI，引导正确选择） */
const BUCKET_HINT: Record<string, string> = {
  runtime:
    'runtime.kiro.dev — 独立限流桶（独立域名），Kiro-Go 实测最有效的跨桶救援目标，建议保留。',
  codewhisperer:
    'codewhisperer — 与 q 同 host 不同服务。Kiro-Go 实测与 q 共用账号级桶，降级到它大概率仍 429，且可能加重风控。',
  amazonq:
    'amazonq — 与 q 同 host 不同服务。同上，Kiro-Go 实测与 q 共用账号级桶，收益存疑。',
  runtime_cli: 'runtime_cli — CLI 协议的 runtime 桶。',
  cli: 'cli — CLI 协议主端点桶。',
  ide: 'ide — Kiro IDE 主端点桶。',
}

/**
 * 429 降级桶链配置：主端点 429 时「换桶不换号」依次尝试的备用桶（有序、可勾选）。
 * 未配置时走各端点静态默认链。空选 = 该主端点不降级。
 */
export function EndpointChainsDialog({ open, onOpenChange }: EndpointChainsDialogProps) {
  const { data, isLoading } = useEndpointChains()
  const { mutate: save, isPending: saving } = useSetEndpointChains()

  // 本地编辑态：primary -> 有序桶名数组
  const [draft, setDraft] = useState<Record<string, string[]>>({})
  const [maxAttempts, setMaxAttempts] = useState<number>(6)
  const [idleTimeout, setIdleTimeout] = useState<number>(120)

  // 载入服务端当前值到编辑态
  useEffect(() => {
    if (!data) return
    setDraft(JSON.parse(JSON.stringify(data.chains)))
    setMaxAttempts(data.maxBucketAttemptsPerRequest)
    setIdleTimeout(data.streamIdleTimeoutSecs)
  }, [data])

  const primaries = useMemo(
    () => Object.keys(data?.availableBuckets ?? {}).sort(),
    [data],
  )

  const toggleBucket = (primary: string, bucket: string) => {
    setDraft((prev) => {
      const cur = prev[primary] ?? []
      const next = cur.includes(bucket)
        ? cur.filter((b) => b !== bucket)
        : [...cur, bucket]
      return { ...prev, [primary]: next }
    })
  }

  const move = (primary: string, idx: number, dir: -1 | 1) => {
    setDraft((prev) => {
      const cur = [...(prev[primary] ?? [])]
      const j = idx + dir
      if (j < 0 || j >= cur.length) return prev
      ;[cur[idx], cur[j]] = [cur[j], cur[idx]]
      return { ...prev, [primary]: cur }
    })
  }

  const resetToDefaults = () => {
    if (!data) return
    setDraft(JSON.parse(JSON.stringify(data.defaults)))
    toast.info('已重置为静态默认链（未保存）')
  }

  const handleSave = () => {
    save(
      { chains: draft, maxBucketAttemptsPerRequest: maxAttempts, streamIdleTimeoutSecs: idleTimeout },
      {
        onSuccess: () => {
          toast.success('降级桶链已保存')
          onOpenChange(false)
        },
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      },
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Network className="h-4 w-4" />
            429 降级桶链
          </DialogTitle>
          <DialogDescription>
            主端点被 429 限流时，用<b>同一张凭据</b>依次尝试下列备用桶（换桶不换号），
            命中第一个 2xx 即返回。勾选启用、上下箭头排序。不勾选任何桶 = 该主端点不降级。
            未配置时走内置默认链。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <div className="py-8 text-center text-sm text-muted-foreground">加载中…</div>
        ) : (
          <div className="space-y-5 py-1">
            {primaries.map((primary) => {
              const options: EndpointBucketOption[] = data?.availableBuckets[primary] ?? []
              const selected = draft[primary] ?? []
              // 已选的按 draft 顺序在前，未选的按名字排在后
              const unselected = options
                .filter((o) => !selected.includes(o.name))
                .map((o) => o.name)
              return (
                <div key={primary} className="rounded-lg border p-3">
                  <div className="mb-2 text-sm font-medium">
                    {PRIMARY_LABEL[primary] ?? primary}
                    <span className="ml-2 font-mono text-xs text-muted-foreground">{primary}</span>
                  </div>

                  {/* 已选（有序） */}
                  {selected.length > 0 && (
                    <div className="mb-2 space-y-1">
                      {selected.map((bucket, idx) => (
                        <div
                          key={bucket}
                          className="flex items-center gap-2 rounded-md bg-muted/50 px-2 py-1.5"
                        >
                          <span className="w-5 text-center text-xs text-muted-foreground">
                            {idx + 1}
                          </span>
                          <Checkbox
                            checked
                            onCheckedChange={() => toggleBucket(primary, bucket)}
                          />
                          <div className="flex-1">
                            <span className="font-mono text-[13px]">{bucket}</span>
                            <p className="text-[11px] leading-tight text-muted-foreground">
                              {BUCKET_HINT[bucket] ?? ''}
                            </p>
                          </div>
                          <Button
                            type="button"
                            size="icon"
                            variant="ghost"
                            className="h-6 w-6"
                            disabled={idx === 0}
                            onClick={() => move(primary, idx, -1)}
                          >
                            <ArrowUp className="h-3.5 w-3.5" />
                          </Button>
                          <Button
                            type="button"
                            size="icon"
                            variant="ghost"
                            className="h-6 w-6"
                            disabled={idx === selected.length - 1}
                            onClick={() => move(primary, idx, 1)}
                          >
                            <ArrowDown className="h-3.5 w-3.5" />
                          </Button>
                        </div>
                      ))}
                    </div>
                  )}

                  {/* 未选 */}
                  {unselected.length > 0 && (
                    <div className="space-y-1">
                      {unselected.map((bucket) => (
                        <div key={bucket} className="flex items-center gap-2 px-2 py-1">
                          <span className="w-5" />
                          <Checkbox
                            checked={false}
                            onCheckedChange={() => toggleBucket(primary, bucket)}
                          />
                          <div className="flex-1">
                            <span className="font-mono text-[13px] text-muted-foreground">
                              {bucket}
                            </span>
                            <p className="text-[11px] leading-tight text-muted-foreground/70">
                              {BUCKET_HINT[bucket] ?? ''}
                            </p>
                          </div>
                        </div>
                      ))}
                    </div>
                  )}

                  {options.length === 0 && (
                    <p className="text-xs text-muted-foreground">该协议无可选备用桶。</p>
                  )}
                </div>
              )
            })}

            <label className="flex items-center gap-3 text-sm">
              <span className="text-muted-foreground">单请求桶尝试上限</span>
              <Input
                type="number"
                min={0}
                value={maxAttempts}
                onChange={(e) => setMaxAttempts(Math.max(0, Number(e.target.value) || 0))}
                className="h-8 w-24"
              />
              <span className="text-xs text-muted-foreground">0 = 不限；防止链长×重试放大成上百次上游调用</span>
            </label>

            <label className="flex items-center gap-3 text-sm">
              <span className="text-muted-foreground">流式空闲超时（秒）</span>
              <Input
                type="number"
                min={0}
                value={idleTimeout}
                onChange={(e) => setIdleTimeout(Math.max(0, Number(e.target.value) || 0))}
                className="h-8 w-24"
              />
              <span className="text-xs text-muted-foreground">上游 200 后连续无字节多久主动收尾；0 = 关闭（仅靠绝对超时兜底）</span>
            </label>
          </div>
        )}

        <DialogFooter className="gap-2 sm:justify-between">
          <Button type="button" variant="outline" size="sm" onClick={resetToDefaults} disabled={saving}>
            <RotateCcw className="mr-1 h-3.5 w-3.5" />
            恢复默认
          </Button>
          <Button type="button" size="sm" onClick={handleSave} disabled={saving || isLoading}>
            {saving ? '保存中…' : '保存'}
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}
