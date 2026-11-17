/**
 * The computer-use tool's settings.
 *
 * One job: tell the operator whether this machine can actually drive apps, and
 * what to fix when it cannot. The tool itself lives in `computer-use-kernel/`
 * (a separate macOS process); this page never pretends to own its state — it
 * asks the kernel for a dependency report and draws the answer.
 */
import { useState } from 'react'
import type { Kernel, ToolDependencyReport, ToolView } from '../api'
import { Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'
import { Button } from './ui/button'
import { PanelGroup } from './ui/panel'
import { Switch } from './ui/switch'

/** One dependency's verdict, drawn as the same filled/hollow/dashed language
 * the sidebar uses for its lamps (there is no colour to spend on a status). */
function StatusMark({ status }: { status: string }) {
  if (status === 'ok') return <span className="size-2 shrink-0 rounded-full bg-fg" aria-hidden />
  if (status === 'unknown') {
    return (
      <span
        className="size-2 shrink-0 rounded-full border border-fg/40"
        style={{
          backgroundImage: 'repeating-conic-gradient(currentColor 0 25%, transparent 0 50%)',
          backgroundSize: '2px 2px',
        }}
        aria-hidden
      />
    )
  }
  return <span className="size-2 shrink-0 rounded-full border border-fg" aria-hidden />
}

function ReportView({ report }: { report: ToolDependencyReport }) {
  return (
    <div className="flex flex-col gap-2">
      <p className={cn('text-[12px]', report.ok ? 'text-fg' : 'text-muted')}>{report.summary}</p>
      <ul className="flex flex-col">
        {report.items.map((item) => (
          <li key={item.id} className="mz-divide-t flex items-start gap-3 py-2">
            <span className="mt-1.5">
              <StatusMark status={item.status} />
            </span>
            <span className="flex min-w-0 flex-col gap-0.5">
              <span className="text-[12px]">{item.label}</span>
              <span className="break-words text-[11px] leading-4 text-muted">{item.detail}</span>
              {item.fix === null ? null : (
                <span className="text-[11px] leading-4 text-muted">修复：{item.fix}</span>
              )}
            </span>
          </li>
        ))}
      </ul>
      {report.hint === null ? null : (
        <p className="text-[11px] leading-5 text-muted">{report.hint}</p>
      )}
    </div>
  )
}

export function ComputerUseSettings({
  tool,
  kernel,
  busy,
  onToggle,
}: {
  tool: ToolView
  kernel: Kernel
  busy: boolean
  onToggle: (enabled: boolean) => void
}) {
  const [report, setReport] = useState<ToolDependencyReport | null>(null)
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function check() {
    setChecking(true)
    setError(null)
    try {
      setReport(await kernel.send<ToolDependencyReport>({ op: 'tool_dependencies', tool: 'computer-use' }))
    } catch (failure) {
      setError(failureMessage(failure, '检查依赖完整性没有成功'))
    } finally {
      setChecking(false)
    }
  }

  return (
    <div className="flex flex-col gap-6 pb-10">
      <p className="max-w-[52ch] text-[12px] leading-5 text-muted">
        让 agent 读界面、点按钮、打字。内核是一个独立的 macOS 进程，通过 MCP 暴露工具；开启后
        本机的 agent 可以操作应用。
      </p>

      <PanelGroup title="开关">
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <p className="text-[12px]">启用 computer use</p>
            <p className="mt-1 text-[11px] leading-5 text-muted">
              {tool.available
                ? '开启后本机 agent 会多出一组电脑操作工具；第一次使用前请先检查依赖。'
                : 'computer use 目前只在 macOS 上提供。'}
            </p>
          </div>
          <Switch
            checked={tool.enabled}
            disabled={!tool.available || busy}
            label={`computer use${tool.enabled ? '已开启' : '已关闭'}`}
            onCheckedChange={(next) => onToggle(next)}
          />
        </div>
      </PanelGroup>

      <section>
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
              <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
              依赖完整性
            </h2>
            <p className="mt-1 text-[12px] leading-5 text-muted">
              检查电脑操作内核、它的版本，以及辅助功能与屏幕录制两项系统权限。
            </p>
          </div>
          <Button
            className="shrink-0"
            size="sm"
            variant="outline"
            onClick={() => void check()}
            disabled={checking}
          >
            {checking ? <Spinner /> : null}
            {checking ? '正在检查…' : '检查依赖完整性'}
          </Button>
        </div>

        {error === null ? null : (
          <p className="mt-3 bg-ink px-3 py-1.5 text-[12px] leading-5 text-bg">{error}</p>
        )}

        <div className="mt-3">
          {report === null ? (
            <p className="text-[12px] text-muted">
              {checking ? '正在检查…' : '还没有检查。检查是只读的，不会改动机器上的任何东西。'}
            </p>
          ) : (
            <ReportView report={report} />
          )}
        </div>
      </section>
    </div>
  )
}
