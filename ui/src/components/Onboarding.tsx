import { useEffect, useState } from 'react'
import type {
  AccountUser,
  DatasetState,
  EngineId,
  EngineStatus,
  Kernel,
  ModelCheck,
  ModelState,
  RemotePrefs,
  ServerPrefs,
  ServerProbeResult,
  ServerView,
} from '../api'
import { AGENTS } from '../navigation'
import { ENGINE_INSTALL_HINTS } from '../engines'
import { formatEta, formatGigabytes, formatModelVersion, formatSpeed } from '../format'
import { useDownloadSpeed } from '../lib/use-download-speed'
import { useShellSettings, type ShellSettingsState } from '../lib/use-shell-settings'
import { type DesktopPlatform } from '../platform'
import { quitApp, shellAvailable, shellFailure } from '../shell'
import { Button } from './ui/button'
import { Progress } from './ui/progress'
import { Switch } from './ui/switch'
import { CheckIcon, ENGINE_MARKS, RingMark, Spinner } from '../icons'
import { DshPluginHint } from './DshPluginHint'
import { ServerChoice } from './ServerChoice'
import { useIsOled } from '../lib/oled'
import { ShellSettingsList } from './ShellSettingsList'
import { WindowControls } from './WindowControls'
import { WindowResizeHandles } from './WindowResizeHandles'
import { cn, failureMessage } from '../lib/utils'
import { probeFailureCopy, probeSuccessCopy } from '../lib/server'

/** Every step the guide can draw, in the order it walks them. */
const STEP_ORDER = [
  'welcome',
  'server',
  'login',
  'choose',
  'model',
  'remote',
  'preferences',
  'sharing',
  'finish',
] as const

type StepId = (typeof STEP_ORDER)[number]

/** The titles, keyed by step so a hidden step cannot shift the others' names. */
const STEP_TITLES: Record<StepId, string> = {
  welcome: '欢迎',
  server: '服务器',
  login: '登录',
  choose: '选择 Agent',
  model: '下载模型',
  remote: '远程 Agent',
  preferences: '偏好',
  sharing: '共享会话数据',
  finish: '完成',
}

/**
 * The steps a self-hosted install does not walk.
 *
 * No account service means no sign-in, and no dataset endpoint means the sharing
 * question has no recipient. Everything else — agents, model, remote policy,
 * startup — is local and stays.
 */
const SELF_HOSTED_HIDDEN: readonly StepId[] = ['login', 'sharing']

/** The phone app's current distribution channels; the guide links to both. */
const MOBILE_APP_TESTFLIGHT = 'https://testflight.apple.com/join/MyaqdT1z'
const MOBILE_APP_ANDROID = 'https://github.com/ShirazuNagisa/DSH-Mizar-Android/releases'

interface OnboardingProps {
  kernel: Kernel
  account: AccountUser | null
  serverConnected: boolean
  /** The stored relay choice, which this guide may change on its server step. */
  serverInfo: ServerView
  engines: EngineStatus[]
  model: ModelState | null
  /** The operator's current answer about anonymous session sharing. */
  dataset: DatasetState
  /** Which desktop the words and the window chrome belong to. */
  platform: DesktopPlatform
  accountBusy: boolean
  accountError: string | null
  onLogin: (identifier: string, password: string) => Promise<void>
  onGithubStart: () => Promise<void>
  onGithubExchange: (handoff: string) => Promise<void>
  onAppleStart: () => Promise<void>
  onAppleExchange: (handoff: string) => Promise<void>
  onRefreshState: () => Promise<void>
  /** Called after the kernel has been told the guide is done. */
  onFinished: () => Promise<void>
}

/**
 * The first-run guide: a server question, then everything local and, on the
 * official server, the account.
 *
 * The list of steps is not fixed: a self-hosted install skips 登录 and 共享会话数据
 * because its relay runs neither service. Progress and the footer's next/back
 * buttons walk the visible list, so "第 3 步 / 共 7 步" counts what this person
 * will actually see.
 *
 * The official flow is unchanged: the account is what makes the phone side work,
 * so on that path the login step will not let the window past without one.
 *
 * State lives where it is used: the server choice and the verification are this
 * component's (the guide is the only place that asks before saving), the account
 * comes down from the window, the engines from the snapshot, a download's
 * progress from the kernel's events, and the three shell switches from IPC.
 */
