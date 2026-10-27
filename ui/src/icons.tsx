/**
 * The marks this window draws.
 *
 * The three engines have no icon assets on this machine, so each is a monochrome
 * glyph built from the same 24-unit grid: it takes the accent colour when its
 * agent is the one being looked at, and the muted colour otherwise. Drawing them
 * here rather than importing bitmaps keeps them sharp at every size and free of
 * the colour noise a brand-coloured logo would add to a one-colour sidebar.
 */

interface MarkProps {
  size?: number
  className?: string
}

/** DSH: the six-pointed star the app itself uses. */
export function DshMark({ size = 20, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <path d="M12 2.6 14.35 8.55 19.9 7.35 16.25 12l3.65 4.65-5.55-1.2L12 21.4l-2.35-5.95-5.55 1.2L7.75 12 4.1 7.35l5.55 1.2Z" />
    </svg>
  )
}

/** Claude: a six-petal asterisk, the shape the product mark reads as at 20px. */
export function ClaudeMark({ size = 20, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      className={className}
      aria-hidden
    >
      {[0, 60, 120].map((angle) => (
        <line
          key={angle}
          x1={12 + 7.4 * Math.cos((angle * Math.PI) / 180)}
          y1={12 + 7.4 * Math.sin((angle * Math.PI) / 180)}
          x2={12 - 7.4 * Math.cos((angle * Math.PI) / 180)}
          y2={12 - 7.4 * Math.sin((angle * Math.PI) / 180)}
        />
      ))}
    </svg>
  )
}

/** Codex: a terminal prompt in a rounded square. */
export function CodexMark({ size = 20, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <rect x="3.4" y="3.4" width="17.2" height="17.2" rx="5" />
      <path d="m8 9 3 3-3 3" />
      <path d="M13.4 15h2.8" />
    </svg>
  )
}

/** A gear, for settings. */
export function GearIcon({ size = 18, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <circle cx="12" cy="12" r="3.1" />
      <path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09a1.7 1.7 0 0 0-1.1-1.55 1.7 1.7 0 0 0-1.88.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06A1.7 1.7 0 0 0 4.6 15a1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09A1.7 1.7 0 0 0 4.6 8.9a1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06A1.7 1.7 0 0 0 9 4.6a1.7 1.7 0 0 0 1-1.55V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1 1.55 1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06A1.7 1.7 0 0 0 19.4 9v.09a1.7 1.7 0 0 0 1.55 1H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.51 1Z" />
    </svg>
  )
}

/** A person, for the login entry. */
export function PersonIcon({ size = 18, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.7}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <circle cx="12" cy="8" r="3.6" />
      <path d="M4.8 20c.9-3.3 3.7-5.2 7.2-5.2s6.3 1.9 7.2 5.2" />
    </svg>
  )
}

/** A chevron, for disclosure. */
export function ChevronIcon({ size = 16, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <path d="m6 9 6 6 6-6" />
    </svg>
  )
}

/** A small check, for the selected appearance. */
export function CheckIcon({ size = 14, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2.4}
      strokeLinecap="round"
      strokeLinejoin="round"
      className={className}
      aria-hidden
    >
      <path d="m5 12.5 4.5 4.5L19 7.5" />
    </svg>
  )
}

/** A spinner, for work this window is waiting on. */
export function Spinner({ size = 14, className }: MarkProps) {
  return (
    <svg
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2.4}
      strokeLinecap="round"
      className={className}
      aria-hidden
      style={{ animation: 'spin 0.8s linear infinite' }}
    >
      <path d="M12 3a9 9 0 1 0 9 9" />
      <style>{'@keyframes spin { to { transform: rotate(360deg) } }'}</style>
    </svg>
  )
}
