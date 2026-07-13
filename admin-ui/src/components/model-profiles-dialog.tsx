import { useEffect, useMemo, useState, type FormEvent } from 'react'
import {
  AlertTriangle,
  Brain,
  CloudDownload,
  History,
  Info,
  ListChecks,
  Lock,
  Pencil,
  Plus,
  RefreshCw,
  Save,
  ShieldCheck,
  Trash2,
  Unlock,
} from 'lucide-react'
import { toast } from 'sonner'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { useConfirm } from '@/components/ui/confirm-dialog'
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from '@/components/ui/dialog'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  useApplyModelProfilePreview,
  useDeleteModelProfile,
  useFetchModelProfile,
  useModelProfiles,
  usePatchModelProfile,
  usePreviewModelProfiles,
  useSetModelProfileSettings,
  useSyncModelProfiles,
} from '@/hooks/use-model-profiles'
import {
  buildApplyRequest,
  buildFetchProfileRequest,
  buildPreviewProfileRequest,
  buildProfilePatch,
  hasProfileDraftValue,
  MAX_MODEL_PROFILE_TOKENS,
  ModelProfileRequestError,
  validateProfileDraft,
  type ModelProfileDraft,
} from '@/lib/model-profiles'
import { extractErrorMessage } from '@/lib/utils'
import type {
  ModelProfileField,
  ModelProfileFieldName,
  ModelProfilePreviewResponse,
  ModelProfileSyncSummary,
  ModelProfileView,
} from '@/types/api'

interface ModelProfilesDialogProps {
  open: boolean
  onOpenChange: (open: boolean) => void
}

interface EditorState {
  modelId: string
  isNew: boolean
  draft: ModelProfileDraft
  profile?: ModelProfileView
}

const PROFILE_FIELDS: Array<{
  name: ModelProfileFieldName
  label: string
  placeholder: string
  numeric: boolean
}> = [
  {
    name: 'contextWindowTokens',
    label: '上下文窗口',
    placeholder: '例如 1000000',
    numeric: true,
  },
  {
    name: 'maxOutputTokens',
    label: '最大输出',
    placeholder: '例如 128000',
    numeric: true,
  },
  {
    name: 'knowledgeCutoff',
    label: '知识截止日期',
    placeholder: 'YYYY-MM 或 YYYY-MM-DD',
    numeric: false,
  },
  {
    name: 'releaseDate',
    label: '发布日期',
    placeholder: 'YYYY-MM 或 YYYY-MM-DD',
    numeric: false,
  },
]

