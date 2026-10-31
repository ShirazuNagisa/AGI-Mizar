import { cn } from '../../lib/utils'
import { useIsOled } from '../../lib/oled'

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
 *
 * 1-bit: an outlined trough with a solid fill. Width is the only thing that
 * animates — colour transitions would invent intermediate shades.
 */
export function Progress({ value, label, className }: ProgressProps) {
 const oled = useIsOled()
 const percent = Math.max(0, Math.min(100, Math.round(value)))
 return (
    <div
      role="progressbar"
      aria-label={label}
      aria-valuemin={0}
      aria-valuemax={100}
      aria-valuenow={percent}
      className={cn(
        'h-2 w-full overflow-hidden',
        oled ? 'border border-ink' : 'rounded-full bg-surface-2',
        className,
      )}
    >
      <div
        className={cn('h-full transition-[width] duration-150', oled ? 'bg-ink' : 'rounded-full bg-accent')}
        style={{ width: `${percent}%` }}
      />
    </div>
 )
}
