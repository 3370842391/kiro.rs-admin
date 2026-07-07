import { useState } from 'react'
import { Shuffle, Plus, Trash2, ArrowRight } from 'lucide-react'
import { toast } from 'sonner'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogContent, DialogHeader, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import {
  useModelMappings, useUpsertModelMapping, useDeleteModelMapping,
} from '@/hooks/use-model-mappings'
import { extractErrorMessage } from '@/lib/utils'

interface ModelMappingsDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

/**
 * 模型映射管理：请求时把源模型名（如 gpt-5.5）转发到目标模型名（如 claude-opus-4.8）。
 * 源名不出现在 /v1/models 列表，仅在请求命中时转发。大小写不敏感。
 */
export function ModelMappingsDialog({ open, onOpenChange }: ModelMappingsDialogProps) {
  const { data, isLoading } = useModelMappings()
  const { mutate: upsert, isPending: saving } = useUpsertModelMapping()
  const { mutate: remove } = useDeleteModelMapping()

  const [source, setSource] = useState('')
  const [target, setTarget] = useState('')

  const mappings = data?.mappings ?? []

  const handleAdd = (e: React.FormEvent) => {
    e.preventDefault()
    const s = source.trim()
    const t = target.trim()
    if (!s || !t) {
      toast.error('源模型名和目标模型名都不能为空')
      return
    }
    upsert(
      { source: s, target: t },
      {
        onSuccess: () => {
          toast.success(`已保存映射 ${s} → ${t}`)
          setSource('')
          setTarget('')
        },
        onError: (err) => toast.error(`保存失败: ${extractErrorMessage(err)}`),
      },
    )
  }

  const handleDelete = (src: string) => {
    remove(src, {
      onSuccess: () => toast.success(`已删除映射 ${src}`),
      onError: (err) => toast.error(`删除失败: ${extractErrorMessage(err)}`),
    })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="sm:max-w-lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Shuffle className="h-4 w-4" />
            模型映射
          </DialogTitle>
          <DialogDescription>
            请求时把源模型名转发到目标模型名（如 <code>gpt-5.5</code> →{' '}
            <code>claude-opus-4.8</code>）。源名不会出现在 <code>/v1/models</code>{' '}
            列表，仅在请求命中时转发；匹配大小写不敏感。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleAdd} className="flex items-end gap-2 py-1">
          <label className="flex-1 text-xs font-medium text-muted-foreground">
            源模型名
            <Input
              placeholder="gpt-5.5"
              value={source}
              onChange={(e) => setSource(e.target.value)}
              disabled={saving}
              className="mt-1 h-8 font-mono text-[13px]"
            />
          </label>
          <ArrowRight className="mb-2 h-4 w-4 shrink-0 text-muted-foreground" />
          <label className="flex-1 text-xs font-medium text-muted-foreground">
            目标模型名
            <Input
              placeholder="claude-opus-4.8"
              value={target}
              onChange={(e) => setTarget(e.target.value)}
              disabled={saving}
              className="mt-1 h-8 font-mono text-[13px]"
            />
          </label>
          <Button type="submit" size="sm" disabled={saving} className="mb-0">
            <Plus className="h-3.5 w-3.5" />
            {saving ? '保存中…' : '添加'}
          </Button>
        </form>

        <div className="max-h-72 space-y-1.5 overflow-y-auto">
          {isLoading ? (
            <p className="py-6 text-center text-sm text-muted-foreground">加载中…</p>
          ) : mappings.length === 0 ? (
            <p className="py-6 text-center text-sm text-muted-foreground">
              暂无映射。添加后，客户端用源模型名请求即会转发到目标模型。
            </p>
          ) : (
            mappings.map((m) => (
              <div
                key={m.source}
                className="flex items-center justify-between gap-2 rounded-md border border-border/60 bg-secondary/30 px-3 py-2"
              >
                <div className="flex min-w-0 items-center gap-2 font-mono text-[13px]">
                  <span className="truncate text-foreground">{m.source}</span>
                  <ArrowRight className="h-3.5 w-3.5 shrink-0 text-muted-foreground" />
                  <span className="truncate text-muted-foreground">{m.target}</span>
                </div>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-7 w-7 shrink-0 text-muted-foreground hover:text-destructive"
                  onClick={() => handleDelete(m.source)}
                  title="删除"
                >
                  <Trash2 className="h-3.5 w-3.5" />
                </Button>
              </div>
            ))
          )}
        </div>
      </DialogContent>
    </Dialog>
  )
}
