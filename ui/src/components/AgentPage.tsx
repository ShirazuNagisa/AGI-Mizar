import { useEffect, useRef, useState } from 'react'
import type {
  Balance,
  DependencyReport,
  DshPluginView,
  EngineId,
  EngineStatus,
  Kernel,
  Session,
  TurnRecord,
} from '../api'
import { ENGINE_INSTALL_HINTS } from '../engines'
import { sessionOnSurface, type AgentTab } from '../navigation'
import { formatAmount } from '../format'
import { Button } from './ui/button'
import { Toolbar } from './Toolbar'
import { DshPluginHint } from './DshPluginHint'
import { ENGINE_MARKS, Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'
import { isWindows } from '../platform'

interface AgentPageProps {
  tab: AgentTab
  status: EngineStatus | undefined
  kernel: Kernel
  /** The kernel's last word on the DSH plugin, shared with the update popup. */
  plugin: DshPluginView | null | undefined
  /** The rows the engine has found, including GUI-created sessions. */
  sessions: Session[]
  onReconnect: (engine: EngineId) => Promise<void>
}

/**
 * One agent's page: what this computer knows about it, and the one action that
 * is useful when it is not working. There are deliberately no technical numbers
 * here — the operator does not need a port to decide whether DSH is attached.
 *
 * Two states come before the content, because "not installed" and "installed but
 * never configured" are different problems with different fixes: one is a trip to
 * a terminal, the other is one click.
 */
export function AgentPage({ tab, status, kernel, plugin, sessions, onReconnect }: AgentPageProps) {
  const { engine, name, surface } = tab
  const connected = status?.connected ?? false
  const engaged = status?.engaged === true
  // A hosted engine with no CLI can still read transcripts; only a silent
  // engine is genuinely missing. `available` alone would hide that history.
  const installed = (status?.available ?? true) || connected
  const configured = status?.configured ?? true
  const [balance, setBalance] = useState<Balance | null>(null)
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<string | null>(null)
  // The unconfigured screen has no dependency card, but it is still the page
  // where a missing hook or plugin is discovered. One button here saves a trip
  // through the configured page after "一键配置" fails.
  const [depsBusy, setDepsBusy] = useState(false)
  const [depsNote, setDepsNote] = useState<string | null>(null)

  useEffect(() => {
    setBalance(null)
    if (engine !== 'dsh') return
    // Reading before the plugin attaches answers "this host has no account",
    // which is a different claim than "it is not connected yet". Wait for it.
    if (!connected) return
    let cancelled = false
    void kernel
      .send<Balance>({ op: 'balance', engine: 'dsh' })
      .then((value) => {
        if (!cancelled) setBalance(value)
      })
      .catch(() => {
        if (!cancelled) setBalance({ status: 'unavailable' })
      })
    return () => {
      cancelled = true
    }
  }, [kernel, engine, connected])

  async function run(label: string) {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'reconnect', engine })
      await onReconnect(engine)
      setNote(label)
    } catch (error) {
      setNote(failureMessage(error, '没有成功'))
    } finally {
      setBusy(false)
    }
  }

  /** Discovery again: for an agent installed after this window opened. */
  async function rescan() {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'engine_rescan' })
      await onReconnect(engine)
      setNote('已重新检测')
    } catch (error) {
      setNote(failureMessage(error, '重新检测没有成功'))
    } finally {
      setBusy(false)
    }
  }

  /** One click for an agent that is here but was never switched on. */
  async function configure() {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'engine_configure', engine, enabled: true })
      await onReconnect(engine)
      setNote('已配置')
    } catch (error) {
      setNote(failureMessage(error, '配置没有成功'))
    } finally {
      setBusy(false)
    }
  }

  /** Check and fix the connection dependencies without switching the agent on. */
  async function repairDependencies() {
    setDepsBusy(true)
    setDepsNote(null)
    try {
      const report = await kernel.send<DependencyReport>({
        op: 'dependencies_repair',
        engine,
      })
      const lines = [report.summary, ...report.changed]
      if (report.error !== null) lines.push(report.error)
      setDepsNote(lines.join(' · '))
    } catch (error) {
      setDepsNote(failureMessage(error, '检查依赖完整性没有成功'))
    } finally {
      setDepsBusy(false)
    }
  }

  const Mark = ENGINE_MARKS[engine]
  // The dependency report is a snapshot of files and a link, and both move
  // while this page is open. Re-read it when anything it describes changes,
  // or "waiting for the plugin" outlives the connection it was waiting for.
  const dependencyRevision = [
    connected,
    installed,
    configured,
    status?.detail ?? '',
    plugin?.installed ?? '',
    plugin?.latest ?? '',
  ].join('|')

  if (!installed || !configured) {
    return (
      <div className="flex h-full min-w-0 flex-1 flex-col">
        <Toolbar
          title={name}
          badge={<StatusLamp engaged={engaged} installed={installed} />}
        />
        <div className="flex flex-1 items-center justify-center overflow-y-auto px-8 py-10">
          <div className="w-full max-w-[440px] text-center">
            <Mark size={26} className="mx-auto text-muted" />
            {installed ? (
              <>
                <h2 className="mt-4 text-[12px] font-semibold text-fg">尚未配置该 Agent</h2>
                <p className="mt-2 text-[12px] leading-5 text-muted">
                  {name} 已经装在这台电脑上，但还没有在这里启用。启用后它的会话才会出现在左侧。
                </p>
                <div className="mt-5 flex items-center justify-center gap-2">
                  <Button
                    variant="primary"
                    onClick={() => void configure()}
                    disabled={busy}
                  >
                    {busy ? <Spinner /> : null}
                    一键配置
                  </Button>
                  <Button
                    variant="outline"
                    onClick={() => void repairDependencies()}
                    disabled={depsBusy}
                  >
                    {depsBusy ? <Spinner /> : null}
                    检查依赖完整性
                  </Button>
                </div>
                {depsNote === null ? null : (
                  <p className="mt-3 text-[12px] leading-5 text-muted">{depsNote}</p>
                )}
                {engine === 'dsh' ? (
                  <DshPluginHint className="mt-4 text-left" kernel={kernel} />
                ) : null}
              </>
            ) : (
              <>
                <h2 className="mt-4 text-[12px] font-semibold text-fg">本机未检测到该 Agent</h2>
                <p className="mt-2 text-[12px] leading-5 text-muted">
                  {ENGINE_INSTALL_HINTS[engine]}
                </p>
                <p className="mt-1 text-[12px] leading-5 text-muted">
                  装好后回到这里点「重新检测」，不用重启应用。
                </p>
                <Button
                  className="mt-5"
                  variant="primary"
                  onClick={() => void rescan()}
                  disabled={busy}
                >
                  {busy ? <Spinner /> : null}
                  重新检测
                </Button>
              </>
            )}
            {note === null ? null : (
              <p className="mt-3 text-[12px] leading-5 text-muted">{note}</p>
            )}
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title={name} badge={<StatusLamp engaged={engaged} installed={installed} />}>
        {engine === 'dsh' ? (
          <Button size="sm" onClick={() => void run('已重新检查连接')} disabled={busy}>
            {busy ? <Spinner /> : null}
            重新连接
          </Button>
        ) : (
          <Button size="sm" onClick={() => void rescan()} disabled={busy}>
            {busy ? <Spinner /> : null}
            重新检测
          </Button>
        )}
      </Toolbar>

      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
          {note === null ? null : <p className="mb-4 text-[12px] text-muted">{note}</p>}

          <DependencySection
            kernel={kernel}
            engine={engine}
            revision={dependencyRevision}
          />

          <RecentSessions tab={tab} sessions={sessions} kernel={kernel} />

          {engine === 'dsh' ? (
            <PluginSection kernel={kernel} plugin={plugin ?? null} />
          ) : null}

          {engine === 'dsh' ? (
            <section>
              <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
                <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
                账户余额
              </h2>
              <div className="mt-2 mz-border px-4 py-3">
                <BalanceRows balance={balance} connected={connected} />
              </div>
            </section>
          ) : (
            <section>
              <p className="text-[12px] leading-6 text-muted">
                {surface === 'gui'
                  ? guiSurfaceCopy(name)
                  : `${name} 在本机以命令行工具运行，模型与权限由它自己的配置文件管理。上方的「连接依赖」会检查它是否仍可用；装好或修好后可以点「重新检测」。`}
              </p>
            </section>
          )}
        </div>
      </div>
    </div>
  )
}