export function Onboarding({
  kernel,
  account,
  serverConnected,
  serverInfo,
  engines,
  model,
  dataset,
  platform,
  accountBusy,
  accountError,
  onLogin,
  onGithubStart,
  onGithubExchange,
  onAppleStart,
  onAppleExchange,
  onRefreshState,
  onFinished,
}: OnboardingProps) {
  const [step, setStep] = useState<StepId>('welcome')
  const [server, setServer] = useState<ServerPrefs>(() => ({
    mode: serverInfo.mode,
    origin: serverInfo.origin,
    pin: '',
  }))
  const [probe, setProbe] = useState<ServerProbeResult | null>(null)
  const [probing, setProbing] = useState(false)
  const [probeError, setProbeError] = useState<string | null>(null)
  // Which origin+PIN pair a successful check was made with. Editing either field
  // changes the key, so a stale "verified" cannot let a different server through.
  const [verifiedFor, setVerifiedFor] = useState<string | null>(null)
  const [selected, setSelected] = useState<Set<EngineId>>(
    () => new Set(engines.filter((engine) => engine.configured).map((engine) => engine.engine)),
  )
  const [share, setShare] = useState(() =>
    dataset.policyVersion > 0 ? dataset.enabled : true,
  )
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // Read once, at the start: the step is one screen of switches and the answer
  // saves on its 下一步, not on every click.
  const [remote, setRemote] = useState<RemotePrefs | null>(null)
  const [remoteError, setRemoteError] = useState<string | null>(null)
  const [remoteLoading, setRemoteLoading] = useState(true)
  const shell = useShellSettings()

  useEffect(() => {
    let cancelled = false
    void (async () => {
      try {
        const next = await kernel.send<RemotePrefs>({ op: 'remote_target_settings' })
        if (!cancelled) setRemote(next)
      } catch (failure) {
        if (!cancelled) setRemoteError(failureMessage(failure, '读取远程 Agent 设置失败'))
      } finally {
        if (!cancelled) setRemoteLoading(false)
      }
    })()
    return () => {
      cancelled = true
    }
  }, [kernel])

  async function retryRemote() {
    setRemoteLoading(true)
    setRemoteError(null)
    try {
      setRemote(await kernel.send<RemotePrefs>({ op: 'remote_target_settings' }))
    } catch (failure) {
      setRemoteError(failureMessage(failure, '读取远程 Agent 设置失败'))
    } finally {
      setRemoteLoading(false)
    }
  }

  function toggle(engine: EngineId) {
    setSelected((current) => {
      const next = new Set(current)
      if (next.has(engine)) next.delete(engine)
      else next.add(engine)
      return next
    })
  }

  // What this person actually walks. A hidden step can never be the current one:
  // the mode changes on the server step, before either hidden step is reachable.
  const visible = STEP_ORDER.filter((id) =>
    server.mode === 'selfHosted' ? !SELF_HOSTED_HIDDEN.includes(id) : true,
  )
  const place = Math.max(visible.indexOf(step), 0)

  function neighbours(from: StepId) {
    const at = Math.max(visible.indexOf(from), 0)
    return {
      next: visible[Math.min(at + 1, visible.length - 1)] ?? 'finish',
      previous: visible[Math.max(at - 1, 0)] ?? 'welcome',
    }
  }

  const verificationKey = `${server.mode}|${server.origin.trim()}|${server.pin.trim()}`
  const serverVerified = server.mode === 'official' || verifiedFor === verificationKey

  function chooseMode(mode: ServerPrefs['mode']) {
    setServer((current) => ({ ...current, mode }))
    forgetVerification()
  }

  function changeOrigin(origin: string) {
    setServer((current) => ({ ...current, origin }))
    forgetVerification()
  }

  function changePin(pin: string) {
    setServer((current) => ({ ...current, pin }))
    forgetVerification()
  }

  function forgetVerification() {
    setProbe(null)
    setProbeError(null)
    setVerifiedFor(null)
  }

  async function verify() {
    setProbing(true)
    forgetVerification()
    try {
      const result = await kernel.send<ServerProbeResult>({
        op: 'server_probe',
        origin: server.origin,
        pin: server.pin.trim(),
      })
      setProbe(result)
      setVerifiedFor(result.ok ? verificationKey : null)
    } catch (failure) {
      setProbeError(failureMessage(failure, '验证失败'))
    } finally {
      setProbing(false)
    }
  }

  function back() {
    setError(null)
    setStep(neighbours(step).previous)
  }

  function forward() {
    setError(null)
    void moveForward()
  }

  async function moveForward() {
    if (step === 'server') {
      setBusy(true)
      try {
        await kernel.send({
          op: 'set_server_prefs',
          mode: server.mode,
          origin: server.origin,
          // An untouched PIN field must not erase the stored one; a typed value
          // is the new PIN. Clearing it happens by switching servers and typing
          // a new one, which is the only case where an empty field is meaningful.
          ...(server.pin.trim() === '' ? {} : { pin: server.pin.trim() }),
        })
        // The tunnel is rebuilt by the kernel; the snapshot carries the new
        // server view the rest of the guide and the window draw from.
        await onRefreshState().catch(() => undefined)
        setStep(neighbours('server').next)
      } catch (failure) {
        setError(failureMessage(failure, '没有记住服务器设置'))
      } finally {
        setBusy(false)
      }
      return
    }
    if (step === 'choose') {
      setBusy(true)
      try {
        for (const { engine } of AGENTS) {
          if (!selected.has(engine)) continue
          const status = engines.find((candidate) => candidate.engine === engine)
          if (status?.available !== true) continue
          await kernel.send({ op: 'engine_configure', engine, enabled: true })
        }
        // A failed reread is not a reason to hold the guide: the window polls,
        // and the settings it just wrote are already on disk.
        await onRefreshState().catch(() => undefined)
        setStep(neighbours('choose').next)
      } catch (failure) {
        setError(failureMessage(failure, '没有记住所选的 Agent'))
      } finally {
        setBusy(false)
      }
      return
    }
    if (step === 'remote') {
      // Nothing was read means nothing is remembered: it would replace whatever
      // the machine already had with defaults.
      if (remote === null) {
        setStep(neighbours('remote').next)
        return
      }
      setBusy(true)
      try {
        await kernel.send({
          op: 'set_remote_target_settings',
          enabled: remote.enabled,
          allow_lan: remote.allowLan,
          allow_wan: remote.allowWan,
          allow_all_workspaces: remote.allowAllWorkspaces,
          workspaces: remote.workspaces,
          blocked: remote.blocked,
        })
        setStep(neighbours('remote').next)
      } catch (failure) {
        setError(failureMessage(failure, '没有记住远程 Agent 的选择'))
      } finally {
        setBusy(false)
      }
      return
    }
    if (step === 'sharing') {
      setBusy(true)
      try {
        await kernel.send({ op: 'dataset_share', enabled: share })
        setStep(neighbours('sharing').next)
      } catch (failure) {
        setError(failureMessage(failure, '没有记住共享选择'))
      } finally {
        setBusy(false)
      }
      return
    }
    if (step === 'finish') {
      setBusy(true)
      try {
        await kernel.send({ op: 'onboarding_complete' })
        await onFinished()
      } catch (failure) {
        setError(failureMessage(failure, '没有完成引导'))
      } finally {
        setBusy(false)
      }
      return
    }
    setStep(neighbours(step).next)
  }

  const anyAvailable = engines.some((candidate) => candidate.available)
  const canAdvance =
    step === 'server'
      ? serverVerified
      : step === 'login'
        ? account !== null
        : step === 'choose'
          ? selected.size > 0 || !anyAvailable
          : true

  return (
    <div className="flex h-full w-full flex-col bg-bg text-fg">
      {/* The window draws its own chrome on macOS and Windows: this first row
          is the drag strip. macOS overlays its traffic lights here; Windows
          paints its window buttons in the top-right corner instead. Linux
          keeps its title bar. */}
      {platform === 'macos' || platform === 'windows' ? (
        <div data-tauri-drag-region className="h-8 shrink-0" />
      ) : (
        <div className="h-2 shrink-0" />
      )}
      <WindowControls />
      <WindowResizeHandles />
      <div className="flex min-h-0 flex-1 justify-center overflow-y-auto px-8 pb-8">
        <div className="flex min-h-full w-full max-w-[560px] flex-col pt-2">
          <div className="flex items-center gap-1.5">
            {visible.map((id, index) => (
              <span
                key={id}
                className={cn(
                  'h-2 flex-1 mz-border',
                  index <= place ? 'bg-ink' : 'bg-transparent',
                )}
              />
            ))}
          </div>
          <p className="mt-2.5 text-[12px] text-muted">
            第 {place + 1} 步 / 共 {visible.length} 步 · {STEP_TITLES[step] ?? ''}
          </p>

          <div className="mt-8 flex-1">
            {step === 'welcome' ? <WelcomeStep /> : null}
            {step === 'server' ? (
              <ServerStep
                mode={server.mode}
                origin={server.origin}
                pin={server.pin}
                hasPin={serverInfo.hasPin}
                probing={probing}
                probe={probe}
                probeError={probeError}
                onMode={chooseMode}
                onOrigin={changeOrigin}
                onPin={changePin}
                onVerify={() => void verify()}
              />
            ) : null}
            {step === 'login' ? (
              <LoginStep
                kernel={kernel}
                account={account}
                serverConnected={serverConnected}
                busy={accountBusy}
                error={accountError}
                onLogin={onLogin}
                onGithubStart={onGithubStart}
                onGithubExchange={onGithubExchange}
                onAppleStart={onAppleStart}
                onAppleExchange={onAppleExchange}
              />
            ) : null}
            {step === 'choose' ? (
              <AgentStep
                kernel={kernel}
                engines={engines}
                selected={selected}
                onToggle={toggle}
                onRefreshState={onRefreshState}
              />
            ) : null}
            {step === 'model' ? (
              <ModelStep
                kernel={kernel}
                model={model}
                onLater={() => {
                  setError(null)
                  setStep(neighbours('model').next)
                }}
              />
            ) : null}
            {step === 'remote' ? (
              <RemoteStep
                selfHosted={server.mode === 'selfHosted'}
                prefs={remote}
                loading={remoteLoading}
                error={remoteError}
                onChange={setRemote}
                onRetry={() => void retryRemote()}
              />
            ) : null}
            {step === 'preferences' ? <PreferenceStep shell={shell} platform={platform} /> : null}
            {step === 'sharing' ? (
              <SharingStep kernel={kernel} share={share} onChange={setShare} />
            ) : null}
            {step === 'finish' ? (
              <DoneStep
                account={account}
                server={serverInfo}
                engines={engines}
                selected={selected}
                model={model}
                shell={shell}
                platform={platform}
                sharing={share}
                remote={remote}
              />
            ) : null}
          </div>

          {error === null ? null : (
            <p className="mt-4 text-[12px] leading-5 bg-ink px-1.5 text-bg">{error}</p>
          )}

          <footer className="mt-8 flex shrink-0 items-center gap-2 mz-divide-t pt-4">
            <Button variant="ghost" onClick={back} disabled={step === 'welcome' || busy}>
              上一步
            </Button>
            <div className="flex-1" />
            {step === 'server' && server.mode === 'selfHosted' && !serverVerified ? (
              <span className="text-[12px] text-muted">验证连通性后才能继续</span>
            ) : null}
            {step === 'login' && account === null ? (
              <span className="text-[12px] text-muted">登录后才能继续</span>
            ) : null}
            {step === 'choose' && selected.size === 0 ? (
              <span className="text-[12px] text-muted">至少选一个 Agent</span>
            ) : null}
            <Button variant="primary" size="lg" onClick={forward} disabled={!canAdvance || busy}>
              {busy ? <Spinner /> : null}
              {step === 'finish' ? '开始使用' : '下一步'}
            </Button>
          </footer>
        </div>
      </div>
    </div>
  )
}