export function ModelProfilesDialog({ open, onOpenChange }: ModelProfilesDialogProps) {
  const confirm = useConfirm()
  const { data, isLoading, error, refetch } = useModelProfiles(open)
  const patchProfile = usePatchModelProfile()
  const deleteProfile = useDeleteModelProfile()
  const fetchProfile = useFetchModelProfile()
  const syncProfiles = useSyncModelProfiles()
  const previewProfiles = usePreviewModelProfiles()
  const applyPreview = useApplyModelProfilePreview()
  const setSettings = useSetModelProfileSettings()

  const [editor, setEditor] = useState<EditorState | null>(null)
  const [preview, setPreview] = useState<ModelProfilePreviewResponse | null>(null)
  const [selectedChanges, setSelectedChanges] = useState<string[]>([])
  const [lastSummary, setLastSummary] = useState<ModelProfileSyncSummary | null>(null)
  const [showSummary, setShowSummary] = useState(false)

  const summary = lastSummary ?? data?.lastSync ?? null
  const busy =
    patchProfile.isPending ||
    deleteProfile.isPending ||
    fetchProfile.isPending ||
    syncProfiles.isPending ||
    previewProfiles.isPending ||
    applyPreview.isPending ||
    setSettings.isPending

  const profiles = useMemo(
    () => [...(data?.profiles ?? [])].sort((left, right) => left.modelId.localeCompare(right.modelId)),
    [data?.profiles],
  )

  const handleMutationError = (operation: string, mutationError: unknown) => {
    toast.error(`${operation}失败: ${extractErrorMessage(mutationError)}`)
    if (mutationError instanceof ModelProfileRequestError && mutationError.status === 409) {
      void refetch()
    }
  }

  const handleSync = () => {
    if (!data) return
    syncProfiles.mutate(
      { baseRevision: data.revision, forcePublic: false },
      {
        onSuccess: ({ summary: nextSummary }) => {
          setLastSummary(nextSummary)
          setShowSummary(true)
          toast.success(`同步完成：补充 ${nextSummary.applied.length} 个字段`)
        },
        onError: (mutationError) => handleMutationError('同步', mutationError),
      },
    )
  }

  const handleFetch = (modelId: string) => {
    if (!data) return
    fetchProfile.mutate(
      { modelId, request: buildFetchProfileRequest(data.revision) },
      {
        onSuccess: ({ summary: nextSummary }) => {
          setLastSummary(nextSummary)
          setShowSummary(true)
          toast.success(`已获取 ${modelId}：补充 ${nextSummary.applied.length} 个字段`)
        },
        onError: (mutationError) => handleMutationError('获取', mutationError),
      },
    )
  }

  const handlePreview = () => {
    previewProfiles.mutate(
      buildPreviewProfileRequest(),
      {
        onSuccess: (nextPreview) => {
          setPreview(nextPreview)
          setSelectedChanges(
            nextPreview.changes.filter((change) => !change.locked).map((change) => change.id),
          )
          if (nextPreview.changes.length === 0) toast.info('当前没有可预览的资料差异')
        },
        onError: (mutationError) => handleMutationError('获取差异', mutationError),
      },
    )
  }

  const handleApplyPreview = () => {
    if (!preview) return
    const request = buildApplyRequest(preview, selectedChanges)
    if (request.changes.length === 0) {
      toast.error('请至少选择一个未锁定字段')
      return
    }
    applyPreview.mutate(request, {
      onSuccess: () => {
        toast.success(`已应用 ${request.changes.length} 个资料字段`)
        setPreview(null)
        setSelectedChanges([])
      },
      onError: (mutationError) => {
        handleMutationError('应用预览', mutationError)
        if (
          mutationError instanceof ModelProfileRequestError &&
          (mutationError.previewExpired || mutationError.status === 409)
        ) {
          setPreview(null)
          setSelectedChanges([])
        }
      },
    })
  }

  const handleDelete = async (profile: ModelProfileView) => {
    if (!data) return
    const accepted = await confirm({
      title: '删除模型资料',
      description: (
        <>
          将删除 <code>{profile.modelId}</code> 的全部持久化资料；模型不会被隐藏，模型映射也不会删除，运行时会回退到 resolved/builtin 资料。
        </>
      ),
      confirmText: '删除资料',
      destructive: true,
    })
    if (!accepted) return
    deleteProfile.mutate(
      { modelId: profile.modelId, baseRevision: data.revision },
      {
        onSuccess: () => toast.success(`已删除 ${profile.modelId} 的持久化资料`),
        onError: (mutationError) => handleMutationError('删除', mutationError),
      },
    )
  }

  const handleSettings = (enabled: boolean) => {
    setSettings.mutate(enabled, {
      onSuccess: () => toast.success(enabled ? '已启用模型资料认证回复' : '已关闭本地认证回复'),
      onError: (mutationError) => handleMutationError('保存设置', mutationError),
    })
  }

  return (
    <>
      <Dialog open={open} onOpenChange={(next) => !busy && onOpenChange(next)}>
        <DialogContent className="max-h-[92vh] overflow-y-auto sm:max-w-6xl">
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <Brain className="h-4 w-4" />
              模型能力与身份
            </DialogTitle>
            <DialogDescription>
              按规范模型 ID 维护上下文窗口、输出上限和公开资料；普通获取与同步只补空值。
            </DialogDescription>
          </DialogHeader>

          {isLoading ? (
            <p className="py-12 text-center text-sm text-muted-foreground">加载模型资料…</p>
          ) : error || !data ? (
            <div
              role="alert"
              className="rounded-xl border border-destructive/30 bg-destructive/5 p-4 text-sm text-destructive"
            >
              加载失败：{extractErrorMessage(error)}
            </div>
          ) : (
            <div className="space-y-4">
              <section className="flex flex-col gap-3 rounded-2xl border border-border/70 bg-muted/20 p-4 lg:flex-row lg:items-center lg:justify-between">
                <div className="flex min-w-0 items-start gap-3">
                  <ShieldCheck className="mt-0.5 h-5 w-5 shrink-0 text-emerald-500" />
                  <div>
                    <label htmlFor="model-profile-exact-answers" className="text-sm font-medium">
                      启用模型资料认证回复
                    </label>
                    <p className="mt-0.5 text-xs leading-relaxed text-muted-foreground">
                      仅对严格匹配的上下文窗口和知识截止日期探针本地回答。关闭后仍可编辑、获取和同步，探针将继续走上游。
                    </p>
                  </div>
                </div>
                <Switch
                  id="model-profile-exact-answers"
                  checked={data.exactAnswersEnabled}
                  onCheckedChange={handleSettings}
                  disabled={setSettings.isPending}
                />
              </section>

              <div className="flex flex-wrap items-center gap-2">
                <Button size="sm" onClick={handleSync} disabled={busy}>
                  <RefreshCw className={syncProfiles.isPending ? 'animate-spin motion-reduce:animate-none' : ''} />
                  {syncProfiles.isPending ? '同步中…' : '同步全部'}
                </Button>
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => setEditor(newEditorState())}
                  disabled={busy}
                >
                  <Plus />新增手填模型
                </Button>
                <Button size="sm" variant="outline" onClick={handlePreview} disabled={busy}>
                  <ListChecks />
                  {previewProfiles.isPending ? '获取差异中…' : '预览强制覆盖'}
                </Button>
                <Button
                  size="sm"
                  variant="ghost"
                  onClick={() => setShowSummary((value) => !value)}
                  disabled={!summary}
                >
                  <History />{showSummary ? '收起同步结果' : '查看同步结果'}
                </Button>
                <span className="ml-auto text-xs text-muted-foreground">
                  revision {data.revision} · {profiles.length} 个模型
                </span>
              </div>

              {showSummary && summary && <SyncSummaryPanel summary={summary} />}

              <div className="overflow-x-auto rounded-2xl border border-border/70">
                <table className="w-full min-w-[1040px] border-collapse text-left text-sm">
                  <thead className="bg-muted/50 text-xs text-muted-foreground">
                    <tr>
                      <th className="px-3 py-2.5 font-medium">模型 ID</th>
                      {PROFILE_FIELDS.map((field) => (
                        <th key={field.name} className="px-3 py-2.5 font-medium">
                          {field.label}
                        </th>
                      ))}
                      <th className="px-3 py-2.5 font-medium">最近更新</th>
                      <th className="px-3 py-2.5 text-right font-medium">操作</th>
                    </tr>
                  </thead>
                  <tbody>
                    {profiles.length === 0 ? (
                      <tr>
                        <td colSpan={7} className="px-4 py-12 text-center text-muted-foreground">
                          尚无持久化模型资料。可新增手填模型，或先执行“同步全部”。
                        </td>
                      </tr>
                    ) : (
                      profiles.map((profile) => (
                        <tr
                          key={profile.modelId}
                          className="border-t border-border/60 transition-colors hover:bg-muted/20"
                        >
                          <td className="max-w-56 px-3 py-3 align-top font-mono text-xs font-medium">
                            <span className="break-all">{profile.modelId}</span>
                          </td>
                          {PROFILE_FIELDS.map((field) => (
                            <td key={field.name} className="px-3 py-3 align-top">
                              <ProfileValue profile={profile} field={field.name} />
                            </td>
                          ))}
                          <td className="px-3 py-3 align-top text-xs text-muted-foreground">
                            {formatTimestamp(latestUpdatedAt(profile))}
                          </td>
                          <td className="px-3 py-3 align-top">
                            <div className="flex justify-end gap-1">
                              <Button
                                type="button"
                                size="icon"
                                variant="ghost"
                                className="h-8 w-8"
                                onClick={() => handleFetch(profile.modelId)}
                                disabled={busy}
                                title="获取当前模型（只补空值）"
                                aria-label={`获取 ${profile.modelId}`}
                              >
                                <CloudDownload />
                              </Button>
                              <Button
                                type="button"
                                size="icon"
                                variant="ghost"
                                className="h-8 w-8"
                                onClick={() => setEditor(editorStateFromProfile(profile))}
                                disabled={busy}
                                title="编辑资料"
                                aria-label={`编辑 ${profile.modelId}`}
                              >
                                <Pencil />
                              </Button>
                              <Button
                                type="button"
                                size="icon"
                                variant="ghost"
                                className="h-8 w-8 text-muted-foreground hover:text-destructive"
                                onClick={() => void handleDelete(profile)}
                                disabled={busy}
                                title="删除持久化资料"
                                aria-label={`删除 ${profile.modelId} 的资料`}
                              >
                                <Trash2 />
                              </Button>
                            </div>
                          </td>
                        </tr>
                      ))
                    )}
                  </tbody>
                </table>
              </div>

              <div className="flex gap-2 rounded-xl border border-sky-500/20 bg-sky-500/5 p-3 text-xs leading-relaxed text-muted-foreground">
                <Info className="mt-0.5 h-4 w-4 shrink-0 text-sky-500" />
                <p>
                  手填非空字段默认锁定；普通获取和同步不会覆盖任何已有值。强制覆盖必须先预览并逐字段勾选，revision 变化后需刷新重试。
                </p>
              </div>
            </div>
          )}
        </DialogContent>
      </Dialog>

      <ProfileEditorDialog
        state={editor}
        revision={data?.revision ?? 0}
        saving={patchProfile.isPending}
        onClose={() => setEditor(null)}
        onChange={setEditor}
        onSave={(state) => {
          if (!data) return
          const modelId = state.modelId.trim()
          if (!modelId) {
            toast.error('模型 ID 不能为空')
            return
          }
          if (state.isNew && !hasProfileDraftValue(state.draft)) {
            toast.error('新增模型至少需要填写一个资料字段')
            return
          }
          const validationError = validateProfileDraft(state.draft)
          if (validationError) {
            toast.error(validationError)
            return
          }
          patchProfile.mutate(
            { modelId, request: buildProfilePatch(data.revision, state.draft) },
            {
              onSuccess: () => {
                toast.success(`已保存 ${modelId} 的资料`)
                setEditor(null)
              },
              onError: (mutationError) => handleMutationError('保存', mutationError),
            },
          )
        }}
      />

      <PreviewDialog
        preview={preview}
        selected={selectedChanges}
        applying={applyPreview.isPending}
        onSelectedChange={setSelectedChanges}
        onClose={() => {
          if (!applyPreview.isPending) {
            setPreview(null)
            setSelectedChanges([])
          }
        }}
        onApply={handleApplyPreview}
      />
    </>
  )
}

