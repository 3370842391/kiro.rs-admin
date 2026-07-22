import { api } from '@/api/credentials'
import type {
  ProfitConfigUpdate,
  ProfitConfigView,
  ProfitReport,
} from '@/types/api'

export async function getProfitConfig(): Promise<ProfitConfigView> {
  const { data } = await api.get<ProfitConfigView>('/config/profit')
  return data
}

export async function updateProfitConfig(
  value: ProfitConfigUpdate,
): Promise<ProfitConfigView> {
  const newapiToken = value.newapiToken ?? ''
  const payload: ProfitConfigUpdate = {
    ...value,
    newapiToken: newapiToken.trim() ? newapiToken.trim() : undefined,
  }
  const { data } = await api.put<ProfitConfigView>('/config/profit', payload)
  return data
}

export async function runProfitReport(minutes: number): Promise<ProfitReport> {
  const { data } = await api.post<ProfitReport>(
    '/profit/report',
    { minutes },
    { timeout: 60_000 },
  )
  return data
}
