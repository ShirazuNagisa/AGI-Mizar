/**
 * The remote-agent settings surface.
 *
 * One panel, because the page's other half is gone: this computer is the target
 * (who may use it, in which folders), and the setup that makes that possible is
 * a dependency check rather than a list of knobs. The delegation list, the LAN
 * certificate and the audit tail live in the agent surfaces and the log export;
 * repeating them here made the page a dashboard instead of a setting.
 */
import { useEffect, useState } from 'react'
import type { Kernel, RemoteHealthReport, RemotePrefs } from '../api'
import { Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'
import { desktopPlatform } from '../platform'
import { exportText, shellAvailable } from '../shell'
import { Button } from './ui/button'
import { PanelGroup } from './ui/panel'
import { Switch } from './ui/switch'

export function RemoteAgentSettings({ kernel }: { kernel: Kernel }) {
  const [prefs, setPrefs] = useState<RemotePrefs | null>(null)
  const [workspaces, setWorkspaces] = useState('')
  const [blocked, setBlocked] = useState('')
  const [busy, setBusy] = useState<'save' | 'check' | 'export' | null>(null)
  const [health, setHealth] = useState<RemoteHealthReport | null>(null)
  const [healthFailed, setHealthFailed] = useState(false)
  const [checking, setChecking] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [saved, setSaved] = useState(false)
  const [exported, setExported] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const next = await kernel.send<RemotePrefs>({ op: 'remote_target_settings' })
        if (cancelled) return
        setPrefs(next)
        setWorkspaces(next.workspaces.join('\n'))
        setBlocked(next.blocked.join('\n'))
      } catch (failure) {
        if (!cancelled) setError(failureMessage(failure, '读取远程 Agent 设置失败'))
      }
    })()
    // The check is read-only and is the first thing a person would press; run it
    // once so the page opens with a verdict instead of a blank box.
    void (async () => {
      try {
        const next = await kernel.send<RemoteHealthReport>({ op: 'remote_health' })
        if (!cancelled) setHealth(next)
      } catch {
        // The report box says so instead of waiting forever; the button retries
        // with the sentence in the page's error line.
        if (!cancelled) setHealthFailed(true)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [kernel])

  async function save(next: RemotePrefs) {
    setPrefs(next)
    setSaved(false)
    setBusy('save')
    setError(null)
    try {
      await kernel.send<RemotePrefs>({
        op: 'set_remote_target_settings',
        enabled: next.enabled,
        allow_lan: next.allowLan,
        allow_wan: next.allowWan,
        allow_all_workspaces: next.allowAllWorkspaces,
        workspaces: next.workspaces,
        blocked: next.blocked,
      })
      setSaved(true)
      // The policy is one of the items, so the verdict has to follow the switch
      // rather than wait for the next page visit. A failed re-read is not a
      // failed save: the switch stays where it was and the report says it
      // could not be read.
      try {
        setHealth(await kernel.send<RemoteHealthReport>({ op: 'remote_health' }))
        setHealthFailed(false)
      } catch {
        setHealthFailed(true)
      }
    } catch (failure) {
      setError(failureMessage(failure, '没有记住远程 Agent 设置'))
    } finally {
      setBusy(null)
    }
  }

  async function checkDependencies() {
    setChecking(true)
    setBusy('check')
    setError(null)
    try {
      setHealth(await kernel.send<RemoteHealthReport>({ op: 'remote_health', repair: true }))
      setHealthFailed(false)
    } catch (failure) {
      setError(failureMessage(failure, '检查依赖完整性没有成功'))
    } finally {
      setChecking(false)
      setBusy(null)
    }
  }

  async function exportLogs() {
    setBusy('export')
    setError(null)
    setExported(null)
    try {
      const log = await kernel.send<{ path: string; text: string }>({ op: 'remote_logs' })
      const name = `agi-mizar-remote-logs-${timestamp()}.txt`
      if (shellAvailable()) {
        // The shell owns the save dialog; a cancelled dialog is not a failure
        // and must not claim a file was written.
        const path = await exportText(name, log.text)
        if (path !== null) setExported(path)
        return
      }
      // A browser page has no shell (the dev loop): the download attribute is
      // the only way to hand the file over, and the audit file's path is still
      // shown so a terminal can find it.
      const url = URL.createObjectURL(new Blob([log.text], { type: 'text/plain;charset=utf-8' }))
      const link = document.createElement('a')
      link.href = url
      link.download = name
      link.click()
      URL.revokeObjectURL(url)
      setExported(name)
    } catch (failure) {
      setError(failureMessage(failure, '导出日志没有成功'))
    } finally {
      setBusy(null)
    }
  }

  return (
    <div className="flex flex-col gap-4">
      <p className="text-[12px] leading-6 text-muted">
        本功能旨在为多设备间Agent任务指派提供支持。你可以在Agent中使用本功能指定的skill，然后使你的模型可以指派另一台电脑上的Agent模型作为子代理，从而实现多端的高效任务协同。
      </p>

      <section>
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
              <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
              依赖完整性
            </h2>
            <p className="mt-1 text-[12px] leading-5 text-muted">
              检查 agi-remote 工具、DSH 里的 MCP 条目与 skill、账户和两条线路；能修的部分会一并修好。
            </p>
          </div>
          <Button
            className="shrink-0"
            size="sm"
            variant="outline"
            onClick={() => void checkDependencies()}
            disabled={checking || busy !== null}
          >
            {checking ? <Spinner /> : null}
            {checking ? '正在检查…' : '检查依赖完整性'}
          </Button>
        </div>

        <div className="mt-2">
          <HealthReport report={health} failed={healthFailed} />
        </div>
      </section>

      {prefs === null ? (
        <PanelGroup title="被远程使用（这台电脑）">
          <p className="text-[12px] text-muted">{error === null ? '读取中…' : '没有读到这台电脑的设置'}</p>
        </PanelGroup>
      ) : (
        <PanelGroup title="被远程使用（这台电脑）">
          <div className="flex flex-col gap-3">
            <Row
              label="允许远程 Agent"
              hint="关闭时 19393 不监听，任何 peer token 都不会被签发"
              checked={prefs.enabled}
              onChange={(enabled) => void save({ ...prefs, enabled })}
            />
            <Row
              label="允许局域网"
              hint="同账户、同一局域网；不经过 relay，免费"
              checked={prefs.allowLan}
              onChange={(allowLan) => void save({ ...prefs, allowLan })}
            />
            <Row
              label="允许跨网（付费）"
              hint="通过账户 relay；需要付费账户"
              checked={prefs.allowWan}
              onChange={(allowWan) => void save({ ...prefs, allowWan })}
            />
            <Row
              label="对委派机开放所有工作区"
              hint="打开后不再限制工作区白名单，委派机可查询本机 DSH 的全部工作区"
              checked={prefs.allowAllWorkspaces}
              onChange={(allowAllWorkspaces) => void save({ ...prefs, allowAllWorkspaces })}
            />
            {prefs.allowAllWorkspaces ? (
              <p className="border-l-2 border-accent/50 pl-3 text-[11px] leading-5 text-muted">
                已开放全部工作区。委派机将直接看到本机 DSH 的现有工作区，不再使用下面的白名单。
              </p>
            ) : (
              <>
            <label className="flex flex-col gap-1.5">
              <span className="text-[12px] text-fg">可远程使用的工作区</span>
              <textarea
                className="min-h-24 w-full resize-y border border-border bg-transparent px-2 py-1.5 font-mono text-[12px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                value={workspaces}
                spellCheck={false}
                onChange={(event) => setWorkspaces(event.target.value)}
                placeholder={
                  desktopPlatform() === 'macos' ? '/Users/you/project' : 'C:\\Users\\you\\project'
                }
              />
              <span className="text-[11px] text-muted">
                一行一个绝对路径。远程 peer 只能在这些目录里创建会话。
              </span>
            </label>
              </>
            )}
            <label className="flex flex-col gap-1.5">
              <span className="text-[12px] text-fg">拒绝的设备</span>
              <textarea
                className="min-h-16 w-full resize-y border border-border bg-transparent px-2 py-1.5 font-mono text-[12px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                value={blocked}
                spellCheck={false}
                onChange={(event) => setBlocked(event.target.value)}
                placeholder="device-id"
              />
              <span className="text-[11px] text-muted">
                一行一个 deviceId。即使 token 有效，LAN/WAN 连接也会被拒绝。
              </span>
            </label>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                disabled={busy !== null}
                onClick={() =>
                  void save({
                    ...prefs,
                    workspaces: prefs.allowAllWorkspaces ? prefs.workspaces : lines(workspaces),
                    blocked: lines(blocked),
                  })
                }
              >
                {prefs.allowAllWorkspaces ? '保存拒绝列表' : '保存工作区与拒绝列表'}
              </Button>
              {saved ? <span className="text-[11px] text-muted">已保存</span> : null}
            </div>
          </div>
        </PanelGroup>
      )}

      <div className="mz-border flex items-center justify-between gap-4 px-4 py-2.5">
        <span className="text-[12px] leading-5 text-muted">
          导出远程 Agent 的全部日志：审计记录、委派记录和当时的依赖状态，不含 prompt 与回复正文。
        </span>
        <Button
          size="sm"
          variant="outline"
          className="shrink-0"
          onClick={() => void exportLogs()}
          disabled={busy !== null}
        >
          {busy === 'export' ? <Spinner /> : null}
          导出日志
        </Button>
      </div>

      {exported === null ? null : (
        <p className="selectable text-[11px] leading-5 text-muted">{exported}</p>
      )}

      {error === null ? null : (
        <p className="mz-border px-3 py-2 text-[11px] leading-5 text-fg">{error}</p>
      )}
    </div>
  )
}

/** The verdict and its items; the same drawing the agent page uses. */
function HealthReport({
  report,
  failed,
}: {
  report: RemoteHealthReport | null
  failed: boolean
}) {
  if (report === null) {
    return (
      <div className="mz-border px-4 py-3">
        <p className="text-[12px] text-muted">
          {failed ? '没有读到依赖状态，点上面的按钮重试。' : '正在读取依赖…'}
        </p>
      </div>
    )
  }
  return (
    <div className="mz-border">
      <div className="flex items-center gap-2 px-4 py-2.5">
        <span className={cn('size-2 shrink-0', report.ok ? 'bg-ink' : 'mz-border')} />
        <span className="text-[12px] text-fg">{report.summary}</span>
      </div>
      <div className="mz-divide-t flex flex-col gap-1.5 px-4 py-2.5">
        {report.items.map((item) => (
          <div key={item.id} className="flex items-start gap-2 text-[12px] leading-5">
            <span
              className={cn(
                'mt-[7px] size-2 shrink-0',
                item.status === 'ok'
                  ? 'bg-ink'
                  : item.status === 'outdated'
                    ? 'bg-ink dither-50'
                    : 'mz-border',
              )}
              aria-hidden
            />
            <span className="min-w-0">
              <span className="text-fg">{item.label}</span>
              <span className="text-muted"> · {item.detail}</span>
              {item.fix === null || item.status === 'ok' ? null : (
                <span className="text-muted">（会：{item.fix}）</span>
              )}
            </span>
          </div>
        ))}
      </div>
      {report.changed.length === 0 ? null : (
        <div className="mz-divide-t flex flex-col gap-1 bg-ink px-4 py-2 text-[12px] leading-5 text-bg">
          {report.changed.map((line) => (
            <p key={line}>{line}</p>
          ))}
        </div>
      )}
      {report.error === null ? null : (
        <p className="mz-divide-t bg-ink px-4 py-2 text-[12px] leading-5 text-bg">{report.error}</p>
      )}
    </div>
  )
}

function Row({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string
  hint: string
  checked: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <div className="text-[12px] text-fg">{label}</div>
        <div className="text-[11px] text-muted">{hint}</div>
      </div>
      <Switch checked={checked} onCheckedChange={onChange} label={label} />
    </div>
  )
}

/** One path per line, trimmed, as the kernel stores them. */
function lines(text: string): string[] {
  return text
    .split('\n')
    .map((line) => line.trim())
    .filter(Boolean)
}

/** `20261108-1842` for a filename that sorts by time. */
function timestamp(): string {
  const now = new Date()
  const pad = (value: number) => String(value).padStart(2, '0')
  return `${now.getFullYear()}${pad(now.getMonth() + 1)}${pad(now.getDate())}-${pad(now.getHours())}${pad(now.getMinutes())}`
}
