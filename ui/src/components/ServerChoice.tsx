import { CheckIcon } from '../icons'
import { cn } from '../lib/utils'

/**
 * One of the two relay cards, shared by the guide and the settings page.
 *
 * Selected inverts, in the same one-bit language as the agent cards: the window
 * has no colour to spend on "this one is chosen".
 */
export function ServerChoice({
  title,
  detail,
  hint,
  selected,
  onClick,
}: {
  title: string
  detail: string
  hint: string
  selected: boolean
  onClick: () => void
}) {
  return (
    <button
      type="button"
      aria-pressed={selected}
      onClick={onClick}
      className={cn(
        'flex flex-col gap-1 p-3 text-left outline-none mz-border transition-colors',
        selected ? 'bg-ink text-bg' : 'hover:bg-surface-2',
      )}
    >
      <span className="flex w-full items-center justify-between">
        <span className="text-[12px]">{title}</span>
        {selected ? <CheckIcon size={14} /> : null}
      </span>
      <span
        className={cn(
          'break-all font-mono text-[11px] leading-4',
          selected ? 'text-bg/80' : 'text-muted',
        )}
      >
        {detail}
      </span>
      <span className={cn('text-[11px] leading-4', selected ? 'text-bg/80' : 'text-muted')}>
        {hint}
      </span>
    </button>
  )
}
