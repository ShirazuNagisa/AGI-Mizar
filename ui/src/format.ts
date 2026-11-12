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

/**
 * A model version as the repository tags it: `v1`, `v2.1`.
 *
 * The repository numbers models by release, not by semver patch: the shipped
 * manifest may say `1.0.0` while the tag says `v1`, and showing both forms in
 * one window reads as two different models. Trailing zeros are dropped for the
 * same reason.
 */
export function formatModelVersion(version: string): string {
  const bare = version.trim().replace(/^v/i, '')
  if (bare === '') return version
  const trimmed = bare.replace(/(\.0)+$/, '')
  return `v${trimmed === '' ? '0' : trimmed}`
}

/**
 * A file size as a download reads it: gigabytes, two decimals.
 *
 * Models are about a gigabyte and people compare them by that number, so even a
 * small sidecar is worth saying in the same unit rather than switching to
 * megabytes halfway through a progress line.
 */
export function formatGigabytes(bytes: number): string {
  if (!Number.isFinite(bytes) || bytes <= 0) return '0.00 GB'
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`
}

/**
 * A download speed as the progress line reads it: MB/s, one decimal under 10.
 *
 * Decimal megabytes on purpose: a person comparing 0.4 and 12 is looking at the
 * order of magnitude, not at the binary difference between MB and MiB.
 */
export function formatSpeed(bytesPerSecond: number): string {
  if (!Number.isFinite(bytesPerSecond) || bytesPerSecond <= 0) return '—'
  const megabytes = bytesPerSecond / 1_000_000
  return megabytes >= 10
    ? `${megabytes.toFixed(0)} MB/s`
    : `${megabytes.toFixed(1)} MB/s`
}

/**
 * The time a download has left, in the shortest form that is still useful.
 *
 * A gigabyte over a fast link is seconds and over a slow one is hours (the
 * release CDN from China measured 29 hours), so both ends exist and neither
 * should read as `0:00:07`.
 */
export function formatEta(seconds: number | null): string | null {
  if (seconds === null || !Number.isFinite(seconds) || seconds <= 0) return null
  if (seconds < 60) return `剩余 ${Math.ceil(seconds)} 秒`
  if (seconds < 3600) return `剩余 ${Math.ceil(seconds / 60)} 分钟`
  return `剩余 ${(seconds / 3600).toFixed(1)} 小时`
}

/**
 * A wallet amount as the agent page shows it: two decimals, rounded.
 *
 * DSH reports the provider's decimal string verbatim, often with eight digits
 * (`9.86578102`). That precision belongs in accounting, not in a card read from
 * across the desk, so the window rounds for display only — the stored value and
 * the API stay exact.
 *
 * The rounding is done on the string rather than through `toFixed`: `1.005` is
 * stored as slightly less than 1.005 as a binary float, so `toFixed(2)` would
 * answer `1.00` while a person, and the phone's `Decimal` formatter, expect
 * `1.01`. Exponent forms (`1e-8`, which the balance grammar allows) fall back to
 * the numeric path; they are not the shape a wallet is written in.
 */
export function formatAmount(amount: string): string {
  const text = amount.trim()
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(text)
  if (match === null) {
    const value = Number(text)
    if (!Number.isFinite(value)) return amount
    return value.toFixed(2)
  }

  const [, sign = '', whole = '0', fraction = ''] = match
  let cents = Number(fraction.slice(0, 2).padEnd(2, '0'))
  // The third digit decides, exactly like the phone's NSDecimalRound.
  if (Number(fraction.slice(2, 3) || '0') >= 5) cents += 1
  let integer = whole.replace(/^0+(?=\d)/, '')
  if (cents === 100) {
    cents = 0
    // Two decimal places on a balance never approach 2^53, but the expression
    // is written so a future `bigint` swap has one obvious place to land.
    integer = String(Number(integer) + 1)
  }
  return `${sign}${integer}.${String(cents).padStart(2, '0')}`
}