/**
 * What this window can do with the sessions the desktop app wrote.
 *
 * macOS and Linux drive Codex's desktop app over its private Unix socket, so a
 * GUI transcript there can be continued from this window. Windows has no such
 * channel (the control side is compiled only for Unix), so promising "continue
 * here" would be a lie the first click exposes; say what is true instead.
 */
function guiSurfaceCopy(name: string): string {
  if (isWindows()) {
    return `${name} 的会话来自桌面应用。Windows 上这些会话在这里只读：桌面控制通道只在 macOS 与 Linux 提供，要继续请在命令行面操作。模型与权限由那个桌面应用自己的设置管理。`
  }
  return `${name} 的会话来自桌面应用；Claude GUI 只读，Codex GUI 可以在最近会话里继续。模型与权限由桌面应用自己的设置管理。`
}

/**
 * The last few rows this engine found, with the surface that created them.
 *
 * The desktop window had no session list at all: the phone knew what happened
 * while the window only showed whether the engine was installed. A GUI-created
 * transcript is still a session, so show a short, read-only slice here and say
 * "Claude 桌面" / "Codex 桌面" when the source is known.
 */
function RecentSessions({
  tab,
  sessions,
  kernel,
}: {
  tab: AgentTab
  sessions: Session[]
  kernel: Kernel
}) {
  const { engine } = tab
  const rows = sessions
    .filter((session) => sessionOnSurface(session, tab) && session.origin === null)
    .sort((left, right) => right.updated_at - left.updated_at)
    .slice(0, 8)
  const [open, setOpen] = useState<string | null>(null)
  const [draft, setDraft] = useState('')
  const [sending, setSending] = useState(false)
  const [forking, setForking] = useState<string | null>(null)
  const [forkNote, setForkNote] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  if (rows.length === 0) return null

  async function send(session: Session) {
    const text = draft.trim()
    if (text === '' || sending) return
    setSending(true)
    setError(null)
    try {
      await kernel.send({
        op: session.running ? 'steer' : 'prompt',
        session: session.key,
        text,
      })
      setDraft('')
      setOpen(null)
    } catch (reason) {
      setError(failureMessage(reason, '消息没有发出去'))
    } finally {
      setSending(false)
    }
  }

  async function fork(session: Session) {
    if (forking !== null) return
    setForking(session.key)
    setForkNote(null)
    setError(null)
    try {
      await kernel.send({ op: 'fork', session: session.key, at_seq: 0 })
      setForkNote('已创建分叉')
    } catch (reason) {
      setError(failureMessage(reason, '分叉失败'))
    } finally {
      setForking(null)
    }
  }

  return (
    <section className="mb-6">
      <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
        <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
        最近会话
        {tab.surface === null ? null : (
          <span className="border border-fg/30 px-1.5 py-0.5 text-[9px] font-normal tracking-normal text-muted">
            {tab.surface === 'gui' ? 'GUI' : 'CLI'}
          </span>
        )}
      </h2>
      <div className="mt-2 mz-border">
        {rows.map((session) => {
          const source =
            session.source === 'claude_desktop'
              ? 'Claude 桌面'
              : session.source === 'codex_desktop'
                ? 'Codex 桌面'
                : null
          const readOnly = session.source === 'claude_desktop'
          const cwd = session.cwd?.split('/').filter(Boolean).at(-1) ?? null
          return (
            <div key={session.key} className="mz-divide-t px-4 py-2.5 text-[12px] leading-5">
              <div className="flex items-start gap-2">
                <span
                  className={cn(
                    'mt-[7px] size-2 shrink-0',
                    session.running ? 'bg-fg' : 'border border-fg/50',
                  )}
                  aria-hidden
                />
                <div className="min-w-0 flex-1">
                  <div className="flex items-center gap-2">
                    <span className="min-w-0 truncate text-fg">
                      {session.title?.trim() || '未命名会话'}
                    </span>
                    {source === null ? null : (
                      <span className="shrink-0 border border-fg/30 px-1.5 py-0.5 text-[10px] leading-none text-muted">
                        {source}
                      </span>
                    )}
                    {readOnly ? (
                      <span className="shrink-0 text-[10px] text-muted">只读</span>
                    ) : (
                      <>
                        {engine === 'codex' ? (
                          <button
                            type="button"
                            className="ml-auto shrink-0 text-[10px] text-muted underline-offset-2 hover:text-fg hover:underline"
                            disabled={forking !== null}
                            onClick={() => void fork(session)}
                          >
                            {forking === session.key ? '分叉中' : '分叉'}
                          </button>
                        ) : null}
                        <button
                          type="button"
                          className={cn(
                            'shrink-0 text-[10px] text-muted underline-offset-2 hover:text-fg hover:underline',
                            engine === 'codex' ? '' : 'ml-auto',
                          )}
                          onClick={() => {
                            setOpen(open === session.key ? null : session.key)
                            setDraft('')
                            setError(null)
                          }}
                        >
                          发消息
                        </button>
                      </>
                    )}
                  </div>
                  <div className="mt-0.5 flex min-w-0 items-center gap-2 text-[10px] text-muted">
                    {cwd === null ? null : <span className="truncate">{cwd}</span>}
                    {session.model === null ? null : (
                      <span className="truncate">{session.model}</span>
                    )}
                    <span className="ml-auto shrink-0">{relative(session.updated_at)}</span>
                  </div>
                </div>
              </div>
              {open === session.key ? (
                <SessionMessages kernel={kernel} session={session.key} />
              ) : null}
              {open === session.key ? (
                <div className="mt-2 flex items-center gap-2">
                  <input
                    autoFocus
                    value={draft}
                    onChange={(event) => setDraft(event.target.value)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') void send(session)
                      if (event.key === 'Escape') setOpen(null)
                    }}
                    placeholder={session.running ? '追加消息…' : '发消息…'}
                    className="min-w-0 flex-1 border border-fg/20 bg-transparent px-2 py-1 text-[12px] text-fg outline-none focus:border-fg/50"
                  />
                  <Button
                    type="button"
                    size="sm"
                    disabled={sending || draft.trim() === ''}
                    onClick={() => void send(session)}
                  >
                    {sending ? '发送中' : '发送'}
                  </Button>
                </div>
              ) : null}
              {forkNote !== null && forking === null ? (
                <p className="mt-1 text-[10px] leading-5 text-muted">{forkNote}</p>
              ) : null}
              {error !== null && open === session.key ? (
                <p className="mt-1 text-[10px] leading-5 text-muted">{error}</p>
              ) : null}
            </div>
          )
        })}
      </div>
    </section>
  )
}

