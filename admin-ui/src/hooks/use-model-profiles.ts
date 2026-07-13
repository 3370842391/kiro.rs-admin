import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  applyModelProfilePreview,
  deleteModelProfile,
  fetchModelProfile,
  listModelProfiles,
  patchModelProfile,
  previewModelProfiles,
  setModelProfileSettings,
  syncModelProfiles,
} from '@/api/model-profiles'
import {
  ModelProfileRequestError,
  modelProfileError,
} from '@/lib/model-profiles'
import type {
  ApplyModelProfilesRequest,
  FetchModelProfileRequest,
  PatchModelProfileRequest,
  PreviewModelProfilesRequest,
  SyncModelProfilesRequest,
} from '@/types/api'

const MODEL_PROFILES_QUERY_KEY = ['model-profiles'] as const

async function withModelProfileError<T>(operation: () => Promise<T>): Promise<T> {
  try {
    return await operation()
  } catch (error) {
    throw new ModelProfileRequestError(modelProfileError(error))
  }
}

export function useModelProfiles(enabled = true) {
  return useQuery({
    queryKey: MODEL_PROFILES_QUERY_KEY,
    queryFn: listModelProfiles,
    enabled,
    staleTime: 5000,
  })
}

export function usePatchModelProfile() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ modelId, request }: { modelId: string; request: PatchModelProfileRequest }) =>
      withModelProfileError(() => patchModelProfile(modelId, request)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}

export function useDeleteModelProfile() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ modelId, baseRevision }: { modelId: string; baseRevision: number }) =>
      withModelProfileError(() => deleteModelProfile(modelId, { baseRevision })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}

export function useFetchModelProfile() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: ({ modelId, request }: { modelId: string; request: FetchModelProfileRequest }) =>
      withModelProfileError(() => fetchModelProfile(modelId, request)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}

export function useSyncModelProfiles() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (request: SyncModelProfilesRequest) =>
      withModelProfileError(() => syncModelProfiles(request)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}

export function usePreviewModelProfiles() {
  return useMutation({
    mutationFn: (request: PreviewModelProfilesRequest) =>
      withModelProfileError(() => previewModelProfiles(request)),
  })
}

export function useApplyModelProfilePreview() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (request: ApplyModelProfilesRequest) =>
      withModelProfileError(() => applyModelProfilePreview(request)),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}

export function useSetModelProfileSettings() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (enabled: boolean) =>
      withModelProfileError(() => setModelProfileSettings({ enabled })),
    onSuccess: () => queryClient.invalidateQueries({ queryKey: MODEL_PROFILES_QUERY_KEY }),
  })
}