/**
 * The second question the guide asks: which relay, and — for a self-hosted one —
 * whether it can be reached.
 *
 * The official card is the default and needs no check. The self-hosted card
 * requires a passing connectivity check before the footer's 下一步 unlocks: a
 * saved origin that does not answer would leave the person with a tunnel that
 * never comes up and no explanation on the next screen.
 */
function ServerStep({
  mode,
  origin,
  pin,
  hasPin,
  probing,
  probe,
  probeError,
  onMode,
  onOrigin,
  onPin,
  onVerify,
}: {
  mode: ServerPrefs['mode']
  origin: string
  pin: string
  hasPin: boolean
  probing: boolean
  probe: ServerProbeResult | null
  probeError: string | null
  onMode: (mode: ServerPrefs['mode']) => void
  onOrigin: (origin: string) => void
  onPin: (pin: string) => void
  onVerify: () => void
}) {
  return (
    <section>
      <h2 className="text-[24px] leading-8">连接服务器</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        手机通过一台服务器找到这台电脑。用官方服务器不用设置；有自己的 relay 就选自建，填域名和 PIN。
      </p>

      <div className="mt-4 grid grid-cols-2 gap-2">
        <ServerChoice
          title="官方服务器"
          detail="dshmizar-1.shirazu-nagisa.com"
          hint="需要登录账户"
          selected={mode === 'official'}
          onClick={() => onMode('official')}
        />
        <ServerChoice
          title="自建服务器"
          detail="你自己的 relay"
          hint="无需账户"
          selected={mode === 'selfHosted'}
          onClick={() => onMode('selfHosted')}
        />
      </div>

      {mode === 'official' ? (
        <div className="mt-4 mz-border px-4 py-3">
          <div className="text-[12px] text-fg">官方服务器</div>
          <p className="mt-0.5 text-[12px] leading-5 text-muted">
            手机与这台电脑登录同一个账户后就能互相找到；下一步是登录。
          </p>
        </div>
      ) : (
        <div className="mt-4 mz-border">
          <div className="flex flex-col gap-2 px-4 py-3">
            <label className="text-[12px] text-muted" htmlFor="onboarding-origin">
              服务器域名
            </label>
            <input
              id="onboarding-origin"
              value={origin}
              onChange={(event) => onOrigin(event.target.value)}
              placeholder="relay.example.com"
              autoCapitalize="off"
              autoComplete="off"
              spellCheck={false}
              className="h-9 mz-border bg-transparent px-3 font-mono text-[12px] text-fg outline-none"
            />
            <p className="-mt-0.5 text-[11px] leading-4 text-muted">
              只填域名或 https://域名；必须使用 https，不能带路径。
            </p>
            <label className="mt-2 text-[12px] text-muted" htmlFor="onboarding-pin">
              服务器 PIN
            </label>
            <input
              id="onboarding-pin"
              type="password"
              value={pin}
              onChange={(event) => onPin(event.target.value)}
              autoComplete="off"
              placeholder={hasPin ? '已保存 PIN，留空保持不变' : '服务器没有 PIN 可以留空'}
              className="h-9 mz-border bg-transparent px-3 font-mono text-[12px] tracking-wider text-fg outline-none"
            />
          </div>
          <div className="mz-divide-t flex items-center justify-between gap-4 px-4 py-2.5">
            <Button
              size="sm"
              variant="outline"
              onClick={onVerify}
              disabled={probing || origin.trim() === ''}
            >
              {probing ? <Spinner /> : null}
              验证连通性
            </Button>
            <ProbeStatus
              probing={probing}
              probe={probe}
              error={probeError}
            />
          </div>
          <p className="mz-divide-t px-4 py-2 text-[12px] leading-5 text-muted">
            自建服务器：无需登录，也不会启用账户、订阅和匿名数据共享。
          </p>
        </div>
      )}
    </section>
  )
}