function SessionMessages({ kernel, session }: { kernel: Kernel; session: string }) {
  const [rows, setRows] = useState<TurnRecord[] | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    let timer: number | undefined

    async function load() {
      try {
        const next = await kernel.send<TurnRecord[]>({
          op: 'messages',
          session,
          limit: 50,
        })
        if (!cancelled) {
          setRows(next)
          setError(null)
        }
      } catch (reason) {
        if (!cancelled) setError(failureMessage(reason, '读不到消息'))
      }
      if (!cancelled) {
        timer = window.setTimeout(() => void load(), 1500)
      }
    }

    setRows(null)
    void load()
    return () => {
      cancelled = true
      if (timer !== undefined) window.clearTimeout(timer)
    }
  }, [kernel, session])

  useEffect(() => {
    return kernel.subscribe((event) => {
      if (event.type !== 'record_appended' || event.session !== session) return
      setRows((current) => {
        const next = (current ?? []).filter((row) => row.seq !== event.record.seq)
        next.push(event.record)
        next.sort((left, right) => left.seq - right.seq)
        return next.slice(-50)
      })
    }, () => {})
  }, [kernel, session])

  if (error !== null) {
    return <p className="mt-2 text-[10px] leading-5 text-muted">{error}</p>
  }
  if (rows === null) {
    return <p className="mt-2 text-[10px] leading-5 text-muted">读取中…</p>
  }
  const visible = rows
    .filter((row) => row.text !== null && row.text.trim() !== '')
    .slice(-20)
  if (visible.length === 0) {
    return <p className="mt-2 text-[10px] leading-5 text-muted">还没有可显示的消息。</p>
  }
  return (
    <div className="mt-2 max-h-56 overflow-y-auto border border-fg/10 px-2 py-1.5">
      {visible.map((row) => (
        <div key={row.seq} className="py-1 text-[11px] leading-5">
          <span className="mr-2 shrink-0 text-[9px] uppercase tracking-[1px] text-muted">
            {row.kind === 'user_message'
              ? '你'
              : row.kind === 'assistant_message'
                ? 'Agent'
                : '记录'}
          </span>
          <span className="whitespace-pre-wrap text-fg/90">{row.text}</span>
        </div>
      ))}
    </div>
  )
}

