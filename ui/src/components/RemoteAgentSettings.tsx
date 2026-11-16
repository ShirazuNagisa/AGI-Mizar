/**
 * The Agent-interconnect settings surface.
 *
 * Two directions on one page: what this computer does with the subagents it
 * delegates (auto-archive, the dependency check) and what other computers may do
 * with it (who may use it, in which folders). The delegation list, the LAN
 * certificate and the audit tail live in the agent surfaces and the log export;
 * repeating them here made the page a dashboard instead of a setting.
 */
import { useEffect, useState } from 'react'
import type {
  Kernel,
  PairingView,
  RemoteHealthReport,
  RemotePeerRow,
  RemotePeerView,
  RemotePrefs,
  SelfHostDesktopsResult,
} from '../api'
import { Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'
import { desktopPlatform } from '../platform'
import { exportText, shellAvailable } from '../shell'
import { Button } from './ui/button'
import { PanelGroup } from './ui/panel'
import { Switch } from './ui/switch'

export function RemoteAgentSettings({
  kernel,
  selfHosted,
}: {
  kernel: Kernel
  /** Which server this install talks to; the pairing surface is self-hosted only. */
  selfHosted: boolean
}) {
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
        if (!cancelled) setError(failureMessage(failure, '读取 Agent 互联设置失败'))
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
        auto_archive: next.autoArchive,
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
      setError(failureMessage(failure, '没有记住 Agent 互联设置'))
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
        Agent 互联让一个 Agent 把任务交给 DSH 子代理执行：可以在本机，也可以在另一台同账户的电脑上。
        在 Agent 中使用本功能提供的 skill（agi-mizar-remote）即可，支持 DSH、Claude Code 与 Codex。
        参考教程：
        <button
          type="button"
          onClick={() =>
            void kernel.send({
              op: 'open_external',
              url: 'https://dsh-mizar.shirazu-nagisa.com/docs/',
            })
          }
          className="cursor-pointer break-all text-accent underline underline-offset-2 hover:opacity-80"
        >
          https://dsh-mizar.shirazu-nagisa.com/docs/
        </button>
      </p>

      <section>
        <div className="flex items-start justify-between gap-4">
          <div className="min-w-0">
            <h2 className="flex items-center gap-2 text-[12px] tracking-[1px]">
              <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
              依赖完整性
            </h2>
            <p className="mt-1 text-[12px] leading-5 text-muted">
              检查 agi-remote 工具、各 Agent（DSH / Claude Code / Codex）里的 MCP 条目与 skill、账户和两条线路；能修的部分会一并修好。
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

      <PanelGroup title="派出的子代理">
        <div className="flex flex-col gap-3">
          {prefs === null ? (
            <p className="text-[12px] text-muted">读取中…</p>
          ) : (
            <>
              <Row
                label="任务结束后自动归档子代理会话"
                hint="本机与远程委派都适用：任务完成或失败后把该子代理会话归档，不再堆在 DSH 会话列表里；被取消的任务保留。默认开启。"
                checked={prefs.autoArchive}
                onChange={(autoArchive) => void save({ ...prefs, autoArchive })}
              />
              <p className="text-[11px] leading-5 text-muted">
                开启时每个子代理任务是一次性的：任务结束即归档，之后不能再向该会话追加；需要多轮追加就把这里关掉。
                归档只影响 DSH 的会话列表，不删除内容。
              </p>
            </>
          )}
        </div>
      </PanelGroup>

      {selfHosted ? <SelfHostedPairing kernel={kernel} /> : null}

      {prefs === null ? (
        <PanelGroup title="被远程使用（这台电脑）">
          <p className="text-[12px] text-muted">{error === null ? '读取中…' : '没有读到这台电脑的设置'}</p>
        </PanelGroup>
      ) : (
        <PanelGroup title="被远程使用（这台电脑）">
          <div className="flex flex-col gap-3">
            <Row
              label="允许其他电脑把任务交给本机"
              hint="关闭时 19393 不监听，任何 peer token 都不会被签发"
              checked={prefs.enabled}
              onChange={(enabled) => void save({ ...prefs, enabled })}
            />
            <Row
              label="允许局域网直连"
              hint={
                selfHosted
                  ? '自建服务器暂不支持局域网直连；配对与委派统一经过你自己的服务器'
                  : '同账户、同一局域网；不经过 relay，免费'
              }
              checked={prefs.allowLan}
              disabled={selfHosted}
              onChange={(allowLan) => void save({ ...prefs, allowLan })}
            />
            <Row
              label={selfHosted ? '允许跨网（自建服务器）' : '允许跨网（付费）'}
              hint={
                selfHosted
                  ? '通过你自己的 relay 转发；不需要账户，也不需要付费'
                  : '通过账户 relay；需要付费账户'
              }
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
          导出 Agent 互联的全部日志：审计记录、委派记录和当时的依赖状态，不含 prompt 与回复正文。
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
  disabled,
  onChange,
}: {
  label: string
  hint: string
  checked: boolean
  disabled?: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <div className="text-[12px] text-fg">{label}</div>
        <div className="text-[11px] text-muted">{hint}</div>
      </div>
      <Switch
        checked={checked}
        disabled={disabled}
        onCheckedChange={onChange}
        label={label}
      />
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

/** What the paired list needs; the token never reaches this file. */
interface PairedPeer {
  id: string
  name: string
  origin: string
}

/**
 * Pairing two computers on the operator's own relay.
 *
 * Both halves live in one card because they are one conversation: this computer
 * shows a code, the other one lists this computer and asks for it. A
 * self-hosted relay has no account to discover with, so the relay's directory
 * plus this code is the whole trust ceremony.
 */
function SelfHostedPairing({ kernel }: { kernel: Kernel }) {
  const [pairing, setPairing] = useState<PairingView | null>(null)
  const [pairingError, setPairingError] = useState<string | null>(null)
  const [codeBusy, setCodeBusy] = useState(false)

  const [directory, setDirectory] = useState<SelfHostDesktopsResult | null>(null)
  const [directoryBusy, setDirectoryBusy] = useState(false)

  const [peers, setPeers] = useState<PairedPeer[] | null>(null)
  const [peersError, setPeersError] = useState<string | null>(null)

  const [selectedRoom, setSelectedRoom] = useState('')
  const [code, setCode] = useState('')
  const [pairingBusy, setPairingBusy] = useState(false)
  const [removing, setRemoving] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    // The code is reused while it lives, so opening the page must not
    // invalidate one somebody is reading off the screen.
    void (async () => {
      try {
        const next = await kernel.send<PairingView>({ op: 'pairing', refresh: false })
        if (!cancelled) setPairing(next)
      } catch (failure) {
        if (!cancelled) setPairingError(failureMessage(failure, '没有拿到这台电脑的配对码'))
      }
    })()
    return () => {
      cancelled = true
    }
  }, [kernel])

  async function loadPeers() {
    setPeersError(null)
    try {
      const answer = await kernel.send<{
        thisDeviceId: string | null
        peers: RemotePeerRow[]
      }>({ op: 'remote_peers' })
      // Self-hosted rows carry the id twice (`id` and `deviceId`); the
      // window's id is the host-issued one.
      setPeers(
        answer.peers.map((row) => ({
          id: row.id ?? row.deviceId,
          name: row.name,
          origin: row.origin ?? '',
        })),
      )
    } catch (failure) {
      setPeersError(failureMessage(failure, '没有读到已配对的电脑'))
      setPeers([])
    }
  }

  async function loadDirectory() {
    setDirectoryBusy(true)
    setError(null)
    try {
      const answer = await kernel.send<SelfHostDesktopsResult>({ op: 'selfhost_desktops' })
      setDirectory(answer)
      // A room that disappeared must not stay selected: pairing would 404.
      if (
        selectedRoom !== '' &&
        !answer.desktops.some((desktop) => desktop.room === selectedRoom)
      ) {
        setSelectedRoom('')
      }
    } catch (failure) {
      setDirectory({ desktops: [], error: { code: 'NETWORK', message: failureMessage(failure, '') } })
    } finally {
      setDirectoryBusy(false)
    }
  }

  useEffect(() => {
    void loadPeers()
    void loadDirectory()
    // Both are reads of the server this page is about; one run per mount is
    // enough and the buttons refresh them.
  }, [kernel])

  async function refreshCode() {
    setCodeBusy(true)
    setPairingError(null)
    try {
      setPairing(await kernel.send<PairingView>({ op: 'pairing', refresh: true }))
    } catch (failure) {
      setPairingError(failureMessage(failure, '没有换到新的配对码'))
    } finally {
      setCodeBusy(false)
    }
  }

  async function pair() {
    if (selectedRoom === '' || code.trim() === '') return
    setPairingBusy(true)
    setError(null)
    setNotice(null)
    try {
      const answer = await kernel.send<{ peer: RemotePeerView }>({
        op: 'peer_pair',
        room: selectedRoom,
        code: code.trim(),
      })
      setCode('')
      setNotice(`已与「${answer.peer.name}」配对。现在可以让 Agent 把任务交给它。`)
      await loadPeers()
    } catch (failure) {
      setError(pairFailureCopy(failure))
    } finally {
      setPairingBusy(false)
    }
  }

  async function forget(peer: PairedPeer) {
    setRemoving(peer.id)
    setError(null)
    setNotice(null)
    try {
      await kernel.send<{ removed: boolean }>({ op: 'peer_forget', id: peer.id })
      setNotice(`已从本机移除「${peer.name}」。对方电脑上的令牌仍然有效。`)
      await loadPeers()
    } catch (failure) {
      setError(failureMessage(failure, '没有移除这台电脑'))
    } finally {
      setRemoving(null)
    }
  }

  const chosen = directory?.desktops.find((desktop) => desktop.room === selectedRoom) ?? null

  return (
    <>
      <PanelGroup title="本机的配对码">
        <div className="flex flex-col gap-2">
          <p className="text-[11px] leading-5 text-muted">
            在另一台电脑上打开「设置 → Agent 互联 → 配对另一台电脑」，选中这台电脑，再输入下面的码。
            码只能用一次。
          </p>
          <div className="flex flex-wrap items-center gap-3">
            <span className="selectable font-mono text-[22px] leading-8 tracking-[3px] text-fg">
              {pairing?.code ?? '—'}
            </span>
            <Button size="sm" variant="outline" onClick={() => void refreshCode()} disabled={codeBusy}>
              {codeBusy ? <Spinner /> : null}
              {codeBusy ? '正在生成…' : '换一个'}
            </Button>
          </div>
          {pairingError === null ? null : (
            <p className="text-[11px] leading-5 text-muted">{pairingError}</p>
          )}
          <p className="text-[11px] leading-5 text-muted">
            {pairing === null
              ? '等待这台电脑的远程服务启动…'
              : pairing.uri.includes('u=')
                ? '配对码里已带自建服务器地址，局域网或跨网都能用。'
                : '自建服务器还没连上：对方需要能连到这台电脑所在的服务器，连上后二维码才会带服务器地址。'}
          </p>
        </div>
      </PanelGroup>

      <PanelGroup title="配对另一台电脑">
        <div className="flex flex-col gap-3">
          <div className="flex items-start justify-between gap-3">
            <div className="min-w-0">
              <div className="text-[12px] text-fg">服务器上在线的电脑</div>
              <div className="text-[11px] text-muted">
                只显示与当前自建服务器配对的电脑；列表来自服务器的在线目录。
              </div>
            </div>
            <Button
              size="sm"
              variant="outline"
              className="shrink-0"
              onClick={() => void loadDirectory()}
              disabled={directoryBusy || pairingBusy}
            >
              {directoryBusy ? <Spinner /> : null}
              {directoryBusy ? '正在读取…' : '刷新列表'}
            </Button>
          </div>

          {directory === null ? (
            <p className="text-[12px] text-muted">正在读取服务器上的电脑…</p>
          ) : directory.error != null ? (
            <p className="mz-border px-3 py-2 text-[12px] leading-5 text-fg">
              {directoryErrorCopy(directory.error.code)}
            </p>
          ) : directory.desktops.length === 0 ? (
            <p className="text-[12px] leading-5 text-muted">
              服务器上没有在线的电脑。确认另一台电脑已经连上同一台自建服务器，再刷新。
            </p>
          ) : (
            <div className="flex flex-col gap-1.5">
              {directory.desktops.map((desktop) => {
                const active = desktop.room === selectedRoom
                return (
                  <button
                    key={desktop.room}
                    type="button"
                    onClick={() => setSelectedRoom(desktop.room)}
                    className={cn(
                      'flex items-center justify-between gap-3 border px-3 py-2 text-left text-[12px]',
                      active ? 'border-ink bg-ink text-bg' : 'border-border text-fg hover:opacity-80',
                    )}
                  >
                    <span className="min-w-0 truncate">{desktop.name}</span>
                    <span className="shrink-0 text-[11px]">
                      {platformName(desktop.platform)}
                      {active ? ' · 已选择' : ''}
                    </span>
                  </button>
                )
              })}
            </div>
          )}

          <div className="flex flex-wrap items-end gap-2">
            <label className="flex flex-col gap-1">
              <span className="text-[11px] text-muted">
                {chosen === null ? '先在列表里选一台电脑' : `${chosen.name} 上显示的配对码`}
              </span>
              <input
                className="h-7 w-44 border border-border bg-transparent px-2 font-mono text-[12px] tracking-[1px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                value={code}
                spellCheck={false}
                autoComplete="off"
                placeholder="K7QP-3M2X"
                onChange={(event) => setCode(event.target.value.toUpperCase())}
              />
            </label>
            <Button
              size="sm"
              onClick={() => void pair()}
              disabled={pairingBusy || chosen === null || code.trim() === ''}
            >
              {pairingBusy ? <Spinner /> : null}
              {pairingBusy ? '正在配对…' : '配对'}
            </Button>
          </div>

          {notice === null ? null : (
            <p className="text-[11px] leading-5 text-muted">{notice}</p>
          )}
          {error === null ? null : (
            <p className="mz-border px-3 py-2 text-[11px] leading-5 text-fg">{error}</p>
          )}
        </div>
      </PanelGroup>

      <PanelGroup title="已配对的电脑">
        <div className="flex flex-col gap-2">
          {peersError !== null ? (
            <p className="text-[12px] leading-5 text-muted">{peersError}</p>
          ) : peers === null ? (
            <p className="text-[12px] text-muted">正在读取…</p>
          ) : peers.length === 0 ? (
            <p className="text-[12px] leading-5 text-muted">
              还没有配对。配对之后，这里的电脑就可以通过 Agent 互联接受任务。
            </p>
          ) : (
            peers.map((peer) => (
              <div
                key={peer.id}
                className="flex items-center justify-between gap-3 border border-border px-3 py-2"
              >
                <div className="min-w-0">
                  <div className="truncate text-[12px] text-fg">{peer.name}</div>
                  <div className="truncate text-[11px] text-muted">{peer.origin}</div>
                </div>
                <Button
                  size="sm"
                  variant="outline"
                  className="shrink-0"
                  onClick={() => void forget(peer)}
                  disabled={removing !== null}
                >
                  {removing === peer.id ? <Spinner /> : null}
                  移除
                </Button>
              </div>
            ))
          )}
          <p className="text-[11px] leading-5 text-muted">
            移除只删除本机的配对记录，不会撤销对方电脑上已经发出的令牌；要彻底撤销，请在对方电脑的「已被远程使用」设备列表里移除这台电脑。
          </p>
        </div>
      </PanelGroup>
    </>
  )
}

/** A server directory's platform, in the words this page uses. */
function platformName(platform: string): string {
  switch (platform) {
    case 'macos':
      return 'macOS'
    case 'windows':
      return 'Windows'
    case 'linux':
      return 'Linux'
    default:
      return '未知平台'
  }
}

/** Why the online list could not be read, said so it can be acted on. */
function directoryErrorCopy(code: string): string {
  switch (code) {
    case 'NO_SERVER':
      return '还没有填写自建服务器地址，先到「服务器」页设置。'
    case 'PIN_REQUIRED':
      return '这台服务器要求 PIN，先到「服务器」页填写。'
    case 'PIN_INVALID':
      return '服务器 PIN 不正确，到「服务器」页修改后重试。'
    case 'RATE_LIMITED':
      return 'PIN 错误次数过多，等 15 分钟再试。'
    case 'UNREACHABLE':
    case 'NETWORK':
      return '连不上自建服务器，检查网络、域名和证书。'
    case 'BAD_ANSWER':
      return '服务器返回的内容看不懂，确认这个地址确实是 DSH Mizar relay。'
    default:
      return '读取服务器上的电脑失败。'
  }
}

/** Why a pairing was refused, with the codes the host uses. */
function pairFailureCopy(failure: unknown): string {
  const text = failure instanceof Error ? failure.message : ''
  switch (text.split(':')[0]?.trim() ?? '') {
    case 'PAIRING_CODE_UNKNOWN':
      return '配对码不对，或者已经被用过了。请在对方电脑上换一个配对码。'
    case 'PAIRING_CODE_EXPIRED':
      return '配对码过期了。请在对方电脑上换一个。'
    case 'PAIRING_CODE_INVALID':
      return '这不是一个配对码，检查有没有多打字符。'
    case 'RATE_LIMITED':
      return '尝试次数过多，等一分钟再试。'
    case 'HOST_OFFLINE':
      return '对方电脑现在没有连到这台服务器。'
    case 'UNREACHABLE':
      return '连不上自建服务器，检查网络、域名和证书。'
    case 'NO_SERVER':
      return '还没有填写自建服务器地址，先到「服务器」页设置。'
    case 'NOT_SELF_HOSTED':
      return '当前不是自建服务器模式。'
    default:
      return failureMessage(failure, '配对没有成功')
  }
}
