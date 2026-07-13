import { api } from '@/api/credentials'
import type { ImageBudgetConfig } from '@/types/api'

export async function getImageBudget(): Promise<ImageBudgetConfig> {
  const { data } = await api.get<ImageBudgetConfig>('/config/image-budget')
  return data
}

export async function setImageBudget(
  value: ImageBudgetConfig,
): Promise<ImageBudgetConfig> {
  const { data } = await api.put<ImageBudgetConfig>('/config/image-budget', value)
  return data
}
