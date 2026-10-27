/**
 * How numbers read in this window.
 *
 * Chinese short units, one decimal, because a dashboard of six-figure token
 * counts is unreadable as a full integer and the phone already speaks this way.
 */
export function formatCount(value: number): string {
  if (!Number.isFinite(value)) return '—'
  if (value < 10_000) return value.toLocaleString('zh-CN')
  if (value < 100_000_000) return `${trim(value / 10_000)} 万`
  return `${trim(value / 100_000_000)} 亿`
}

function trim(value: number): string {
  return value.toFixed(1).replace(/\.0$/, '')
}
