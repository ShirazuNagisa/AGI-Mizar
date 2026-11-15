import type { ServerProbeResult } from '../api'

/**
 * A connectivity check's failure code, said the way a person can act on it.
 *
 * The kernel answers with stable codes and an English sentence for the log; the
 * window is where the Chinese copy lives. Both the guide's server step and the
 * settings page ask the same question, so they share this table — two places
 * wording "wrong PIN" differently is two places to fix.
 */
export function probeFailureCopy(probe: ServerProbeResult): string {
  switch (probe.error) {
    case 'invalid_origin':
      return '域名格式不对：填域名或 https://域名，不要带路径。'
    case 'network':
      return '连不上：检查域名、DNS、端口和证书是否有效。'
    case 'not_relay':
      return '这个地址不是 DSH Mizar relay（/healthz 没有返回 dsh-mizar-relay）。'
    case 'protocol_too_old':
      return '服务器版本太旧，请先升级 relay。'
    case 'pin_required':
      return '这台服务器需要 PIN。'
    case 'pin_invalid':
      return 'PIN 不正确。'
    case 'rate_limited':
      return '尝试次数过多，请稍后再试。'
    default:
      return probe.message ?? '验证失败。'
  }
}

/** What a passing check found, as one short line. */
export function probeSuccessCopy(probe: ServerProbeResult): string {
  const facts = [
    probe.relayVersion === null ? null : `relay ${probe.relayVersion}`,
    probe.protocol === null ? null : `协议 ${probe.protocol}`,
    probe.pinOk ? 'PIN 已确认' : null,
  ].filter((part): part is string => part !== null)
  return facts.length === 0 ? '验证通过' : `验证通过：${facts.join(' · ')}`
}
