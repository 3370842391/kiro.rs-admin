import { useEffect, useState, type FormEvent } from 'react'
import { Images, Save } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { useImageBudget, useSetImageBudget } from '@/hooks/use-image-budget'
import { validateImageBudget } from '@/lib/image-budget'
import { extractErrorMessage } from '@/lib/utils'
import type { ImageBudgetConfig } from '@/types/api'

interface ImageBudgetDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

const DEFAULT_DRAFT: ImageBudgetConfig = {
  enabled: true,
  totalBase64BudgetBytes: 819_200,
  hardBase64LimitBytes: 8 * 1024 * 1024,
  historyMaxDimension: 1280,
  historyJpegQuality: 72,
  retryHistoryMaxDimension: 960,
  retryHistoryJpegQuality: 60,
}

export function ImageBudgetDialog({ open, onOpenChange }: ImageBudgetDialogProps) {
  const { data, isLoading, error } = useImageBudget()
  const { mutate: save, isPending } = useSetImageBudget()
  const [draft, setDraft] = useState<ImageBudgetConfig>(DEFAULT_DRAFT)
  const [validationError, setValidationError] = useState<string | null>(null)

  useEffect(() => {
    if (data) {
      setDraft(data)
      setValidationError(null)
    }
  }, [data])

  const setNumber = (key: keyof ImageBudgetConfig, raw: string) => {
    const value = Number(raw)
    setDraft((current) => ({
      ...current,
      [key]: Number.isFinite(value) ? value : 0,
    }))
  }

  const handleSave = (event: FormEvent) => {
    event.preventDefault()
    const message = validateImageBudget(draft)
    setValidationError(message)
    if (message) return
    save(draft, {
      onSuccess: () => {
        toast.success('图片总预算已保存并立即生效')
        onOpenChange(false)
      },
      onError: (saveError) => toast.error(`保存失败: ${extractErrorMessage(saveError)}`),
    })
  }

  return (
    <Dialog open={open} onOpenChange={(next) => !isPending && onOpenChange(next)}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Images className="h-4 w-4" />
            图片总预算
          </DialogTitle>
          <DialogDescription>
            用软目标控制历史图片压缩，并用独立硬上限防止过大的本地请求。
          </DialogDescription>
        </DialogHeader>

        {isLoading ? (
          <p className="py-10 text-center text-sm text-muted-foreground">加载图片预算…</p>
        ) : error || !data ? (
          <div role="alert" className="rounded-xl border border-destructive/30 bg-destructive/5 p-4 text-sm text-destructive">
            加载失败：{extractErrorMessage(error)}
          </div>
        ) : (
          <form onSubmit={handleSave} className="space-y-5">
            <section className="flex items-center justify-between gap-4 rounded-2xl border border-border/70 p-4">
              <div>
                <label htmlFor="image-budget-enabled" className="text-sm font-medium">
                  启用图片预算治理
                </label>
                <p className="mt-1 text-xs text-muted-foreground">
                  关闭后保持原图透传，也不会生成超限降级请求体。
                </p>
              </div>
              <Switch
                id="image-budget-enabled"
                checked={draft.enabled}
                onCheckedChange={(enabled) => setDraft((current) => ({ ...current, enabled }))}
                disabled={isPending}
              />
            </section>

            <section className="grid gap-4 rounded-2xl border border-border/70 p-4 sm:grid-cols-2">
              <BudgetField
                id="image-budget-kib"
                label="软压缩目标（KiB）"
                description="超过后压缩历史图片；不是拒绝线。范围 256–32768，默认 800。"
                min={256}
                max={32768}
                value={draft.totalBase64BudgetBytes / 1024}
                onChange={(raw) => setNumber('totalBase64BudgetBytes', String(Number(raw) * 1024))}
                disabled={isPending || !draft.enabled}
              />
              <BudgetField
                id="image-hard-limit-kib"
                label="本地硬上限（KiB）"
                description="普通体和激进体都超过才拒绝。范围 256–32768，默认 8192。"
                min={256}
                max={32768}
                value={draft.hardBase64LimitBytes / 1024}
                onChange={(raw) => setNumber('hardBase64LimitBytes', String(Number(raw) * 1024))}
                disabled={isPending || !draft.enabled}
              />
              <BudgetField
                id="image-history-dimension"
                label="历史图片最大边长"
                description="普通预检使用，范围 640–4096。"
                min={640}
                max={4096}
                value={draft.historyMaxDimension}
                onChange={(raw) => setNumber('historyMaxDimension', raw)}
                disabled={isPending || !draft.enabled}
              />
              <BudgetField
                id="image-history-quality"
                label="历史图片 JPEG 质量"
                description="普通预检使用，范围 40–95。"
                min={40}
                max={95}
                value={draft.historyJpegQuality}
                onChange={(raw) => setNumber('historyJpegQuality', raw)}
                disabled={isPending || !draft.enabled}
              />
              <BudgetField
                id="image-retry-dimension"
                label="重试最大边长"
                description="超限后最多重试一次，范围 480–普通边长。"
                min={480}
                max={draft.historyMaxDimension}
                value={draft.retryHistoryMaxDimension}
                onChange={(raw) => setNumber('retryHistoryMaxDimension', raw)}
                disabled={isPending || !draft.enabled}
              />
              <BudgetField
                id="image-retry-quality"
                label="重试 JPEG 质量"
                description="范围 30–普通质量。"
                min={30}
                max={draft.historyJpegQuality}
                value={draft.retryHistoryJpegQuality}
                onChange={(raw) => setNumber('retryHistoryJpegQuality', raw)}
                disabled={isPending || !draft.enabled}
              />
            </section>

            <p className="rounded-xl border border-sky-500/20 bg-sky-500/5 p-3 text-xs leading-relaxed text-muted-foreground">
              只对历史图片做有损压缩，不会删除图片；当前轮只做格式校验或像素无损 PNG 归一化。超过软目标但未超过硬上限的请求仍会发送。
            </p>

            {validationError && <p role="alert" className="text-sm text-destructive">{validationError}</p>}

            <DialogFooter>
              <Button type="submit" size="sm" disabled={isPending}>
                <Save className="mr-1.5 h-3.5 w-3.5" />
                {isPending ? '保存中…' : '保存并立即生效'}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}

function BudgetField({
  id,
  label,
  description,
  min,
  max,
  value,
  onChange,
  disabled,
}: {
  id: string
  label: string
  description: string
  min: number
  max: number
  value: number
  onChange: (value: string) => void
  disabled: boolean
}) {
  return (
    <div className="space-y-2">
      <label htmlFor={id} className="text-sm font-medium">{label}</label>
      <Input
        id={id}
        type="number"
        min={min}
        max={max}
        step={1}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        disabled={disabled}
      />
      <p className="text-xs text-muted-foreground">{description}</p>
    </div>
  )
}
