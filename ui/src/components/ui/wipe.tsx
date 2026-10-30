import type { ButtonHTMLAttributes, ReactNode } from 'react'
import { cn } from '../../lib/utils'

/**
 * Which colour the sweep arrives in.
 *
 * `fill` is the ordinary case: the button rests as an outline and fills with
 * ink. `drain` is for a button that already rests filled — the sweep is the
 * page colour, so it reads as the fill leaving the same way it would arrive.
 */
export type WipeTone = 'fill' | 'drain'

interface LayerProps {
  tone?: WipeTone
  /**
   * The label's layout. Both copies get it verbatim, because a copy that lays
   * out one pixel differently shows as a doubled edge mid-sweep.
   */
  contentClassName?: string
  children: ReactNode
}

/**
 * The three layers a sweeping button is made of.
 *
 * The fill and the inverted copy are revealed by the same clock: the fill
 * scales from the left edge and the copy is clipped to the same width, so a
 * glyph flips colour exactly when the edge crosses it.
 */
export function WipeLayers({ tone = 'fill', contentClassName, children }: LayerProps) {
  return (
    <>
      <span aria-hidden className={cn('wipe-layer', tone === 'fill' ? 'bg-ink' : 'bg-bg')} />
      <span className={cn('wipe-content', contentClassName)}>{children}</span>
      <span
        aria-hidden
        className={cn('wipe-overlay', contentClassName, tone === 'fill' ? 'text-bg' : 'text-ink')}
      >
        {children}
      </span>
    </>
  )
}

interface WipeButtonProps extends ButtonHTMLAttributes<HTMLButtonElement> {
  tone?: WipeTone
  contentClassName?: string
}

/**
 * A button whose hover state slides in from the left instead of switching.
 *
 * Used by the places that draw their own button rather than going through
 * `Button` — the sidebar rows and the segmented controls — so the whole window
 * sweeps the same way.
 */
export function WipeButton({
  tone,
  contentClassName,
  className,
  children,
  type = 'button',
  ...props
}: WipeButtonProps) {
  return (
    <button type={type} className={cn('wipe', className)} {...props}>
      <WipeLayers tone={tone} contentClassName={contentClassName}>
        {children}
      </WipeLayers>
    </button>
  )
}
