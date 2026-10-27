import * as SelectPrimitive from '@radix-ui/react-select'
import { CheckIcon, ChevronIcon } from '../../icons'
import { cn } from '../../lib/utils'

/**
 * A select that reads like a settings row: the chosen value on the right, the
 * list in a small floating panel. Built on Radix so keyboard and focus work
 * without this window reimplementing them.
 */
export const Select = SelectPrimitive.Root
export const SelectValue = SelectPrimitive.Value

export function SelectTrigger({
  className,
  children,
  ...props
}: SelectPrimitive.SelectTriggerProps) {
  return (
    <SelectPrimitive.Trigger
      className={cn(
        'inline-flex h-8 min-w-[7.5rem] items-center justify-between gap-2 rounded-lg border border-border bg-transparent px-2.5 text-[13px] text-fg outline-none hover:bg-surface-2 focus-visible:ring-2 focus-visible:ring-accent/60 data-[placeholder]:text-muted',
        className,
      )}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon asChild>
        <ChevronIcon size={14} className="text-muted" />
      </SelectPrimitive.Icon>
    </SelectPrimitive.Trigger>
  )
}

export function SelectContent({
  className,
  children,
  ...props
}: SelectPrimitive.SelectContentProps) {
  return (
    <SelectPrimitive.Portal>
      <SelectPrimitive.Content
        position="popper"
        sideOffset={6}
        className={cn(
          'z-50 min-w-[8rem] overflow-hidden rounded-lg border border-border bg-surface p-1 shadow-lg shadow-black/20',
          className,
        )}
        {...props}
      >
        <SelectPrimitive.Viewport>{children}</SelectPrimitive.Viewport>
      </SelectPrimitive.Content>
    </SelectPrimitive.Portal>
  )
}

export function SelectItem({
  className,
  children,
  ...props
}: SelectPrimitive.SelectItemProps) {
  return (
    <SelectPrimitive.Item
      className={cn(
        'relative flex h-7 cursor-default select-none items-center gap-2 rounded-md pl-7 pr-2 text-[13px] text-fg outline-none data-[highlighted]:bg-surface-2',
        className,
      )}
      {...props}
    >
      <span className="absolute left-2 inline-flex size-3.5 items-center justify-center text-accent">
        <SelectPrimitive.ItemIndicator>
          <CheckIcon size={13} />
        </SelectPrimitive.ItemIndicator>
      </span>
      <SelectPrimitive.ItemText>{children}</SelectPrimitive.ItemText>
    </SelectPrimitive.Item>
  )
}
