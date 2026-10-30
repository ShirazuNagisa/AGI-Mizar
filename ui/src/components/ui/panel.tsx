import type { HTMLAttributes } from 'react'
import { cn } from '../../lib/utils'

/**
 * The one bordered surface this theme has.
 *
 * There is no shadow and no radius to separate a panel from the page, so the
 * border does all the work. Every card used to spell out `border
 * border-ink` by hand; routing them through here means the next theme change
 * touches one line.
 */
export function Panel({ className, ...props }: HTMLAttributes<HTMLDivElement>) {
 return <div className={cn('border border-ink', className)} {...props} />
}

/** A panel with a titled strip on top, for grouped settings. */
export function PanelGroup({
 title,
 className,
 children,
}: HTMLAttributes<HTMLDivElement> & { title: string }) {
 return (
    <div className={cn('border border-ink', className)}>
      <div className="border-b border-ink px-3.5 py-1.5">{title}</div>
      <div className="px-3.5 py-2.5">{children}</div>
    </div>
 )
}
