import type { ReactNode } from 'react'

interface ToolbarProps {
  title: string
  children?: ReactNode
  /** A mark shown beside the title, e.g. an agent's connection lamp. */
  badge?: ReactNode
}

/**
 * The strip every page shares: what this page is, on the left, and what it can
 * do, on the right. It is also a drag region, so a window without a title bar
 * still moves by its top edge.
 */
export function Toolbar({ title, badge, children }: ToolbarProps) {
  return (
    <header
      data-tauri-drag-region="deep"
      className="flex h-12 shrink-0 items-center gap-2 border-b border-border px-6"
    >
      <h1 className="text-[13px] font-semibold text-fg">{title}</h1>
      {badge}
      <div className="flex-1" />
      {children}
    </header>
  )
}
