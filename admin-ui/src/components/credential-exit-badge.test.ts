import { describe, expect, test } from 'bun:test'
import { countExitUsage, proxyExitHost } from './credential-exit-badge'

describe('proxyExitHost', () => {
  test('从各种写法里取出 host:port', () => {
    expect(proxyExitHost('socks5://user:pass@203.0.113.10:1080')).toBe('203.0.113.10:1080')
    expect(proxyExitHost('http://1.2.3.4:8080')).toBe('1.2.3.4:8080')
    expect(proxyExitHost('1.2.3.4:8080')).toBe('1.2.3.4:8080')
  })

  test('direct 归一成 direct，大小写不敏感', () => {
    expect(proxyExitHost('direct')).toBe('direct')
    expect(proxyExitHost('DIRECT')).toBe('direct')
  })

  test('空值返回 null，不能当成一个叫空字符串的出口', () => {
    // 否则所有没配代理的号会被算成「共用同一个出口」
    expect(proxyExitHost(null)).toBeNull()
    expect(proxyExitHost(undefined)).toBeNull()
    expect(proxyExitHost('   ')).toBeNull()
  })

  test('多候选只取第一个：号池是一号一出口，多候选是例外', () => {
    expect(proxyExitHost('socks5://a:b@1.1.1.1:1080, socks5://c:d@2.2.2.2:1080')).toBe(
      '1.1.1.1:1080',
    )
  })

  test('认证信息里带 @ 时不能把 host 切错', () => {
    // 取最后一个 @ 之后的部分，否则 host 会变成密码的一截
    expect(proxyExitHost('socks5://user:p@ss@9.9.9.9:1080')).toBe('9.9.9.9:1080')
  })
})

describe('countExitUsage', () => {
  test('按出口统计启用中的账号数', () => {
    const counts = countExitUsage([
      { proxyUrl: 'socks5://u:p@1.1.1.1:1080' },
      { proxyUrl: 'socks5://x:y@1.1.1.1:1080' },
      { proxyUrl: 'socks5://u:p@2.2.2.2:1080' },
    ])
    expect(counts.get('1.1.1.1:1080')).toBe(2)
    expect(counts.get('2.2.2.2:1080')).toBe(1)
  })

  test('认证信息不同但出口相同时算作同一个', () => {
    // 机场会轮换密码，按完整 URL 去重会把同一个 IP 拆成两个
    const counts = countExitUsage([
      { proxyUrl: 'socks5://old:pass@1.1.1.1:1080' },
      { proxyUrl: 'socks5://new:secret@1.1.1.1:1080' },
    ])
    expect(counts.get('1.1.1.1:1080')).toBe(2)
  })

  test('不数已禁用的号', () => {
    // 判死的号还留在列表里（保留期内），算进去会让早就没人用的出口显示成热门
    const counts = countExitUsage([
      { proxyUrl: 'socks5://u:p@1.1.1.1:1080' },
      { proxyUrl: 'socks5://u:p@1.1.1.1:1080', disabled: true },
    ])
    expect(counts.get('1.1.1.1:1080')).toBe(1)
  })

  test('没配代理的号不进统计', () => {
    const counts = countExitUsage([{ proxyUrl: null }, { proxyUrl: undefined }])
    expect(counts.size).toBe(0)
  })
})
