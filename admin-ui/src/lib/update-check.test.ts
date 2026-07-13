import { describe, expect, test } from 'bun:test'
import type { UpdateCheckInfo } from '@/types/api'
import { forceCheckSystemUpdate } from './update-check'

describe('forceCheckSystemUpdate', () => {
  test('forces the fetcher and returns its update info unchanged', async () => {
    const updateInfo: UpdateCheckInfo = {
      currentVersion: '0.8.6',
      latestVersion: '0.8.7',
      hasUpdate: true,
      buildType: 'binary',
      checkedAt: '2026-07-13T00:16:04+08:00',
      cached: false,
    }
    const fetcher = async (force: boolean) => {
      expect(force).toBe(true)
      return updateInfo
    }

    const result = await forceCheckSystemUpdate(fetcher)

    expect(result).toBe(updateInfo)
  })
})
