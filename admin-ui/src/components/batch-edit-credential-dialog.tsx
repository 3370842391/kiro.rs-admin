import { useEffect, useState, type FormEvent } from 'react'
import { toast } from 'sonner'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
  DialogFooter,
  DialogDescription,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { Input } from '@/components/ui/input'
import { GroupMultiSelect } from '@/components/group-select'
import { useBatchUpdateCredentials } from '@/hooks/use-credentials'
import { buildBatchUpdateRequest } from '@/lib/rpm-operations'
import { extractErrorMessage } from '@/lib/utils'
import type { CredentialStatusItem } from '@/types/api'

type GroupMode = 'replace' | 'add' | 'remove'

interface BatchEditCredentialDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credentials: CredentialStatusItem[]
  groupOptions: string[]
  onDone: () => void
}

const MODE_LABELS: { value: GroupMode; label: string; desc: string }[] = [
  { value: 'replace', label: '替换', desc: '用所选分组覆盖原分组；不选则清除分组。' },
  { value: 'add', label: '追加', desc: '将所选分组加入原分组。' },
  { value: 'remove', label: '移除', desc: '从原分组中移除所选分组。' },
]

export function BatchEditCredentialDialog({
  open,
  onOpenChange,
  credentials,
  groupOptions,
  onDone,
}: BatchEditCredentialDialogProps) {
  const batchUpdate = useBatchUpdateCredentials()
  const [editRpm, setEditRpm] = useState(false)
  const [rpmLimitDraft, setRpmLimitDraft] = useState('10')
  const [editGroups, setEditGroups] = useState(false)
  const [mode, setMode] = useState<GroupMode>('replace')
  const [groups, setGroups] = useState<string[]>([])
  const [editSource, setEditSource] = useState(false)
  const [sourceChannel, setSourceChannel] = useState('')
  const [running, setRunning] = useState(false)

  useEffect(() => {
    if (!open) return
    setEditRpm(false)
    setRpmLimitDraft('10')
    setEditGroups(false)
    setMode('replace')
    setGroups([])
    setEditSource(false)
    setSourceChannel('')
    setRunning(false)
  }, [open])

  const handleApply = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    if (running) return

    const request = buildBatchUpdateRequest({
      ids: credentials.map((credential) => credential.id),
      editRpm,
      rpmDraft: rpmLimitDraft,
      editGroups,
      groupMode: mode,
      groups,
      editSource,
      sourceChannel,
    })
    if (!request.ok) {
      toast.error(request.message)
      return
    }

    setRunning(true)
    try {
      const result = await batchUpdate.mutateAsync(request.value)
      toast.success(`已更新 ${result.updated} 个账号，${result.unchanged} 个未变化`)
      onOpenChange(false)
      onDone()
    } catch (error) {
      toast.error('批量更新失败: ' + extractErrorMessage(error))
    } finally {
      setRunning(false)
    }
  }

  const rpmHint =
    rpmLimitDraft.trim() === '0'
      ? '不限速'
      : `最近60秒上限：${rpmLimitDraft.trim() || '—'} 次`

  return (
    <Dialog open={open} onOpenChange={(nextOpen) => !running && onOpenChange(nextOpen)}>
      <DialogContent className="max-h-[calc(100dvh-2rem)] overflow-y-auto p-4 sm:p-6 sm:max-w-md">
        <DialogHeader>
          <DialogTitle>批量编辑（{credentials.length} 个账号）</DialogTitle>
          <DialogDescription>
            仅修改已开启的字段，未开启的字段保持不变。
          </DialogDescription>
        </DialogHeader>

        <form onSubmit={handleApply} noValidate className="space-y-4">
          <section className="space-y-3 rounded-md border border-border/60 p-3">
            <label htmlFor="batch-edit-rpm" className="flex items-center justify-between gap-3">
              <span className="text-sm font-medium">修改 RPM 上限</span>
              <Switch
                id="batch-edit-rpm"
                checked={editRpm}
                onCheckedChange={setEditRpm}
                disabled={running}
              />
            </label>
            {editRpm ? (
              <div className="space-y-2">
                <label htmlFor="batch-rpm-limit" className="block text-xs font-medium text-muted-foreground">
                  最近60秒请求上限
                </label>
                <Input
                  id="batch-rpm-limit"
                  type="number"
                  inputMode="numeric"
                  min={0}
                  max={100000}
                  step={1}
                  value={rpmLimitDraft}
                  onChange={(event) => setRpmLimitDraft(event.target.value)}
                  disabled={running}
                  className="tabular-nums"
                />
                <p className="text-xs text-muted-foreground">{rpmHint}</p>
              </div>
            ) : null}
          </section>

          <section className="space-y-3 rounded-md border border-border/60 p-3">
            <label htmlFor="batch-edit-groups" className="flex items-center justify-between gap-3">
              <span className="text-sm font-medium">修改分组</span>
              <Switch
                id="batch-edit-groups"
                checked={editGroups}
                onCheckedChange={setEditGroups}
                disabled={running}
              />
            </label>
            {editGroups ? (
              <>
                <div className="grid grid-cols-3 gap-2">
                  {MODE_LABELS.map((item) => (
                    <Button
                      key={item.value}
                      type="button"
                      size="sm"
                      variant={mode === item.value ? 'default' : 'outline'}
                      onClick={() => setMode(item.value)}
                      disabled={running}
                    >
                      {item.label}
                    </Button>
                  ))}
                </div>
                <p className="text-xs text-muted-foreground">
                  {MODE_LABELS.find((item) => item.value === mode)?.desc}
                </p>
                <GroupMultiSelect
                  value={groups}
                  options={groupOptions}
                  onChange={setGroups}
                  disabled={running}
                />
              </>
            ) : null}
          </section>

          <section className="space-y-3 rounded-md border border-border/60 p-3">
            <label htmlFor="batch-edit-source" className="flex items-center justify-between gap-3">
              <span className="text-sm font-medium">修改来源渠道</span>
              <Switch
                id="batch-edit-source"
                checked={editSource}
                onCheckedChange={setEditSource}
                disabled={running}
              />
            </label>
            {editSource ? (
              <div className="space-y-2">
                <label htmlFor="batch-source-channel" className="block text-xs font-medium text-muted-foreground">
                  来源渠道
                </label>
                <Input
                  id="batch-source-channel"
                  placeholder="留空以清除来源渠道"
                  value={sourceChannel}
                  onChange={(event) => setSourceChannel(event.target.value)}
                  disabled={running}
                />
              </div>
            ) : null}
          </section>

          <DialogFooter className="pt-1">
            <Button
              type="button"
              variant="outline"
              className="w-full sm:w-auto"
              onClick={() => onOpenChange(false)}
              disabled={running}
            >
              取消
            </Button>
            <Button type="submit" className="w-full sm:w-auto" disabled={running}>
              {running ? '应用中…' : '应用'}
            </Button>
          </DialogFooter>
        </form>
      </DialogContent>
    </Dialog>
  )
}