/** What the last connectivity check said, in this page's words. */
function ProbeStatus({
  probing,
  probe,
  error,
}: {
  probing: boolean
  probe: ServerProbeResult | null
  error: string | null
}) {
  if (probing) {
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-muted">
        <Spinner size={12} />
        正在验证…
      </span>
    )
  }
  if (error !== null) {
    return <span className="text-right text-[12px] leading-5 bg-ink px-1.5 text-bg">{error}</span>
  }
  if (probe === null) {
    return <span className="text-right text-[12px] leading-5 text-muted">先验证再继续</span>
  }
  if (probe.ok) {
    return (
      <span className="flex items-start justify-end gap-1.5 text-right text-[12px] leading-5 text-fg">
        <CheckIcon size={14} />
        <span>{probeSuccessCopy(probe)}</span>
      </span>
    )
  }
  return (
    <span className="text-right text-[12px] leading-5 bg-ink px-1.5 text-bg">
      {probeFailureCopy(probe)}
    </span>
  )
}

function WelcomeStep() {
  return (
    <section>
      <RingMark size={48} />
      <h1 className="mt-5 text-[24px] font-semibold tracking-tight text-fg">欢迎使用 AGI Mizar</h1>
      <p className="mt-3 text-[12px] leading-6 text-muted">
        这是一台电脑端宿主：它把本机的 agent 交给你的手机。用官方服务器时，手机端 DSH Mizar
        登录同一个账户就能看到并操控这台电脑；也可以把它接到你自己的 relay 上。
      </p>
      <p className="mt-2 text-[12px] leading-6 text-muted">
        接下来先选服务器，再决定用哪些 Agent、准备模型，最后决定这台电脑以后怎么启动。
        已经保存的账户、已安装的 Agent 和模型都会原样带过来。
      </p>
    </section>
  )
}

interface LoginStepProps {
  kernel: Kernel
  account: AccountUser | null
  serverConnected: boolean
  busy: boolean
  error: string | null
  onLogin: (identifier: string, password: string) => Promise<void>
  onGithubStart: () => Promise<void>
  onGithubExchange: (handoff: string) => Promise<void>
  onAppleStart: () => Promise<void>
  onAppleExchange: (handoff: string) => Promise<void>
}

/**
 * The account the phone finds this computer by.
 *
 * The form is the same one 登录 has, copied rather than shared: the guide may not
 * skip this step, so it needs its own copy of the fields, and the login page
 * keeps its own shape for the case where someone signs in later.
 */
