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
 *
 * 1-bit: a box with a square that is filled when on and hollow when off. The
 * knob is not a circle because there are no circles in this theme.
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
        'relative inline-flex h-5 w-9 shrink-0 items-center border border-ink outline-none disabled:pointer-events-none disabled:border-dashed',
        className,
      )}
    >
      <span
        className={cn(
          'size-3 border border-ink',
          checked ? 'translate-x-[18px] bg-ink' : 'translate-x-[2px] bg-transparent',
        )}
      />
    </button>
 )
}
