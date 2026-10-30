import { Slot } from '@radix-ui/react-slot'
import { cva, type VariantProps } from 'class-variance-authority'
import type { ButtonHTMLAttributes } from 'react'
import { cn } from '../../lib/utils'

/**
 * 1-bit buttons.
 *
 * There is no shading to lean on, so a button is either filled or outlined and
 * hover swaps the two. Disabled is a dashed edge — the one convention that
 * survives without a grey.
 */
const button = cva(
 'inline-flex select-none items-center justify-center gap-1.5 border text-[12px] outline-none disabled:pointer-events-none disabled:border-dashed disabled:bg-transparent disabled:text-ink',
 {
    variants: {
      variant: {
        primary: 'border-ink bg-ink text-bg hover:bg-bg hover:text-ink',
        outline: 'border-ink bg-transparent text-ink hover:bg-ink hover:text-bg',
        subtle: 'border-transparent bg-transparent text-ink hover:bg-ink hover:text-bg',
        ghost: 'border-transparent bg-transparent text-ink hover:bg-ink hover:text-bg',
      },
      size: {
        sm: 'h-7 px-2.5',
        md: 'h-8 px-3',
        lg: 'h-9 px-4',
      },
    },
    defaultVariants: { variant: 'outline', size: 'md' },
 },
)

export interface ButtonProps
 extends ButtonHTMLAttributes<HTMLButtonElement>,
    VariantProps<typeof button> {
 asChild?: boolean
}

export function Button({ className, variant, size, asChild, ...props }: ButtonProps) {
 const Component = asChild ? Slot : 'button'
 return <Component className={cn(button({ variant, size }), className)} {...props} />
}
