import { Slot } from '@radix-ui/react-slot'
import { cva, type VariantProps } from 'class-variance-authority'
import type { ButtonHTMLAttributes } from 'react'
import { cn } from '../../lib/utils'
import { WipeLayers } from './wipe'

/**
 * 1-bit buttons.
 *
 * There is no shading to lean on, so a button is either filled or outlined, and
 * hover swaps the two. The swap is a sweep that arrives from the left (see the
 * `.wipe` rules in styles.css): the fill slides in and the label inverts under
 * it, so the change reads as one edge crossing the button rather than the whole
 * control blinking. Leaving runs the same transition backwards.
 *
 * Disabled is a dashed edge — the one convention that survives without a grey.
 */
const button = cva(
  'inline-flex select-none items-center justify-center border text-[12px] outline-none disabled:pointer-events-none disabled:border-dashed disabled:bg-transparent disabled:text-ink',
  {
    variants: {
      variant: {
        primary: 'border-ink bg-ink text-bg',
        outline: 'border-ink bg-transparent text-ink',
        subtle: 'border-transparent bg-transparent text-ink',
        ghost: 'border-transparent bg-transparent text-ink',
      },
      size: {
        sm: 'h-7',
        md: 'h-8',
        lg: 'h-9',
      },
    },
    defaultVariants: { variant: 'outline', size: 'md' },
  },
)

/** The label's box. The padding lives here so both copies share it exactly. */
const label = {
  sm: 'gap-1.5 px-2.5',
  md: 'gap-1.5 px-3',
  lg: 'gap-1.5 px-4',
}

export interface ButtonProps
  extends ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof button> {
  asChild?: boolean
}

export function Button({
  className,
  variant,
  size,
  asChild,
  children,
  ...props
}: ButtonProps) {
  // `asChild` hands the element to the caller, so there is nowhere to put the
  // second copy of the label; that path keeps the plain colour swap.
  if (asChild) {
    return (
      <Slot className={cn(button({ variant, size }), className)} {...props}>
        {children}
      </Slot>
    )
  }

  return (
    <button className={cn('wipe', button({ variant, size }), className)} {...props}>
      <WipeLayers
        tone={variant === 'primary' ? 'drain' : 'fill'}
        contentClassName={cn('flex items-center justify-center', label[size ?? 'md'])}
      >
        {children}
      </WipeLayers>
    </button>
  )
}