function LoginStep({
  kernel,
  account,
  serverConnected,
  busy,
  error,
  onLogin,
  onGithubStart,
  onGithubExchange,
  onAppleStart,
  onAppleExchange,
}: LoginStepProps) {
  const [identifier, setIdentifier] = useState('')
  const [password, setPassword] = useState('')
  const [codeStarted, setCodeStarted] = useState<'github' | 'apple' | null>(null)
  const [handoff, setHandoff] = useState('')
  const [quitting, setQuitting] = useState(false)
  const [quitError, setQuitError] = useState<string | null>(null)

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (identifier.trim() === '' || password === '') return
    await onLogin(identifier.trim(), password)
    setPassword('')
  }

  async function quit() {
    setQuitting(true)
    setQuitError(null)
    try {
      await quitApp()
    } catch (failure) {
      setQuitError(shellFailure(failure))
      setQuitting(false)
    }
  }

  return (
    <section>
      <h2 className="text-[24px] leading-8">登录账户</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        手机端与这台电脑登录同一个账户，就能互相找到。这里没有配对码这一步。
      </p>

      {account === null ? (
        <form className="mt-4" onSubmit={submit}>
          <div className="flex flex-col gap-2">
            <label className="text-[12px] text-muted" htmlFor="onboarding-identifier">
              邮箱或用户名
            </label>
            <input
              id="onboarding-identifier"
              value={identifier}
              onChange={(event) => setIdentifier(event.target.value)}
              autoComplete="username"
              autoCapitalize="off"
              spellCheck={false}
              className="h-9 mz-border bg-transparent px-3 text-[12px] text-fg outline-none"
            />
            <label className="mt-2 text-[12px] text-muted" htmlFor="onboarding-password">
              密码
            </label>
            <input
              id="onboarding-password"
              type="password"
              value={password}
              onChange={(event) => setPassword(event.target.value)}
              autoComplete="current-password"
              className="h-9 mz-border bg-transparent px-3 text-[12px] text-fg outline-none"
            />
          </div>

          <div className="mt-4 flex items-center gap-3">
            <Button
              type="submit"
              variant="primary"
              disabled={busy || identifier.trim() === '' || password === ''}
            >
              {busy ? <Spinner /> : null}
              登录
            </Button>
            <span className="text-[12px] text-muted">
              服务器连接：{serverConnected ? '已连接' : '未连接'}
            </span>
          </div>

          {error === null ? null : (
            <p className="mt-3 text-[12px] leading-5 bg-ink px-1.5 text-bg">{error}</p>
          )}

          <div className="my-4 flex items-center gap-3 text-[12px] text-muted">
            <span className="h-px flex-1 bg-ink" />
            或
            <span className="h-px flex-1 bg-ink" />
          </div>

          <div className="flex flex-col gap-2">
            <Button
              variant="outline"
              disabled={busy}
              onClick={() => {
                void onGithubStart()
                setCodeStarted('github')
              }}
            >
              使用 GitHub 登录
            </Button>
            <Button
              variant="outline"
              disabled={busy}
              onClick={() => {
                void onAppleStart()
                setCodeStarted('apple')
              }}
            >
              使用 Apple 登录
            </Button>
          </div>

          {codeStarted === null ? null : (
            <div className="mt-3 mz-border p-4">
              <p className="text-[12px] leading-5 text-muted">
                浏览器里完成 {codeStarted === 'apple' ? 'Apple' : 'GitHub'} 授权后，页面会给出一个
                <code>MZ-XXXX-XXXX</code> 代码，粘贴到这里完成登录。
              </p>
              <div className="mt-3 flex gap-2">
                <input
                  value={handoff}
                  onChange={(event) => setHandoff(event.target.value.toUpperCase())}
                  placeholder="MZ-XXXX-XXXX"
                  spellCheck={false}
                  className="h-9 flex-1 mz-border bg-transparent px-3 font-mono text-[12px] tracking-wider text-fg outline-none"
                />
                <Button
                  variant="primary"
                  disabled={busy || handoff.trim().length < 5}
                  onClick={() =>
                    void (codeStarted === 'apple'
                      ? onAppleExchange(handoff.trim())
                      : onGithubExchange(handoff.trim()))
                  }
                >
                  完成
                </Button>
              </div>
            </div>
          )}
        </form>
      ) : (
        <div className="mt-4 mz-border px-4 py-3">
          <div className="flex items-center gap-2 text-[12px] text-fg">
            <CheckIcon size={14} />
            已登录为 {account.username}
          </div>
          <p className="mt-1 text-[12px] leading-5 text-muted">
            手机端用同一个账户登录后，就能在这台电脑的会话里远程操作。也可以直接点下一步。
          </p>
        </div>
      )}

      <div className="mt-5 mz-border px-4 py-3">
        <div className="text-[12px] text-fg">下载手机端 DSH Mizar 配合使用</div>
        <p className="mt-0.5 text-[12px] leading-5 text-muted">
          手机与这台电脑登录同一个账户后，就能查看并操控这里的会话。iOS 通过 TestFlight
          分发，Android 直接安装 APK。
        </p>
        <div className="mt-3 flex flex-col gap-3">
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <div className="text-[12px] text-fg">iPhone / iPad</div>
              <p className="mt-0.5 selectable break-all font-mono text-[11px] leading-4 text-muted">
                {MOBILE_APP_TESTFLIGHT}
              </p>
            </div>
            <Button
              size="sm"
              variant="outline"
              className="shrink-0"
              onClick={() =>
                void kernel.send({ op: 'open_external', url: MOBILE_APP_TESTFLIGHT })
              }
            >
              打开 TestFlight 链接 ↗
            </Button>
          </div>
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <div className="text-[12px] text-fg">Android</div>
              <p className="mt-0.5 selectable break-all font-mono text-[11px] leading-4 text-muted">
                {MOBILE_APP_ANDROID}
              </p>
            </div>
            <Button
              size="sm"
              variant="outline"
              className="shrink-0"
              onClick={() => void kernel.send({ op: 'open_external', url: MOBILE_APP_ANDROID })}
            >
              打开下载页 ↗
            </Button>
          </div>
        </div>
      </div>

      <div className="mt-6 flex items-center gap-3">
        <Button
          variant="ghost"
          size="sm"
          onClick={() => void quit()}
          disabled={quitting || !shellAvailable()}
          title={shellAvailable() ? undefined : '仅桌面窗口可用'}
        >
          {quitting ? <Spinner /> : null}
          退出应用
        </Button>
        {shellAvailable() ? null : <span className="text-[12px] text-muted">仅桌面窗口可用</span>}
        {quitError === null ? null : <span className="text-[12px] bg-ink px-1.5 text-bg">{quitError}</span>}
      </div>
    </section>
  )
}

interface AgentStepProps {
  kernel: Kernel
  engines: EngineStatus[]
  selected: Set<EngineId>
  onToggle: (engine: EngineId) => void
  onRefreshState: () => Promise<void>
}