function relative(at: number): string {
  if (!Number.isFinite(at) || at <= 0) return ''
  const minutes = Math.max(0, Math.floor((Date.now() - at) / 60_000))
  if (minutes < 1) return '刚刚'
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  const days = Math.floor(hours / 24)
  if (days < 30) return `${days} 天前`
  return new Date(at).toLocaleDateString('zh-CN')
}

/**
 * The connection dependencies, and the one button that can fix them.
 *
 * The button does not stop at a diagnosis: it runs the same check, repairs what
 * this app knows how to repair (hooks, a plugin install, a reconnect) and then
 * re-reads, so a person who pressed it once sees either "完整" or the sentence
 * explaining why it is not.
 */
function DependencySection({
  kernel,
  engine,
  revision,
}: {
  kernel: Kernel
  engine: EngineId
  /** Changes when the link, the plugin, or the CLI moved. */
  revision: string
}) {
  const [report, setReport] = useState<DependencyReport | null>(null)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // A repair is itself the freshest read; letting the revision effect fire
  // while it runs would overwrite `changed` with a second, note-less report.
  const repairing = useRef(false)

  useEffect(() => {
    if (repairing.current) return
    setReport(null)
    setError(null)
    let cancelled = false
    void kernel
      .send<DependencyReport>({ op: 'dependencies', engine })
      .then((answer) => {
        if (!cancelled) setReport(answer)
      })
      .catch((failure) => {
        if (!cancelled) setError(failureMessage(failure, '依赖检查失败'))
      })
    return () => {
      cancelled = true
    }
  }, [kernel, engine, revision])

  async function checkAndRepair() {
    repairing.current = true
    setBusy(true)
    setError(null)
    try {
      setReport(await kernel.send<DependencyReport>({ op: 'dependencies_repair', engine }))
    } catch (failure) {
      setError(failureMessage(failure, '检查依赖完整性没有成功'))
    } finally {
      repairing.current = false
      setBusy(false)
    }
  }

  return (
    <section className="mb-6">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
            <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
            连接依赖
          </h2>
          <p className="mt-1 text-[12px] leading-5 text-muted">
            一键检查缺失或过旧的依赖，并自动安装、更新或重新连接。
          </p>
        </div>
        <Button
          className="shrink-0"
          size="sm"
          variant="outline"
          onClick={() => void checkAndRepair()}
          disabled={busy}
        >
          {busy ? <Spinner /> : null}
          {busy ? '正在检查…' : '检查依赖完整性'}
        </Button>
      </div>

      <div className="mt-2 mz-border">
        {report === null ? (
          <p className="px-4 py-3 text-[12px] text-muted">
            {error ?? '正在读取依赖…'}
          </p>
        ) : (
          <>
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
              <p className="mz-divide-t bg-ink px-4 py-2 text-[12px] leading-5 text-bg">
                {report.error}
              </p>
            )}
          </>
        )}
        {report !== null && error !== null ? (
          <p className="mz-divide-t bg-ink px-4 py-2 text-[12px] leading-5 text-bg">{error}</p>
        ) : null}
      </div>
    </section>
  )
}