function ProfileEditorDialog({
  state,
  revision,
  saving,
  onClose,
  onChange,
  onSave,
}: {
  state: EditorState | null
  revision: number
  saving: boolean
  onClose: () => void
  onChange: (state: EditorState | null) => void
  onSave: (state: EditorState) => void
}) {
  const [validationError, setValidationError] = useState<string | null>(null)

  useEffect(() => {
    setValidationError(null)
  }, [state?.modelId])

  const updateDraft = (field: ModelProfileFieldName, value: string) => {
    if (!state) return
    setValidationError(null)
    onChange({ ...state, draft: { ...state.draft, [field]: value } })
  }

  const updateLock = (field: ModelProfileFieldName, locked: boolean) => {
    if (!state) return
    onChange({
      ...state,
      draft: { ...state.draft, locks: { ...state.draft.locks, [field]: locked } },
    })
  }

  const handleSubmit = (event: FormEvent) => {
    event.preventDefault()
    if (!state) return
    const message = validateProfileDraft(state.draft)
    setValidationError(message)
    if (!message) onSave(state)
  }

  return (
    <Dialog open={state !== null} onOpenChange={(next) => !next && !saving && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle>{state?.isNew ? '新增手填模型资料' : '编辑模型资料'}</DialogTitle>
          <DialogDescription>
            保存基于 revision {revision}。留空表示清除该字段的持久化 override，并回退到运行时 resolved 值。
          </DialogDescription>
        </DialogHeader>
        {state && (
          <form onSubmit={handleSubmit} className="space-y-4">
            <label htmlFor="model-profile-id" className="block space-y-1.5 text-sm font-medium">
              模型 ID
              <Input
                id="model-profile-id"
                value={state.modelId}
                onChange={(event) => onChange({ ...state, modelId: event.target.value })}
                disabled={saving || !state.isNew}
                placeholder="claude-opus-4-8"
                className="font-mono"
                autoFocus={state.isNew}
              />
            </label>

            <div className="grid gap-3 sm:grid-cols-2">
              {PROFILE_FIELDS.map((field) => {
                const persisted = state.profile ? profileField(state.profile, field.name) : null
                const resolved = state.profile ? resolvedField(state.profile, field.name) : null
                return (
                  <section key={field.name} className="space-y-3 rounded-xl border border-border/70 p-3">
                    <div className="flex items-center justify-between gap-2">
                      <label htmlFor={`model-profile-${field.name}`} className="text-sm font-medium">
                        {field.label}
                      </label>
                      <Button
                        type="button"
                        size="sm"
                        variant="ghost"
                        className="h-7 px-2 text-xs"
                        onClick={() => updateDraft(field.name, '')}
                        disabled={saving || !state.draft[field.name]}
                      >
                        清空字段
                      </Button>
                    </div>
                    <Input
                      id={`model-profile-${field.name}`}
                      type={field.numeric ? 'number' : 'text'}
                      min={field.numeric ? 1 : undefined}
                      max={field.numeric ? MAX_MODEL_PROFILE_TOKENS : undefined}
                      step={field.numeric ? 1 : undefined}
                      value={state.draft[field.name]}
                      onChange={(event) => updateDraft(field.name, event.target.value)}
                      placeholder={field.placeholder}
                      disabled={saving}
                    />
                    <label className="flex cursor-pointer items-center gap-2 text-xs text-muted-foreground">
                      <Checkbox
                        checked={state.draft.locks[field.name]}
                        onCheckedChange={(checked) => updateLock(field.name, checked === true)}
                        disabled={saving || !state.draft[field.name]}
                      />
                      保存后锁定，阻止强制同步覆盖
                    </label>
                    <div className="grid grid-cols-2 gap-2 text-[11px] text-muted-foreground">
                      <FieldSnapshot label="持久化" field={persisted} />
                      <FieldSnapshot label="运行时解析" field={resolved} />
                    </div>
                  </section>
                )
              })}
            </div>

            <div className="flex gap-2 rounded-xl border border-amber-500/20 bg-amber-500/5 p-3 text-xs leading-relaxed text-muted-foreground">
              <AlertTriangle className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
              清空字段不会隐藏模型。删除整条资料也不会影响模型映射；后续同步可重新补充空值。
            </div>

            {validationError && (
              <p role="alert" className="text-sm text-destructive">
                {validationError}
              </p>
            )}

            <DialogFooter>
              <Button type="button" variant="outline" onClick={onClose} disabled={saving}>
                取消
              </Button>
              <Button type="submit" disabled={saving || !state.modelId.trim()}>
                <Save />{saving ? '保存中…' : '保存资料'}
              </Button>
            </DialogFooter>
          </form>
        )}
      </DialogContent>
    </Dialog>
  )
}

function PreviewDialog({
  preview,
  selected,
  applying,
  onSelectedChange,
  onClose,
  onApply,
}: {
  preview: ModelProfilePreviewResponse | null
  selected: string[]
  applying: boolean
  onSelectedChange: (selected: string[]) => void
  onClose: () => void
  onApply: () => void
}) {
  const selectedSet = new Set(selected)
  return (
    <Dialog open={preview !== null} onOpenChange={(next) => !next && onClose()}>
      <DialogContent className="max-h-[90vh] overflow-y-auto sm:max-w-4xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <ListChecks className="h-4 w-4" />强制覆盖预览
          </DialogTitle>
          <DialogDescription>
            预览 5 分钟有效。只提交明确勾选的未锁定字段，候选值和来源将原样带回后端校验。
          </DialogDescription>
        </DialogHeader>
        {preview && (
          <div className="space-y-4">
            <div className="overflow-x-auto rounded-xl border border-border/70">
              <table className="w-full min-w-[720px] text-left text-sm">
                <thead className="bg-muted/50 text-xs text-muted-foreground">
                  <tr>
                    <th className="w-12 px-3 py-2 font-medium">选择</th>
                    <th className="px-3 py-2 font-medium">模型 / 字段</th>
                    <th className="px-3 py-2 font-medium">当前值</th>
                    <th className="px-3 py-2 font-medium">候选值</th>
                    <th className="px-3 py-2 font-medium">状态</th>
                  </tr>
                </thead>
                <tbody>
                  {preview.changes.length === 0 ? (
                    <tr>
                      <td colSpan={5} className="px-4 py-10 text-center text-muted-foreground">
                        没有可应用的差异。
                      </td>
                    </tr>
                  ) : (
                    preview.changes.map((change) => (
                      <tr key={change.id} className="border-t border-border/60">
                        <td className="px-3 py-3 align-top">
                          <Checkbox
                            aria-label={`选择 ${change.modelId} ${fieldLabel(change.field)}`}
                            checked={selectedSet.has(change.id)}
                            disabled={change.locked || applying}
                            onCheckedChange={(checked) => {
                              onSelectedChange(
                                checked === true
                                  ? [...selectedSet, change.id]
                                  : selected.filter((id) => id !== change.id),
                              )
                            }}
                          />
                        </td>
                        <td className="px-3 py-3 align-top">
                          <div className="font-mono text-xs">{change.modelId}</div>
                          <div className="mt-1 text-xs text-muted-foreground">
                            {fieldLabel(change.field)}
                          </div>
                        </td>
                        <td className="px-3 py-3 align-top">
                          <div className="font-mono text-xs">{displayValue(change.currentValue)}</div>
                          <SourceBadge source={change.currentSource ?? '未持久化'} />
                        </td>
                        <td className="px-3 py-3 align-top">
                          <div className="font-mono text-xs font-medium">{displayValue(change.value)}</div>
                          <SourceBadge source={change.source} />
                        </td>
                        <td className="px-3 py-3 align-top">
                          {change.locked ? (
                            <Badge variant="warning" className="[&_svg]:h-3 [&_svg]:w-3"><Lock />已锁定</Badge>
                          ) : (
                            <Badge variant="success" className="[&_svg]:h-3 [&_svg]:w-3"><Unlock />可覆盖</Badge>
                          )}
                        </td>
                      </tr>
                    ))
                  )}
                </tbody>
              </table>
            </div>
            {preview.warnings.length > 0 && (
              <div role="alert" className="rounded-xl border border-amber-500/20 bg-amber-500/5 p-3">
                <p className="text-xs font-medium text-amber-700 dark:text-amber-300">来源警告</p>
                <ul className="mt-1 list-disc space-y-1 pl-4 text-xs text-muted-foreground">
                  {preview.warnings.map((warning) => <li key={warning}>{warning}</li>)}
                </ul>
              </div>
            )}
            <p className="text-xs text-muted-foreground">
              基准 revision {preview.baseRevision} · 到期时间 {formatTimestamp(preview.expiresAt)}
            </p>
            <DialogFooter>
              <Button type="button" variant="outline" onClick={onClose} disabled={applying}>
                取消
              </Button>
              <Button type="button" onClick={onApply} disabled={applying || selected.length === 0}>
                {applying ? '应用中…' : `应用 ${selected.length} 个字段`}
              </Button>
            </DialogFooter>
          </div>
        )}
      </DialogContent>
    </Dialog>
  )
}

function SyncSummaryPanel({ summary }: { summary: ModelProfileSyncSummary }) {
  return (
    <section className="grid gap-3 rounded-2xl border border-border/70 bg-muted/20 p-4 md:grid-cols-3">
      <SummaryList title={`已补充 ${summary.applied.length}`} items={summary.applied.map(formatFieldRef)} />
      <SummaryList title={`已跳过 ${summary.skipped.length}`} items={summary.skipped.map(formatFieldRef)} />
      <SummaryList title={`警告 ${summary.warnings.length}`} items={summary.warnings} warning />
      {summary.sources.length > 0 && (
        <div className="md:col-span-3 flex flex-wrap gap-2 border-t border-border/60 pt-3">
          {summary.sources.map((source) => (
            <Badge key={`${source.source}-${source.message ?? ''}`} variant={source.ok === false ? 'warning' : 'outline'}>
              {source.source}{source.message ? `：${source.message}` : ''}
            </Badge>
          ))}
        </div>
      )}
    </section>
  )
}

function SummaryList({ title, items, warning = false }: { title: string; items: string[]; warning?: boolean }) {
  return (
    <div>
      <p className={warning ? 'text-xs font-medium text-amber-600 dark:text-amber-400' : 'text-xs font-medium'}>
        {title}
      </p>
      {items.length === 0 ? (
        <p className="mt-1 text-xs text-muted-foreground">无</p>
      ) : (
        <ul className="mt-1 max-h-28 list-disc space-y-1 overflow-y-auto pl-4 text-xs text-muted-foreground">
          {items.map((item, index) => <li key={`${item}-${index}`}>{item}</li>)}
        </ul>
      )}
    </div>
  )
}

function ProfileValue({ profile, field }: { profile: ModelProfileView; field: ModelProfileFieldName }) {
  const persisted = profileField(profile, field)
  const resolved = resolvedField(profile, field)
  const display = persisted ?? resolved
  if (!display) return <span className="text-xs text-muted-foreground">未配置</span>
  return (
    <div className="space-y-1.5">
      <div className="font-mono text-xs font-medium">{displayValue(display.value)}</div>
      <div className="flex max-w-40 flex-wrap gap-1">
        <SourceBadge source={display.source} />
        {persisted?.locked && <Badge variant="warning" className="[&_svg]:h-3 [&_svg]:w-3"><Lock />锁定</Badge>}
        {!persisted && <Badge variant="outline">resolved</Badge>}
      </div>
    </div>
  )
}

function SourceBadge({ source }: { source: string }) {
  return <Badge variant={source === 'manual' ? 'default' : 'secondary'}>{source}</Badge>
}

function FieldSnapshot({
  label,
  field,
}: {
  label: string
  field: ModelProfileField<string | number> | null
}) {
  return (
    <div className="rounded-lg bg-muted/35 p-2">
      <span>{label}</span>
      <strong className="mt-0.5 block truncate font-mono font-medium text-foreground">
        {field ? displayValue(field.value) : '无'}
      </strong>
      <span className="mt-0.5 block truncate">{field?.source ?? '—'}</span>
    </div>
  )
}

function newEditorState(): EditorState {
  return {
    modelId: '',
    isNew: true,
    draft: {
      contextWindowTokens: '',
      maxOutputTokens: '',
      knowledgeCutoff: '',
      releaseDate: '',
      locks: {
        contextWindowTokens: true,
        maxOutputTokens: true,
        knowledgeCutoff: true,
        releaseDate: true,
      },
    },
  }
}

function editorStateFromProfile(profile: ModelProfileView): EditorState {
  return {
    modelId: profile.modelId,
    isNew: false,
    profile,
    draft: {
      contextWindowTokens: valueAsString(profile.contextWindowTokens),
      maxOutputTokens: valueAsString(profile.maxOutputTokens),
      knowledgeCutoff: valueAsString(profile.knowledgeCutoff),
      releaseDate: valueAsString(profile.releaseDate),
      locks: {
        contextWindowTokens: profile.contextWindowTokens?.locked ?? true,
        maxOutputTokens: profile.maxOutputTokens?.locked ?? true,
        knowledgeCutoff: profile.knowledgeCutoff?.locked ?? true,
        releaseDate: profile.releaseDate?.locked ?? true,
      },
    },
  }
}

function profileField(
  profile: ModelProfileView,
  field: ModelProfileFieldName,
): ModelProfileField<string | number> | null {
  return profile[field] ?? null
}

function resolvedField(
  profile: ModelProfileView,
  field: ModelProfileFieldName,
): ModelProfileField<string | number> | null {
  return profile.resolved[field] ?? null
}

function valueAsString(field?: ModelProfileField<string | number> | null): string {
  return field ? String(field.value) : ''
}

function latestUpdatedAt(profile: ModelProfileView): string | undefined {
  const timestamps = PROFILE_FIELDS.flatMap((field) => {
    const persisted = profileField(profile, field.name)
    const resolved = resolvedField(profile, field.name)
    return [persisted?.updatedAt, resolved?.updatedAt].filter((value): value is string => Boolean(value))
  })
  timestamps.sort()
  return timestamps[timestamps.length - 1]
}

function fieldLabel(field: ModelProfileFieldName): string {
  return PROFILE_FIELDS.find((item) => item.name === field)?.label ?? field
}

function formatFieldRef(ref: { modelId: string; field: ModelProfileFieldName; reason?: string | null }): string {
  return `${ref.modelId} / ${fieldLabel(ref.field)}${ref.reason ? `：${ref.reason}` : ''}`
}

function displayValue(value: string | number | null | undefined): string {
  if (value == null || value === '') return '—'
  return typeof value === 'number' ? value.toLocaleString('en-US') : value
}

function formatTimestamp(value?: string | null): string {
  if (!value) return '—'
  const timestamp = new Date(value)
  return Number.isNaN(timestamp.getTime()) ? value : timestamp.toLocaleString()
}
