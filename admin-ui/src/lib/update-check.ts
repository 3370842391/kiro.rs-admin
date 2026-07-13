import { checkSystemUpdate } from '@/api/credentials'
import type { UpdateCheckInfo } from '@/types/api'

export type SystemUpdateFetcher = (force: boolean) => Promise<UpdateCheckInfo>

export function forceCheckSystemUpdate(
  fetcher: SystemUpdateFetcher = checkSystemUpdate,
): Promise<UpdateCheckInfo> {
  return fetcher(true)
}
