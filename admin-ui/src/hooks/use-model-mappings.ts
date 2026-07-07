import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  listModelMappings,
  upsertModelMapping,
  deleteModelMapping,
  replaceModelMappings,
} from '@/api/model-mappings'
import type { ModelMapping, UpsertModelMappingRequest } from '@/types/api'

export function useModelMappings() {
  return useQuery({
    queryKey: ['model-mappings'],
    queryFn: listModelMappings,
    staleTime: 5000,
  })
}

export function useUpsertModelMapping() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (req: UpsertModelMappingRequest) => upsertModelMapping(req),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['model-mappings'] }),
  })
}

export function useDeleteModelMapping() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (source: string) => deleteModelMapping(source),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['model-mappings'] }),
  })
}

export function useReplaceModelMappings() {
  const qc = useQueryClient()
  return useMutation({
    mutationFn: (mappings: ModelMapping[]) => replaceModelMappings(mappings),
    onSuccess: () => qc.invalidateQueries({ queryKey: ['model-mappings'] }),
  })
}