/** Which agents this computer can actually reach, and the install for the rest. */
function AgentStep({ kernel, engines, selected, onToggle, onRefreshState }: AgentStepProps) {
  const oled = useIsOled()
  const [rescanning, setRescanning] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const missing = AGENTS.filter(({ engine }) => {
    const status = engines.find((candidate) => candidate.engine === engine)
    return status?.available !== true
  })

  async function rescan() {
    setRescanning(true)
    setError(null)
    try {
      await kernel.send({ op: 'engine_rescan' })
      await onRefreshState()
    } catch (failure) {
      setError(failureMessage(failure, '刷新列表没有成功'))
    } finally {
      setRescanning(false)
    }
  }

  return (
    <section>
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <h2 className="text-[24px] leading-8">选择要使用的 Agent</h2>
          <p className="mt-1.5 text-[12px] leading-5 text-muted">
            只列出本机已经安装的。没装的可以先跳过，装好之后在对应的 Agent 页面一键配置。
          </p>
        </div>
        <Button
          size="sm"
          variant="outline"
          className="shrink-0"
          onClick={() => void rescan()}
          disabled={rescanning}
        >
          {rescanning ? <Spinner /> : null}
          刷新列表
        </Button>
      </div>

      <div className="mt-4 grid grid-cols-3 gap-2">
        {AGENTS.map(({ engine, name }) => {
          const status = engines.find((candidate) => candidate.engine === engine)
          const available = status?.available ?? false
          const chosen = selected.has(engine)
          const Mark = ENGINE_MARKS[engine]
          return (
            <button
              key={engine}
              type="button"
              aria-pressed={chosen}
              disabled={!available}
              onClick={() => onToggle(engine)}
              className={cn(
                'flex flex-col gap-2 border p-3 text-left outline-none',
                // The one-bit card is an outline that thickens on hover and goes
                // dashed when the agent is not installed. The classic card is a
                // surface that lifts instead — there is a border width to spare.
                oled ? 'rounded-none' : 'rounded-xl transition-colors',
                available
                  ? chosen
                    ? oled
                      ? 'border-ink bg-ink text-bg'
                      : 'border-accent bg-accent/10 text-fg ring-1 ring-accent'
                    : oled
                      ? 'border-ink hover:border-2'
                      : 'border-border bg-surface hover:bg-surface-2'
                  : oled
                    ? 'cursor-not-allowed border-dashed border-ink'
                    : 'cursor-not-allowed border-dashed border-border bg-surface-2/50 opacity-60',
              )}
            >
              <span className="flex w-full items-center justify-between">
                <Mark size={20} />
                {chosen ? <CheckIcon size={14} /> : null}
              </span>
              <span>
                <span className="block text-[12px]">{name}</span>
                <span className="mt-0.5 block text-[12px] leading-4">
                  {available ? (status?.configured ? '已配置' : '已安装') : '本机未检测到'}
                </span>
              </span>
            </button>
          )
        })}
      </div>

      {selected.has('dsh') ? (
        <DshPluginHint className="mt-3" kernel={kernel} />
      ) : null}

      {missing.length === 0 ? null : (
        <div className="mt-4 flex flex-col gap-1">
          {missing.map(({ engine, name }) => (
            <p key={engine} className="text-[12px] leading-5 text-muted">
              {name}：{ENGINE_INSTALL_HINTS[engine]}
            </p>
          ))}
        </div>
      )}

      {error === null ? null : <p className="mt-3 text-[12px] bg-ink px-1.5 text-bg">{error}</p>}
    </section>
  )
}

interface ModelStepProps {
  kernel: Kernel
  model: ModelState | null
  onLater: () => void
}

/** The model the host judges turns with, downloaded once. */
function ModelStep({ kernel, model, onLater }: ModelStepProps) {
  const [check, setCheck] = useState<ModelCheck | null>(null)
  const [checking, setChecking] = useState(true)
  const [busy, setBusy] = useState<'download' | 'cancel' | null>(null)
  const [error, setError] = useState<string | null>(null)

  const active = model?.active ?? null
  const download = model?.download ?? null
  const latest = check?.latest ?? null
  const percent =
    download === null || download.total === 0
      ? 0
      : Math.round((download.done / download.total) * 100)
  const { speed, eta } = useDownloadSpeed(download)
  // Where it is coming from, how fast, and how long: a bare percentage cannot
  // tell "stuck" from "crawling", which is the whole complaint about this step.
  const moving = [
    download?.source ?? null,
    download !== null && download.state === 'running' && speed > 0 ? formatSpeed(speed) : null,
    formatEta(eta),
  ].filter((part): part is string => part !== null)
  // The download finishes while this step is mounted, so "ready" has to be
  // judged against the model that is now on disk, not the check made on mount.
  const ready =
    active !== null &&
    (latest === null || !latest.newer || active.version === latest.version)

  async function recheck() {
    setChecking(true)
    setError(null)
    try {
      setCheck(await kernel.send<ModelCheck>({ op: 'model_check' }))
    } catch (failure) {
      setError(failureMessage(failure, '检查模型版本失败'))
    } finally {
      setChecking(false)
    }
  }

  useEffect(() => {
    // Entering this step is the moment to ask; the answer is one request, and
    // it is re-asked by 重试 if it fails.
    void recheck()
  }, [])

  async function startDownload(version: string) {
    setBusy('download')
    setError(null)
    try {
      await kernel.send({ op: 'model_download', version })
    } catch (failure) {
      setError(failureMessage(failure, '下载没有开始'))
    } finally {
      setBusy(null)
    }
  }

  async function cancelDownload() {
    setBusy('cancel')
    setError(null)
    try {
      await kernel.send({ op: 'model_cancel' })
    } catch (failure) {
      setError(failureMessage(failure, '取消没有成功'))
    } finally {
      setBusy(null)
    }
  }

  return (
    <section>
      <h2 className="text-[24px] leading-8">准备模型</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        Mizarllm 是判断「这一轮做完没有」的本地模型，由这台电脑运行，约 1 GB，只需要下载一次。
      </p>

      <div className="mt-4 divide-y divide-ink mz-border">
        <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
          <span className="shrink-0 text-muted">当前版本</span>
          <span className="selectable truncate text-fg">
            {active === null ? '未下载' : formatModelVersion(active.version)}
          </span>
        </div>
        <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
          <span className="shrink-0 text-muted">最新版本</span>
          <span className="selectable truncate text-fg">
            {checking ? '读取中…' : latest === null ? '未知' : formatModelVersion(latest.version)}
          </span>
        </div>

        <div className="px-4 py-3">
          {checking ? (
            <span className="flex items-center gap-1.5 text-[12px] text-muted">
              <Spinner size={12} />
              正在检查模型版本…
            </span>
          ) : error !== null ? (
            <div className="flex items-center justify-between gap-4">
              <span className="text-[12px] leading-5 bg-ink px-1.5 text-bg">{error}</span>
              <Button size="sm" variant="outline" onClick={() => void recheck()}>
                重试
              </Button>
            </div>
          ) : check !== null && check.error !== null ? (
            <div className="flex items-center justify-between gap-4">
              <span className="text-[12px] leading-5 bg-ink px-1.5 text-bg">{check.error}</span>
              <Button size="sm" variant="outline" onClick={() => void recheck()}>
                重试
              </Button>
            </div>
          ) : ready ? (
            <div className="flex items-center gap-2">
              <CheckIcon size={14} />
              <span className="text-[12px] text-fg">已就绪，无需下载</span>
            </div>
          ) : latest === null ? (
            <div className="flex items-center justify-between gap-4">
              <span className="text-[12px] leading-5 text-muted">没有取到最新版本。</span>
              <Button size="sm" variant="outline" onClick={() => void recheck()}>
                重试
              </Button>
            </div>
          ) : (
            <div className="flex items-center justify-between gap-4">
              <span className="text-[12px] leading-5 text-fg">
                可下载 {formatModelVersion(latest.version)}
                <span className="text-muted"> · {formatGigabytes(latest.size_bytes)}</span>
              </span>
              <Button
                size="sm"
                variant="primary"
                onClick={() => void startDownload(latest.version)}
                disabled={busy !== null}
              >
                {busy === 'download' ? <Spinner /> : null}
                下载
              </Button>
            </div>
          )}
        </div>

        {download === null ? null : (
          <div className="px-4 py-3">
            <div className="flex items-center justify-between gap-4 text-[12px]">
              <span className="text-fg">
                {download.state === 'verifying'
                  ? `正在校验 ${download.version}`
                  : download.state === 'failed'
                    ? `下载失败 ${download.version}`
                    : `正在下载 ${download.version}`}
              </span>
              <span className="tabular shrink-0 text-muted">{percent}%</span>
            </div>
            <Progress className="mt-2" value={percent} label={`下载模型 ${download.version}`} />
            <div className="mt-1.5 flex items-center justify-between gap-4 text-[12px] text-muted">
              <span className="tabular">
                {formatGigabytes(download.done)} / {formatGigabytes(download.total)}
              </span>
              {download.state === 'failed' ? (
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void startDownload(download.version)}
                  disabled={busy !== null}
                >
                  重试
                </Button>
              ) : (
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void cancelDownload()}
                  disabled={busy !== null}
                >
                  {busy === 'cancel' ? <Spinner /> : null}
                  取消
                </Button>
              )}
            </div>
            {moving.length === 0 || download.state === 'failed' ? null : (
              <div className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[12px] text-muted">
                {moving.map((part) => (
                  <span key={part} className="tabular">
                    {part}
                  </span>
                ))}
              </div>
            )}
            {download.error === null ? null : (
              <p className="mt-1.5 text-[12px] leading-5 bg-ink px-1.5 text-bg">{download.error}</p>
            )}
          </div>
        )}
      </div>

      <div className="mt-4 flex items-center justify-between gap-4">
        <Button variant="ghost" size="sm" onClick={onLater}>
          稍后再下载
        </Button>
        <span className="text-[12px] leading-5 text-muted">
          之后可以在「设置 → 模型」里下载。
        </span>
      </div>
    </section>
  )
}

