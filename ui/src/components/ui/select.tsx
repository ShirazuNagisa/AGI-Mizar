import * as SelectPrimitive from '@radix-ui/react-select'
import { CheckIcon, ChevronIcon } from '../../icons'
import { cn } from '../../lib/utils'

/**
 * A select that reads like a settings row: the chosen value on the right, the
 * list in a small floating panel. Built on Radix so keyboard and focus work
 * without this window reimplementing them.
 *
 * 1-bit: the trigger inverts on hover, and the highlighted item is a filled row
 * with knocked-out text — the same trick the sidebar uses for the active page.
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
        'inline-flex h-8 min-w-[7.5rem] items-center justify-between gap-2 border border-ink bg-transparent px-2.5 text-[12px] text-ink outline-none hover:bg-ink hover:text-bg data-[placeholder]:text-ink',
        className,
      )}
      {...props}
    >
      {children}
      <SelectPrimitive.Icon asChild>
        <ChevronIcon size={14} />
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
        sideOffset={4}
        className={cn('z-50 min-w-[8rem] border border-ink bg-bg p-1', className)}
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
        'relative flex h-7 cursor-default select-none items-center gap-2 pl-7 pr-2 text-[12px] text-ink outline-none data-[highlighted]:bg-ink data-[highlighted]:text-bg',
        className,
      )}
      {...props}
    >
      <span className="absolute left-2 inline-flex size-3.5 items-center justify-center">
        <SelectPrimitive.ItemIndicator>
          <CheckIcon size={13} />
        </SelectPrimitive.ItemIndicator>
      </span>
      <SelectPrimitive.ItemText>{children}</SelectPrimitive.ItemText>
    </SelectPrimitive.Item>
 )
}
