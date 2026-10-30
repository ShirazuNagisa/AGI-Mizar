import { cn } from '../../lib/utils'

interface ProgressProps {
  /** 0–100; values outside the range are clamped. */
  value: number
  label: string
  className?: string
}

/**
 * One bar, no library.
 *
 * Models are a gigabyte, so the number is what tells the truth; the bar is the
 * shape of it. `aria-valuenow` carries the same number for anything that is not
 * looking at pixels.
 */
export function Progress({ value, label, className }: ProgressProps) {
  const percent = Math.max(0, Math.min(100, Math.round(value)))
  return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      className={cn('h-1.5 w-full overflow-hidden rounded-full bg-surface-2', className)}
    >
      <div
        className="h-full rounded-full bg-accent transition-[width] duration-300"
        style={{ width: `${percent}%` }}
      />
    </div>
  )
}
