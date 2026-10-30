import { cn } from '../../lib/utils'

interface SwitchProps {
  checked: boolean
  onCheckedChange: (checked: boolean) => void
  disabled?: boolean
  /** Describes the setting for assistive technology. */
  label: string
  id?: string
  className?: string
}

/**
 * An on/off control drawn from a button.
 *
 * Radix has no switch of its own to depend on here, and a native button with
 * `role="switch"` is what the accessibility tree expects anyway: space and
 * enter toggle it, and `aria-checked` is the state a screen reader reads.
 */
export function Switch({ checked, onCheckedChange, disabled, label, id, className }: SwitchProps) {
  return (
    <button
      type="button"
      id={id}
      role="switch"
      aria-checked={checked}
      aria-label={label}
      disabled={disabled}
      onClick={() => onCheckedChange(!checked)}
      className={cn(
        'relative inline-flex h-[20px] w-[34px] shrink-0 items-center rounded-full border transition-colors outline-none focus-visible:ring-2 focus-visible:ring-accent/60 disabled:pointer-events-none disabled:opacity-45',
        checked ? 'border-accent bg-accent' : 'border-border bg-surface-2',
        className,
      )}
    >
      <span
        className={cn(
          'size-[14px] rounded-full shadow-sm transition-transform',
          checked ? 'translate-x-[17px] bg-[#08161c]' : 'translate-x-[2px] bg-muted/70',
        )}
      />
    </button>
  )
}
