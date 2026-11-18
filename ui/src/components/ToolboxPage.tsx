/**
 * The toolbox: the capabilities this app ships, one row each.
 *
 * A row is three things at once — what the tool is, a switch that turns it on,
 * and a way into the tool's own settings. They are separate controls on
 * purpose: flipping a switch must not also navigate, and opening the settings
 * must not toggle anything.
 */
import type { ToolDependencyReport, ToolView } from '../api'
import { ChevronRightIcon, LinkIcon, ToolboxIcon } from '../icons'
import { cn } from '../lib/utils'
import { Button } from './ui/button'
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
  reports,
  onOpen,
  onToggle,
  onOpenPane,
  onRecheck,
}: {
  tools: ToolView[]
  /** The tool whose switch is in flight, if any. */
  busy: string | null
  /** What enabling a tool repaired, or what it still needs. */
  reports: Record<string, ToolDependencyReport>
  onOpen: (tool: ToolView) => void
  onToggle: (tool: ToolView, enabled: boolean) => void
  onOpenPane: (tool: ToolView, item?: string) => void
  onRecheck: (tool: ToolView) => void
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
        {tools.map((tool) => {
          const report = reports[tool.id]
          const missing = report?.items.filter(
            (entry) => entry.status === 'missing' || entry.status === 'outdated',
          )
          return (
          <div key={tool.id} className="mz-divide-b flex flex-col px-6 py-3.5">
          <div className="flex items-center gap-4">
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
          {report === undefined ? null : (
            <div className="mt-2 flex flex-col gap-1.5 pl-8">
              {report.repaired.length === 0 ? null : (
                <p className="text-[11px] leading-4 text-muted">
                  已自动补全：{report.repaired.join('；')}
                </p>
              )}
              {missing !== undefined && missing.length === 0 ? (
                report.repaired.length === 0 ? (
                  <p className="text-[11px] leading-4 text-muted">{report.summary}</p>
                ) : null
              ) : (
                <div className="flex flex-wrap items-center gap-2">
                  {(missing?.filter(
                    (entry) => entry.id === 'screen' || entry.id === 'accessibility',
                  ).length ?? 0) > 1 ? (
                    <p className="w-full text-[11px] leading-4 text-muted">
                      这两项权限在两个不同的面板里，分别点下面的按钮勾选。
                    </p>
                  ) : null}
                  <p className="text-[11px] leading-4 text-muted">
                    还需要：{missing?.map((entry) => entry.label).join('、')}
                  </p>
                  {/* One button per pane: the two grants live in two different
                      panels, and sending both to the same one is how people end
                      up with half a permission. */}
                  {(missing ?? []).map((entry) => {
                    const pane = entry.id === 'screen' ? '屏幕录制' : entry.id === 'accessibility' ? '辅助功能' : null
                    if (pane === null) return null
                    return (
                      <Button
                        key={entry.id}
                        size="sm"
                        variant="outline"
                        onClick={() => onOpenPane(tool, entry.id)}
                      >
                        打开「{pane}」
                      </Button>
                    )
                  })}
                  <Button
                    size="sm"
                    variant="ghost"
                    disabled={busy === tool.id}
                    onClick={() => onRecheck(tool)}
                  >
                    重新检查
                  </Button>
                </div>
              )}
            </div>
          )}
          </div>
          )
        })}
      </div>
    </div>
  )
}
