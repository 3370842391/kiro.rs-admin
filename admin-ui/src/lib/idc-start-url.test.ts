import { describe, expect, test } from 'bun:test'
import {
  applyIdcStartUrlInput,
  authRegionFromPortalHost,
  isIpv4AccessPortal,
  rewriteIpv4PortalUrl,
} from './idc-start-url'

describe('idc start url', () => {
  test('reads auth region from dual-stack portal host', () => {
    expect(
      authRegionFromPortalHost(
        'ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws',
      ),
    ).toBe('ap-southeast-1')
  })

  test('rewrites the known IPv4 alias and fills ap-southeast-1', () => {
    expect(applyIdcStartUrlInput('https://jarvisclaw.awsapps.com/start')).toEqual({
      startUrl: 'https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws',
      region: 'ap-southeast-1',
    })
  })

  test('keeps unknown awsapps portals for the backend to handle', () => {
    expect(rewriteIpv4PortalUrl('https://other-org.awsapps.com/start')).toBe(
      'https://other-org.awsapps.com/start',
    )
    expect(isIpv4AccessPortal('https://other-org.awsapps.com/start')).toBe(true)
    expect(isIpv4AccessPortal('https://view.awsapps.com/start')).toBe(false)
  })
})
