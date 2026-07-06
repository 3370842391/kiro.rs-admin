import { useEffect, useMemo, useState } from 'react'
import { toast } from 'sonner'
import { Activity, CheckCircle2, Loader2, RefreshCw, XCircle } from 'lucide-react'
import {
  Dialog,
  DialogContent,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Badge } from '@/components/ui/badge'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import {
  Select,
  SelectContent,
  SelectItem,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { getCredentialModels, testCredentialResponse } from '@/api/credentials'
import { extractErrorMessage } from '@/lib/utils'
import type {
  AvailableModelItem,
  CredentialResponseTestResponse,
  CredentialStatusItem,
} from '@/types/api'

const DEFAULT_RESPONSE_TEST_MODEL = 'claude-sonnet-4-6'

type TestStatus = 'pending' | 'running' | 'success' | 'failed'

interface TestRow {
  id: number
  status: TestStatus
  result?: CredentialResponseTestResponse
  error?: string
}

interface CredentialResponseTestDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
  credentials: CredentialStatusItem[]
  initialIds: number[]
}

function labelForCredential(c?: CredentialStatusItem) {
  if (!c) return ''
  return c.email || `凭据 #${c.id}`
}

function statusBadge(row: TestRow) {
  if (row.status === 'running') {
    return (
      <Badge variant="secondary" className="gap-1">
        <Loader2 className="h-3 w-3 animate-spin" />
        测试中
      </Badge>
    )
  }
  if (row.status === 'success') {
    return (
      <Badge variant="outline" className="gap-1 border-green-500/50 text-green-600 dark:text-green-400">
        <CheckCircle2 className="h-3 w-3" />
        正常
      </Badge>
    )
  }
  if (row.status === 'failed') {
    return (
      <Badge variant="outline" className="gap-1 border-destructive/50 text-destructive">
        <XCircle className="h-3 w-3" />
        异常
      </Badge>
    )
  }
  return <Badge variant="outline">待测</Badge>
}

