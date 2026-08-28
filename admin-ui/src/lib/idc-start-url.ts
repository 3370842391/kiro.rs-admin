/** 仅 IPv4 的 Access Portal → 双栈门户。与后端 `IPV4_PORTAL_ALIASES` 对齐。 */
const IPV4_PORTAL_ALIASES: Record<string, string> = {
  'jarvisclaw.awsapps.com':
    'https://ssoins-821071a5e59b0789.portal.ap-southeast-1.app.aws',
}

export function hostFromStartUrl(raw: string): string | null {
  const trimmed = raw.trim()
  if (!trimmed) return null
  try {
    const url = new URL(trimmed.includes('://') ? trimmed : `https://${trimmed}`)
    return url.hostname.toLowerCase()
  } catch {
    return null
  }
}

/** `ssoins-xxx.portal.ap-southeast-1.app.aws` → `ap-southeast-1` */
export function authRegionFromPortalHost(host: string): string | null {
  const normalized = host.replace(/\.$/, '').toLowerCase()
  if (!normalized.endsWith('.app.aws')) return null
  const marker = '.portal.'
  const idx = normalized.lastIndexOf(marker)
  if (idx < 0) return null
  const region = normalized.slice(idx + marker.length, -'.app.aws'.length)
  return region.includes('-') ? region : null
}

export function rewriteIpv4PortalUrl(raw: string): string {
  const host = hostFromStartUrl(raw)
  if (!host) return raw
  return IPV4_PORTAL_ALIASES[host] ?? raw
}

export function applyIdcStartUrlInput(raw: string): { startUrl: string; region?: string } {
  const startUrl = rewriteIpv4PortalUrl(raw).replace(/\/$/, '')
  const host = hostFromStartUrl(startUrl)
  const region = host ? authRegionFromPortalHost(host) ?? undefined : undefined
  return region ? { startUrl, region } : { startUrl }
}

export function isIpv4AccessPortal(raw: string): boolean {
  const host = hostFromStartUrl(raw)
  return !!host && host.endsWith('.awsapps.com') && host !== 'view.awsapps.com'
}
