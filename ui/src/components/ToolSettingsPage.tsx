/**
 * One tool's own settings page.
 *
 * The toolbox row opens this; the back control returns to the list. The bodies
 * are not rebuilt here — Agent 互联 reuses the surface that used to live in
 * Settings, and computer use has a page of its own — so there is one
 * implementation per tool no matter which door opened it.
 */
import type { Kernel, ToolView } from '../api'
import { ChevronRightIcon } from '../icons'
import { cn } from '../lib/utils'
import { ComputerUseSettings } from './ComputerUseSettings'
import { RemoteAgentSettings } from './RemoteAgentSettings'
import { Toolbar } from './Toolbar'

export function ToolSettingsPage({
  tool,
  kernel,
  selfHosted,
  busy,
  onBack,
  onToggle,
}: {
  tool: ToolView
  kernel: Kernel
  /** Which relay this install talks to; Agent 互联's pairing surface is self-hosted only. */
  selfHosted: boolean
  /** Whether this tool's switch is in flight. */
  busy: boolean
  onBack: () => void
  onToggle: (enabled: boolean) => void
}) {
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar
        title={tool.name}
        leading={
          <button
            type="button"
            onClick={onBack}
            aria-label="返回工具箱"
            className={cn(
              'mr-1 flex size-7 items-center justify-center rounded-full text-muted outline-none',
              'hover:bg-surface-2 hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/50',
            )}
          >
            <ChevronRightIcon size={16} className="rotate-180" />
          </button>
        }
      />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
          {tool.id === 'interconnect' ? (
            <RemoteAgentSettings kernel={kernel} selfHosted={selfHosted} />
          ) : null}
          {tool.id === 'computer-use' ? (
            <ComputerUseSettings tool={tool} kernel={kernel} busy={busy} onToggle={onToggle} />
          ) : null}
          {tool.id !== 'interconnect' && tool.id !== 'computer-use' ? (
            <p className="text-[12px] text-muted">这个工具还没有设置页。</p>
          ) : null}
        </div>
      </div>
    </div>
  )
}