export function CredentialResponseTestDialog({
  open,
  onOpenChange,
  credentials,
  initialIds,
}: CredentialResponseTestDialogProps) {
  const [selectedIds, setSelectedIds] = useState<Set<number>>(new Set())
  const [model, setModel] = useState(DEFAULT_RESPONSE_TEST_MODEL)
  const [models, setModels] = useState<AvailableModelItem[]>([])
  const [loadingModels, setLoadingModels] = useState(false)
  const [testing, setTesting] = useState(false)
  const [rows, setRows] = useState<Map<number, TestRow>>(new Map())

  const enabledCredentials = useMemo(
    () => credentials.filter((c) => !c.disabled),
    [credentials],
  )
  const selectedCount = selectedIds.size
  const firstSelectedId = selectedIds.values().next().value as number | undefined

  useEffect(() => {
    if (!open) return
    const enabled = new Set(credentials.filter((c) => !c.disabled).map((c) => c.id))
    const next = initialIds.filter((id) => enabled.has(id))
    setSelectedIds(new Set(next.length > 0 ? next : enabledCredentials.slice(0, 1).map((c) => c.id)))
    setRows(new Map())
    setModels([])
    setModel(DEFAULT_RESPONSE_TEST_MODEL)
  }, [open, initialIds, credentials, enabledCredentials])

  const toggle = (id: number) => {
    setSelectedIds((prev) => {
      const next = new Set(prev)
      if (next.has(id)) next.delete(id)
      else next.add(id)
      return next
    })
  }

  const handleLoadModels = async () => {
    const id = firstSelectedId
    if (!id) {
      toast.error('请先选择凭据')
      return
    }
    setLoadingModels(true)
    try {
      const res = await getCredentialModels(id)
      setModels(res.models)
      if (res.models.length > 0 && !res.models.some((m) => m.modelId === model)) {
        setModel(res.models[0].modelId)
      }
      toast.success(`已获取 ${res.models.length} 个模型`)
    } catch (err) {
      toast.error('获取模型失败: ' + extractErrorMessage(err))
    } finally {
      setLoadingModels(false)
    }
  }

  const handleRun = async () => {
    const ids = Array.from(selectedIds)
    if (ids.length === 0) {
      toast.error('请先选择凭据')
      return
    }
    const targetModel = model.trim() || DEFAULT_RESPONSE_TEST_MODEL
    setTesting(true)
    setRows(new Map(ids.map((id) => [id, { id, status: 'pending' as TestStatus }])))

    let ok = 0
    let failed = 0
    for (const id of ids) {
      setRows((prev) => new Map(prev).set(id, { id, status: 'running' }))
      try {
        const result = await testCredentialResponse(id, { model: targetModel })
        if (result.success) ok += 1
        else failed += 1
        setRows((prev) =>
          new Map(prev).set(id, {
            id,
            status: result.success ? 'success' : 'failed',
            result,
            error: result.error,
          }),
        )
      } catch (err) {
        failed += 1
        setRows((prev) =>
          new Map(prev).set(id, {
            id,
            status: 'failed',
            error: extractErrorMessage(err),
          }),
        )
      }
    }
    setTesting(false)
    if (failed === 0) toast.success(`响应测试完成：${ok} 个正常`)
    else toast.warning(`响应测试完成：正常 ${ok}，异常 ${failed}`)
  }

  const resultRows = Array.from(rows.values())

  return (
    <Dialog open={open} onOpenChange={(next) => !testing && onOpenChange(next)}>
      <DialogContent className="sm:max-w-2xl max-h-[85vh] flex flex-col">
        <DialogHeader>
          <DialogTitle>测试响应</DialogTitle>
        </DialogHeader>

        <div className="flex-1 space-y-4 overflow-y-auto py-2 pr-1">
          <div className="grid gap-2 sm:grid-cols-[1fr_auto]">
            {models.length > 0 ? (
              <Select value={model} onValueChange={setModel} disabled={testing}>
                <SelectTrigger className="h-10 rounded-xl px-3.5">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  {models.map((m) => (
                    <SelectItem key={m.modelId} value={m.modelId}>
                      {m.modelName || m.modelId}
                    </SelectItem>
                  ))}
                </SelectContent>
              </Select>
            ) : (
              <Input
                value={model}
                onChange={(e) => setModel(e.target.value)}
                disabled={testing}
                className="font-mono text-sm"
              />
            )}
            <Button
              type="button"
              variant="outline"
              onClick={handleLoadModels}
              disabled={testing || loadingModels || !firstSelectedId}
            >
              <RefreshCw className={loadingModels ? 'h-4 w-4 animate-spin' : 'h-4 w-4'} />
              模型列表
            </Button>
          </div>

          <div className="rounded-md border">
            <div className="flex items-center justify-between border-b px-3 py-2 text-sm">
              <span className="font-medium">凭据</span>
              <Badge variant="secondary">已选 {selectedCount}</Badge>
            </div>
            <div className="max-h-48 overflow-y-auto p-2 space-y-1">
              {enabledCredentials.length === 0 ? (
                <div className="py-6 text-center text-sm text-muted-foreground">没有可测试的启用凭据</div>
              ) : (
                enabledCredentials.map((credential) => (
                  <label
                    key={credential.id}
                    className="flex cursor-pointer items-center gap-2 rounded-md px-2 py-1.5 text-sm hover:bg-accent/50"
                  >
                    <Checkbox
                      checked={selectedIds.has(credential.id)}
                      onCheckedChange={() => toggle(credential.id)}
                      disabled={testing}
                    />
                    <span className="shrink-0 font-medium">#{credential.id}</span>
                    <span className="min-w-0 truncate text-muted-foreground">
                      {labelForCredential(credential)}
                    </span>
                  </label>
                ))
              )}
            </div>
          </div>

          {resultRows.length > 0 && (
            <div className="rounded-md border divide-y">
              {resultRows.map((row) => {
                const credential = credentials.find((c) => c.id === row.id)
                const detail = row.result?.responseSnippet || row.error
                return (
                  <div key={row.id} className="space-y-1 px-3 py-2 text-sm">
                    <div className="flex items-center justify-between gap-2">
                      <div className="flex min-w-0 items-center gap-2">
                        <span className="shrink-0 font-medium">#{row.id}</span>
                        <span className="truncate text-muted-foreground">{labelForCredential(credential)}</span>
                      </div>
                      <div className="flex shrink-0 items-center gap-2">
                        {row.result?.httpStatus != null && (
                          <Badge variant="outline" className="tabular-nums">
                            HTTP {row.result.httpStatus}
                          </Badge>
                        )}
                        {row.result && (
                          <Badge variant="secondary" className="tabular-nums">
                            {row.result.latencyMs}ms
                          </Badge>
                        )}
                        {statusBadge(row)}
                      </div>
                    </div>
                    {detail && (
                      <div className="break-words rounded bg-secondary/50 px-2 py-1 font-mono text-[11px] text-muted-foreground">
                        {detail}
                      </div>
                    )}
                  </div>
                )
              })}
            </div>
          )}
        </div>

        <div className="flex justify-end gap-2 border-t pt-3">
          <Button type="button" variant="outline" onClick={() => onOpenChange(false)} disabled={testing}>
            关闭
          </Button>
          <Button type="button" onClick={handleRun} disabled={testing || selectedCount === 0}>
            {testing ? <Loader2 className="h-4 w-4 animate-spin" /> : <Activity className="h-4 w-4" />}
            {testing ? '测试中' : '开始测试'}
          </Button>
        </div>
      </DialogContent>
    </Dialog>
  )
}
