import { keepPreviousData, useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  cleanupErrorSnapshots,
  deleteErrorSnapshot,
  downloadErrorSnapshot,
  getErrorSnapshot,
  getErrorSnapshotPayload,
  getErrorSnapshotStorage,
  listErrorSnapshots,
  pinErrorSnapshot,
  unpinErrorSnapshot,
} from '@/api/error-snapshots'
import type { ErrorSnapshotQuery } from '@/types/api'

export function useErrorSnapshots(query: ErrorSnapshotQuery, enabled = true) {
  return useQuery({
    queryKey: ['errorSnapshots', query],
    queryFn: () => listErrorSnapshots(query),
    enabled,
    placeholderData: keepPreviousData,
    refetchInterval: enabled ? 30_000 : false,
    staleTime: 10_000,
    refetchOnWindowFocus: false,
  })
}

export function useErrorSnapshot(id: string | null, enabled = true) {
  return useQuery({
    queryKey: ['errorSnapshots', 'detail', id],
    queryFn: () => getErrorSnapshot(id as string),
    enabled: enabled && id != null,
    staleTime: 10_000,
  })
}

export function useErrorSnapshotPayload(id: string | null, seq: number | null, enabled = true) {
  return useQuery({
    queryKey: ['errorSnapshots', 'payload', id, seq],
    queryFn: () => getErrorSnapshotPayload(id as string, seq as number),
    enabled: enabled && id != null && seq != null,
    staleTime: 60_000,
  })
}

export function useErrorSnapshotStorage(enabled = true) {
  return useQuery({
    queryKey: ['errorSnapshots', 'storage'],
    queryFn: getErrorSnapshotStorage,
    enabled,
    refetchInterval: enabled ? 30_000 : false,
    staleTime: 10_000,
  })
}

function invalidateSnapshots(queryClient: ReturnType<typeof useQueryClient>) {
  void queryClient.invalidateQueries({ queryKey: ['errorSnapshots'] })
  void queryClient.invalidateQueries({ queryKey: ['traces'] })
}

export function usePinErrorSnapshot() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => pinErrorSnapshot(id),
    onSuccess: () => invalidateSnapshots(queryClient),
  })
}

export function useUnpinErrorSnapshot() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => unpinErrorSnapshot(id),
    onSuccess: () => invalidateSnapshots(queryClient),
  })
}

export function useDeleteErrorSnapshot() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: (id: string) => deleteErrorSnapshot(id),
    onSuccess: () => invalidateSnapshots(queryClient),
  })
}

export function useDownloadErrorSnapshot() {
  return useMutation({ mutationFn: (id: string) => downloadErrorSnapshot(id) })
}

export function useCleanupErrorSnapshots() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: cleanupErrorSnapshots,
    onSuccess: () => invalidateSnapshots(queryClient),
  })
}
