import { describe, expect, test } from 'bun:test'
import { proxyDisplayHost, proxySchemeLabel } from './utils'

describe('proxyDisplayHost', () => {
  test('strips scheme and credentials', () => {
    expect(proxyDisplayHost('socks5://user:pass@216.175.194.72:443')).toBe(
      '216.175.194.72:443',
    )
  })

  test('keeps host when there is no userinfo', () => {
    expect(proxyDisplayHost('http://1.2.3.4:8080/path')).toBe('1.2.3.4:8080')
  })

  test('handles password containing @', () => {
    expect(proxyDisplayHost('socks5://u:p@x@10.0.0.1:1080')).toBe('10.0.0.1:1080')
  })

  test('treats direct as direct', () => {
    expect(proxyDisplayHost('direct')).toBe('direct')
  })
})

describe('proxySchemeLabel', () => {
  test('reads scheme', () => {
    expect(proxySchemeLabel('socks5://u:p@1.2.3.4:1080')).toBe('socks5')
  })

  test('empty without scheme', () => {
    expect(proxySchemeLabel('1.2.3.4:1080')).toBe('')
  })
})
