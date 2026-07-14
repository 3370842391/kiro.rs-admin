import { describe, expect, test } from 'bun:test'

import {
  maskApiKey,
  parseApiKeyLines,
} from './api-key-import'

describe('parseApiKeyLines', () => {
  test('双列格式使用批次 API Region，并忽略空行、注释及两侧空白', () => {
    const result = parseApiKeyLines(
      [
        '  # synthetic fixtures only  ',
        '',
        '  alpha  |  ksk_test_alpha_12345678  ',
        'beta|ksk_test_beta_87654321',
      ].join('\n'),
      'us-east-1',
    )

    expect(result.errors).toEqual([])
    expect(result.entries).toEqual([
      {
        lineNumber: 3,
        nickname: 'alpha',
        kiroApiKey: 'ksk_test_alpha_12345678',
        maskedApiKey: 'ksk_••••5678',
        apiRegion: 'us-east-1',
      },
      {
        lineNumber: 4,
        nickname: 'beta',
        kiroApiKey: 'ksk_test_beta_87654321',
        maskedApiKey: 'ksk_••••4321',
        apiRegion: 'us-east-1',
      },
    ])
  })

  test('三列格式逐行覆盖批次 API Region', () => {
    const result = parseApiKeyLines(
      'eu-account | ksk_test_eu_12345678 | eu-central-1',
      'us-east-1',
    )

    expect(result.errors).toEqual([])
    expect(result.entries[0]?.apiRegion).toBe('eu-central-1')
  })

  test('空 nickname、空 Key、非 ksk_、非法 Region、缺少批次 Region及列数错误逐行报错', () => {
    const result = parseApiKeyLines(
      [
        ' | ksk_test_empty_name_12345678',
        'empty-key | ',
        'wrong-prefix | token_test_12345678',
        'wrong-region | ksk_test_region_12345678 | ap-southeast-1',
        'too-many | ksk_test_columns_12345678 | us-east-1 | extra',
        'too-few',
      ].join('\n'),
    )

    expect(result.entries).toEqual([])
    expect(result.errors.map((error) => error.lineNumber)).toEqual([1, 2, 3, 4, 5, 6])
    expect(result.errors.map((error) => error.message)).toEqual([
      'nickname 不能为空；请选择批次 API Region或在第三列指定',
      'API Key 不能为空；请选择批次 API Region或在第三列指定',
      'API Key 必须以 ksk_ 开头；请选择批次 API Region或在第三列指定',
      'API Region 仅支持 us-east-1 或 eu-central-1',
      '列数必须为 2 或 3 列',
      '列数必须为 2 或 3 列',
    ])
  })

  test('同一批次重复 Key 只保留首次出现的有效行', () => {
    const duplicateKey = 'ksk_test_duplicate_12345678'
    const result = parseApiKeyLines(
      [
        `first | ${duplicateKey}`,
        `second | ${duplicateKey}`,
      ].join('\n'),
      'eu-central-1',
    )

    expect(result.entries).toHaveLength(1)
    expect(result.errors).toHaveLength(1)
    expect(result.errors[0]).toMatchObject({
      lineNumber: 2,
      nickname: 'second',
      maskedApiKey: 'ksk_••••5678',
      apiRegion: 'eu-central-1',
      message: 'API Key 与第 1 行重复',
    })
  })

  test('错误对象与掩码预览不保留完整 Key 或原始整行', () => {
    const secret = 'ksk_test_never_expose_13572468'
    const rawLine = `unsafe-${secret} | ${secret} | invalid-region`
    const result = parseApiKeyLines(rawLine, 'us-east-1')
    const serializedErrors = JSON.stringify(result.errors)

    expect(result.entries).toEqual([])
    expect(serializedErrors).not.toContain(secret)
    expect(serializedErrors).not.toContain(rawLine)
    expect(result.errors[0]?.nickname).toBe('unsafe-ksk_••••2468')
    expect(result.errors[0]?.maskedLine).toBe(
      'unsafe-ksk_••••2468 | ksk_••••2468 | invalid-region',
    )
    expect(maskApiKey(secret)).toBe('ksk_••••2468')
    expect(maskApiKey('token_invalid_secret')).toBe('••••')
  })
})