/**
 * The remote-agent question, asked once.
 *
 * It is here because the feature only works when both computers say yes, and a
 * person is most likely to have both in front of them while setting up. The
 * routes are separate because one is free and physical (same LAN) and the other
 * costs money (through the relay), so turning the feature on must not turn the
 * paid route on with it.
 */
function RemoteStep({
  selfHosted,
  prefs,
  loading,
  error,
  onChange,
  onRetry,
}: {
  selfHosted: boolean
  prefs: RemotePrefs | null
  loading: boolean
  error: string | null
  onChange: (next: RemotePrefs) => void
  onRetry: () => void
}) {
  return (
    <section>
      <h2 className="text-[24px] leading-8">远程 Agent</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        {selfHosted
          ? '自建服务器模式下，另一台电脑可以把任务交给这台电脑上的 Agent。这里先决定这台电脑是否接受；之后随时可以在「设置 → 远程Agent」里改。'
          : '同账户的另一台电脑可以把任务交给这台电脑上的 Agent，反过来也可以。这里先决定这台电脑是否接受；之后随时可以在「设置 → 远程Agent」里改。'}
      </p>

      {loading ? (
        <p className="mt-4 flex items-center gap-1.5 text-[12px] text-muted">
          <Spinner size={12} />
          正在读取这台电脑的设置…
        </p>
      ) : prefs === null ? (
        <div className="mt-4 flex items-center justify-between gap-4 mz-border px-4 py-2.5">
          <span className="text-[12px] leading-5 text-muted">{error ?? '没有读到这台电脑的设置'}</span>
          <Button size="sm" variant="outline" onClick={onRetry}>
            重试
          </Button>
        </div>
      ) : (
        <div className="mt-4 mz-border">
          <div className="flex flex-col gap-3 px-4 py-3">
            <RemoteRow
              label="允许远程 Agent"
              hint="总开关。关着时这台电脑不接受任何其他电脑的任务。"
              checked={prefs.enabled}
              onChange={(enabled) => onChange({ ...prefs, enabled })}
            />
            <RemoteRow
              label="允许局域网（免费）"
              hint="同一局域网、同账户；不经过中继。"
              checked={prefs.allowLan}
              disabled={!prefs.enabled}
              onChange={(allowLan) => onChange({ ...prefs, allowLan })}
            />
            <RemoteRow
              label={selfHosted ? '允许跨网（自建服务器）' : '允许跨网（付费）'}
              hint={
                selfHosted
                  ? '通过你自己的 relay 转发；不需要账户。'
                  : '通过账户中继；需要付费账户。'
              }
              checked={prefs.allowWan}
              disabled={!prefs.enabled}
              onChange={(allowWan) => onChange({ ...prefs, allowWan })}
            />
            <RemoteRow
              label="对委派机开放所有工作区"
              hint="打开后，委派机可查询本机 DSH 的全部工作区，不再受白名单限制。"
              checked={prefs.allowAllWorkspaces}
              disabled={!prefs.enabled}
              onChange={(allowAllWorkspaces) => onChange({ ...prefs, allowAllWorkspaces })}
            />
          </div>
          <p className="mz-divide-t px-4 py-2 text-[12px] leading-5 text-muted">
            打开总开关后再选允许的线路；全部关着也能继续，之后在设置里改。
          </p>
        </div>
      )}

      {selfHosted ? (
        <div className="mt-4 mz-border px-4 py-3">
          <div className="flex items-start justify-between gap-4">
            <div className="min-w-0">
              <div className="text-[12px] text-fg">配对另一台电脑</div>
              <p className="mt-0.5 text-[12px] leading-5 text-muted">
                自建模式的跨网 peer 配对即将由配对码完成。
              </p>
            </div>
            <Button size="sm" variant="outline" disabled className="shrink-0">
              即将支持
            </Button>
          </div>
        </div>
      ) : null}
    </section>
  )
}