/**
 * The plugin's own version check, kept separate from the dependency list because
 * it is the one thing here that is also an update: a person can come back after
 * a release and ask npm without repairing anything else.
 */
function PluginSection({ kernel, plugin }: { kernel: Kernel; plugin: DshPluginView | null }) {
  const [view, setView] = useState<DshPluginView | null>(plugin)
  const [busy, setBusy] = useState<'check' | 'update' | null>(null)
  const [note, setNote] = useState<string | null>(null)

  useEffect(() => {
    setView(plugin)
  }, [plugin])

  async function check() {
    setBusy('check')
    setNote(null)
    try {
      setView(await kernel.send<DshPluginView>({ op: 'dsh_plugin_check' }))
    } catch (failure) {
      setNote(failureMessage(failure, '检查插件更新失败'))
    } finally {
      setBusy(null)
    }
  }

  async function update() {
    const installing = view?.installed === null
    setBusy('update')
    setNote(null)
    try {
      const next = await kernel.send<DshPluginView>({ op: 'dsh_plugin_update' })
      setView(next)
      setNote(
        `已${installing ? '安装' : '更新'}到 ${next.installed ?? next.latest ?? '最新版本'}，重启 DeepSeek Harness 后生效`,
      )
    } catch (failure) {
      setNote(failureMessage(failure, '更新插件没有成功'))
    } finally {
      setBusy(null)
    }
  }

  const line = pluginLine(view)
  const canUpdate = view?.updateAvailable === true

  return (
    <section className="mb-6">
      <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
        <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
        插件更新
      </h2>
      <div className="mt-2 mz-border">
        <div className="flex items-start justify-between gap-4 px-4 py-3">
          <div className="min-w-0">
            <div className="text-[12px] text-fg">dsh-mizar</div>
            <p className="mt-0.5 text-[12px] leading-5 text-muted">{line}</p>
            {note === null ? null : (
              <p className="mt-1 text-[12px] leading-5 text-fg">{note}</p>
            )}
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {canUpdate ? (
              <Button
                size="sm"
                variant="primary"
                onClick={() => void update()}
                disabled={busy !== null}
              >
                {busy === 'update' ? <Spinner /> : null}
                {view?.installed === null ? '安装' : '更新到'} {view?.latest}
              </Button>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              onClick={() => void check()}
              disabled={busy !== null}
            >
              {busy === 'check' ? <Spinner /> : null}
              检查插件更新
            </Button>
          </div>
        </div>
      </div>
    </section>
  )
}

/** The one sentence under "dsh-mizar". */
function pluginLine(view: DshPluginView | null): string {
  if (view === null) return '正在读取版本…'
  const installed = view.installed === null ? '本机未安装' : `本机 ${view.installed}`
  const latest =
    view.latest === null
      ? view.error === null
        ? '还没有检查 npm'
        : `无法检查 npm（${view.error}）`
      : `npm 最新 ${view.latest}`
  const suffix = view.cli ? '' : ' · 未找到 dsh 命令行，无法一键安装'
  return `${installed} · ${latest}${suffix}`
}

function StatusLamp({ engaged, installed }: { engaged: boolean; installed: boolean }) {
  return (
    <span className="flex items-center gap-1.5 text-[12px] text-muted">
      <span
        className={cn(
          'size-2',
          engaged ? 'bg-fg' : installed ? 'border border-fg' : 'border border-fg/40',
        )}
      />
      {engaged ? '已连接' : installed ? '未连接' : '未安装'}
    </span>
  )
}

function BalanceRows({ balance, connected }: { balance: Balance | null; connected: boolean }) {
  if (!connected) {
    return <p className="text-[12px] text-muted">连接后读取</p>
  }
  if (balance === null) {
    return <p className="text-[12px] text-muted">正在读取…</p>
  }
  switch (balance.status) {
    case 'ready':
      return (
        <div className="flex flex-col gap-2">
          {balance.wallets.length === 0 ? (
            <p className="text-[12px] text-muted">余额为空</p>
          ) : (
            balance.wallets.map((wallet) => (
              <div key={wallet.currency} className="flex items-baseline justify-between">
                <span className="text-[12px] text-muted">{wallet.currency}</span>
                <span className="text-[24px] font-semibold text-fg tabular">
                  {formatAmount(wallet.amount)}
                </span>
              </div>
            ))
          )}
        </div>
      )
    case 'signed_out':
      return <p className="text-[12px] text-muted">需要先在 DSH 中登录账户</p>
    case 'unsupported':
      return <p className="text-[12px] text-muted">DSH 没有提供余额</p>
    default:
      return <p className="text-[12px] text-muted">余额暂时读不到</p>
  }
}
