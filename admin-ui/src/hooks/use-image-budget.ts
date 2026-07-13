import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { getImageBudget, setImageBudget } from '@/api/image-budget'

export function useImageBudget() {
  return useQuery({
    queryKey: ['image-budget'],
    queryFn: getImageBudget,
  })
}

export function useSetImageBudget() {
  const queryClient = useQueryClient()
  return useMutation({
    mutationFn: setImageBudget,
    onSuccess: (value) => {
      queryClient.setQueryData(['image-budget'], value)
      queryClient.invalidateQueries({ queryKey: ['image-budget'] })
    },
  })
}