function RemoteRow({
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
        className="mt-0.5"
        checked={checked}
        disabled={disabled}
        onCheckedChange={onChange}
        label={label}
      />
    </div>
  )
}

/** How this computer behaves once it is closed and reopened. */
function PreferenceStep({
  shell,
  platform,
}: {
  shell: ShellSettingsState
  platform: DesktopPlatform
}) {
  return (
    <section>
      <h2 className="text-[24px] leading-8">这台电脑怎么启动</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        默认只显示{platform === 'macos' ? '程序坞' : '任务栏'}图标，另外两项是关的；
        随时可以在「设置 → 启动与窗口」里改，改动立即生效。
      </p>
      <div className="mt-4">
        <ShellSettingsList shell={shell} platform={platform} />
      </div>
    </section>
  )
}

/**
 * The one question in the guide that is not about this machine working.
 *
 * Pre-ticked on a first run, because the operator asked for that. The copy is
 * the consent: what is shared, what was removed first, and the fact that it is
 * only for Mizarllm. Un-ticking it is one click and sends the same command the
 * settings page sends.
 */
function SharingStep({
  kernel,
  share,
  onChange,
}: {
  kernel: Kernel
  share: boolean
  onChange: (next: boolean) => void
}) {
  return (
    <section>
      <h2 className="text-[24px] leading-8">共享会话数据</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        帮助改进 Mizarllm：匿名共享这台电脑上最近 12 个月的会话轮次，包括已归档的对话。
        每个轮次只包含你的指令、用到的工具、失败调用和最后几条回复。
      </p>

      <div className="mt-4 mz-border">
        <div className="flex items-start justify-between gap-6 px-4 py-3">
          <div className="min-w-0">
            <div className="text-[12px] text-fg">匿名共享会话数据以帮助我们改善</div>
            <p className="mt-0.5 text-[12px] leading-5 text-muted">
              上传前会在本机去掉账户、设备名、路径、邮箱和密钥；数据不公开、不出售、
              不与你的账户关联，只用于训练 Mizarllm 的轮次判定能力。
            </p>
          </div>
          <Switch
            className="mt-0.5"
            label="匿名共享会话数据以帮助我们改善"
            checked={share}
            onCheckedChange={onChange}
          />
        </div>
        <div className="mz-divide-t flex items-center justify-between gap-4 px-4 py-2">
          <span className="text-[12px] text-muted">默认开启，之后可以在设置里随时关闭。</span>
          <Button
            size="sm"
            variant="ghost"
            onClick={() =>
              void kernel.send({
                op: 'open_external',
                url: 'https://dsh-mizar.shirazu-nagisa.com/privacy/',
              })
            }
          >
            查看隐私政策
          </Button>
        </div>
      </div>
    </section>
  )
}

interface DoneStepProps {
  account: AccountUser | null
  /** The relay the guide just kept or chose. */
  server: ServerView
  engines: EngineStatus[]
  selected: Set<EngineId>
  model: ModelState | null
  shell: ShellSettingsState
  platform: DesktopPlatform
  sharing: boolean
  remote: RemotePrefs | null
}

/** What is in place, so the window does not leave the guide quietly wrong. */
function DoneStep({
  account,
  server,
  engines,
  selected,
  model,
  shell,
  platform,
  sharing,
  remote,
}: DoneStepProps) {
  const selfHosted = server.mode === 'selfHosted'
  const serverText = selfHosted
    ? `自建 · ${server.origin === '' ? '未填写域名' : server.origin}`
    : '官方'

  const configured = AGENTS.filter(
    ({ engine }) =>
      selected.has(engine) ||
      engines.find((candidate) => candidate.engine === engine)?.configured === true,
  )
  const active = model?.active ?? null

  const preferences: string[] = []
  if (shell.settings !== null) {
    if (shell.settings.launch_at_login) preferences.push('开机静默启动')
    if (shell.settings.show_dock_icon) {
      preferences.push(platform === 'macos' ? '程序坞图标' : '任务栏图标')
    }
    if (shell.settings.tray_menu_only) preferences.push('托盘只开快捷菜单')
  }
  const remoteText =
    remote === null
      ? '未读取'
      : !remote.enabled
        ? '未开启'
        : `已开启（${[
            remote.allowLan ? '局域网' : null,
            remote.allowWan ? '跨网' : null,
          ]
            .filter(Boolean)
            .join('、') || '没有线路'}${remote.allowAllWorkspaces ? ' · 全部工作区' : ''}）`

  const preferenceText =
    shell.settings === null
      ? shell.available
        ? '读取中…'
        : '仅桌面窗口可改'
      : preferences.length === 0
        ? '全部关闭'
        : preferences.join('、')

  return (
    <section>
      <h2 className="text-[24px] leading-8">都准备好了</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        下面这些已经就绪；没准备好的，进主页后也能补。
      </p>
      <div className="mt-4 divide-y divide-ink mz-border">
        <DoneRow label="服务器" value={serverText} />
        <DoneRow
          label="登录账户"
          value={selfHosted ? '自建服务器：无需登录' : (account?.username ?? '未登录')}
        />
        <DoneRow
          label="Agent"
          value={configured.length === 0 ? '未选择' : configured.map(({ name }) => name).join('、')}
        />
        <DoneRow label="模型" value={active === null ? '未下载' : `Mizarllm ${active.version}`} />
        <DoneRow label="偏好" value={preferenceText} />
        <DoneRow label="远程 Agent" value={remoteText} />
        {selfHosted ? null : (
          <DoneRow label="会话数据" value={sharing ? '匿名共享中' : '仅保留在本机'} />
        )}
      </div>
    </section>
  )
}

function DoneRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
      <span className="shrink-0 text-muted">{label}</span>
      <span className="truncate text-fg">{value}</span>
    </div>
  )
}
