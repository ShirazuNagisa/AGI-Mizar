/**
 * The toolbox: the capabilities this app ships, one row each.
 *
 * A row is three things at once — what the tool is, a switch that turns it on,
 * and a way into the tool's own settings. They are separate controls on
 * purpose: flipping a switch must not also navigate, and opening the settings
 * must not toggle anything.
 */
import type { ToolView } from '../api'
import { ChevronRightIcon, LinkIcon, ToolboxIcon } from '../icons'
import { cn } from '../lib/utils'
import { Switch } from './ui/switch'
import { Toolbar } from './Toolbar'

/** The mark each built-in tool draws beside its name. */
function ToolMark({ id }: { id: string }) {
  if (id === 'interconnect') return <LinkIcon size={18} />
  if (id === 'computer-use') return <ToolboxIcon size={18} />
  return <ToolboxIcon size={18} />
}

export function ToolboxPage({
  tools,
  busy,
  onOpen,
  onToggle,
}: {
  tools: ToolView[]
  /** The tool whose switch is in flight, if any. */
  busy: string | null
  onOpen: (tool: ToolView) => void
  onToggle: (tool: ToolView, enabled: boolean) => void
}) {
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="工具箱" />
      <div className="flex flex-col gap-1 px-6 pt-4 pb-6">
        <p className="max-w-[52ch] text-[12px] leading-5 text-muted">
          这台电脑内置的 agent 工具。每个工具可以单独开关，点右侧箭头进入它的设置。
        </p>
      </div>
      <div className="flex-1 overflow-y-auto">
        {tools.length === 0 ? (
          <p className="px-6 text-[12px] text-muted">这个版本没有内置工具。</p>
        ) : null}
        {tools.map((tool) => (
          <div
            key={tool.id}
            className="mz-divide-b flex items-center gap-4 px-6 py-3.5"
          >
            <button
              type="button"
              onClick={() => onOpen(tool)}
              className="flex min-w-0 flex-1 items-center gap-3 text-left outline-none focus-visible:ring-2 focus-visible:ring-accent/50"
            >
              <span className="shrink-0 text-fg" aria-hidden>
                <ToolMark id={tool.id} />
              </span>
              <span className="flex min-w-0 flex-col gap-0.5">
                <span className="flex items-center gap-2">
                  <span className="text-[13px]">{tool.name}</span>
                  {tool.available ? null : (
                    <span className="shrink-0 text-[9px] leading-none text-muted">本平台不可用</span>
                  )}
                </span>
                <span className="truncate text-[11px] leading-4 text-muted">{tool.summary}</span>
              </span>
            </button>
            <div className="flex shrink-0 items-center gap-3">
              <Switch
                checked={tool.enabled}
                disabled={!tool.available || busy === tool.id}
                label={`${tool.name}${tool.enabled ? '已开启' : '已关闭'}`}
                onCheckedChange={(next) => onToggle(tool, next)}
              />
              <button
                type="button"
                onClick={() => onOpen(tool)}
                aria-label={`打开 ${tool.name} 设置`}
                className={cn(
                  'flex size-7 items-center justify-center rounded-full text-muted outline-none',
                  'hover:bg-surface-2 hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/50',
                )}
              >
                <ChevronRightIcon size={16} />
              </button>
            </div>
          </div>
        ))}
      </div>
    </div>
  )
}
