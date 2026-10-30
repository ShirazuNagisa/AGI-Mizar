import { useEffect, useState } from 'react'
import type { AccountUser, EngineId, EngineStatus, Kernel, ModelCheck, ModelState } from '../api'
import { AGENTS } from '../navigation'
import { ENGINE_INSTALL_HINTS } from '../engines'
import { formatGigabytes, formatModelVersion } from '../format'
import { useShellSettings, type ShellSettingsState } from '../lib/use-shell-settings'
import { quitApp, shellAvailable, shellFailure } from '../shell'
import { Button } from './ui/button'
import { Progress } from './ui/progress'
import { CheckIcon, ENGINE_MARKS, Spinner, StarMark } from '../icons'
import { DshPluginHint } from './DshPluginHint'
import { ShellSettingsList } from './ShellSettingsList'
import { cn, failureMessage } from '../lib/utils'

/** The six steps, in the order the guide walks them. */
const STEP_TITLES = ['欢迎', '登录', '选择 Agent', '下载模型', '偏好', '完成'] as const

const WELCOME = 0
const LOGIN = 1
const CHOOSE = 2
const MODEL = 3
const PREFERENCES = 4
const FINISH = 5

interface OnboardingProps {
  kernel: Kernel
  account: AccountUser | null
  serverConnected: boolean
  engines: EngineStatus[]
  model: ModelState | null
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
 * The first-run guide: six steps, full window, no sidebar.
 *
 * It cannot be skipped — the account is what makes the phone side work, so the
 * login step will not let the window past until it has one. The two deliberate
 * ways out are 退出应用 on the login step (this guide is not for you right now)
 * and 稍后再下载 on the model step (1 GB is not a decision to force).
 *
 * State lives where it is used: the account comes down from the window, the
 * engines from the snapshot, a download's progress from the kernel's events, and
 * the three shell switches from IPC — each step reads the source that owns it
 * rather than copying it here.
 */
export function Onboarding({
  kernel,
  account,
  serverConnected,
  engines,
  model,
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
  const [step, setStep] = useState(WELCOME)
  const [selected, setSelected] = useState<Set<EngineId>>(
    () => new Set(engines.filter((engine) => engine.configured).map((engine) => engine.engine)),
  )
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const shell = useShellSettings()

  function toggle(engine: EngineId) {
    setSelected((current) => {
      const next = new Set(current)
      if (next.has(engine)) next.delete(engine)
      else next.add(engine)
      return next
    })
  }

  function back() {
    setError(null)
    setStep((current) => Math.max(current - 1, WELCOME))
  }

  function forward() {
    setError(null)
    void moveForward()
  }

  async function moveForward() {
    if (step === CHOOSE) {
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
        setStep(MODEL)
      } catch (failure) {
        setError(failureMessage(failure, '没有记住所选的 Agent'))
      } finally {
        setBusy(false)
      }
      return
    }
    if (step === FINISH) {
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
    setStep(step + 1)
  }

  const anyAvailable = engines.some((candidate) => candidate.available)
  const canAdvance =
    step === LOGIN
      ? account !== null
      : step === CHOOSE
        ? selected.size > 0 || !anyAvailable
        : true

  return (
    <div className="flex h-full w-full flex-col bg-bg text-fg">
      <div data-tauri-drag-region className="h-8 shrink-0" />
      <div className="flex min-h-0 flex-1 justify-center overflow-y-auto px-8 pb-8">
        <div className="flex min-h-full w-full max-w-[560px] flex-col pt-2">
          <div className="flex items-center gap-1.5">
            {STEP_TITLES.map((title, index) => (
              <span
                key={title}
                className={cn(
 'h-2 flex-1 border border-ink',
                  index <= step ? 'bg-ink' : 'bg-transparent',
                )}
              />
            ))}
          </div>
          <p className="mt-2.5 text-[12px] text-muted">
            第 {step + 1} 步 / 共 {STEP_TITLES.length} 步 · {STEP_TITLES[step] ?? ''}
          </p>

          <div className="mt-8 flex-1">
            {step === WELCOME ? <WelcomeStep /> : null}
            {step === LOGIN ? (
              <LoginStep
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
            {step === CHOOSE ? (
              <AgentStep
                kernel={kernel}
                engines={engines}
                selected={selected}
                onToggle={toggle}
                onRefreshState={onRefreshState}
              />
            ) : null}
            {step === MODEL ? (
              <ModelStep
                kernel={kernel}
                model={model}
                onLater={() => {
                  setError(null)
                  setStep(PREFERENCES)
                }}
              />
            ) : null}
            {step === PREFERENCES ? <PreferenceStep shell={shell} /> : null}
            {step === FINISH ? (
              <DoneStep
                account={account}
                engines={engines}
                selected={selected}
                model={model}
                shell={shell}
              />
            ) : null}
          </div>

          {error === null ? null : (
            <p className="mt-4 text-[12px] leading-5 bg-ink px-1.5 text-bg">{error}</p>
          )}

          <footer className="mt-8 flex shrink-0 items-center gap-2 border-t border-ink pt-4">
            <Button variant="ghost" onClick={back} disabled={step === WELCOME || busy}>
              上一步
            </Button>
            <div className="flex-1" />
            {step === LOGIN && account === null ? (
              <span className="text-[12px] text-muted">登录后才能继续</span>
            ) : null}
            {step === CHOOSE && selected.size === 0 ? (
              <span className="text-[12px] text-muted">至少选一个 Agent</span>
            ) : null}
            <Button variant="primary" size="lg" onClick={forward} disabled={!canAdvance || busy}>
              {busy ? <Spinner /> : null}
              {step === FINISH ? '开始使用' : '下一步'}
            </Button>
          </footer>
        </div>
      </div>
    </div>
  )
}

function WelcomeStep() {
  return (
    <section>
      <StarMark size={48} />
      <h1 className="mt-5 text-[24px] font-semibold tracking-tight text-fg">欢迎使用 AGI Mizar</h1>
      <p className="mt-3 text-[12px] leading-6 text-muted">
        这是一台电脑端宿主：它把本机的 agent 交给你的手机。手机端 DSH Mizar 用同一个账户登录后，
        就能看到并操控这台电脑，不需要扫码配对。
      </p>
      <p className="mt-2 text-[12px] leading-6 text-muted">
        接下来依次登录账户、选择要用的 Agent、准备模型，再决定这台电脑以后怎么启动。
        已经登录的账户、已安装的 Agent 和模型都会原样带过来。
      </p>
    </section>
  )
}

interface LoginStepProps {
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
              className="h-9 border border-ink bg-transparent px-3 text-[12px] text-fg outline-none"
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
              className="h-9 border border-ink bg-transparent px-3 text-[12px] text-fg outline-none"
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
            <div className="mt-3 border border-ink p-4">
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
                  className="h-9 flex-1 border border-ink bg-transparent px-3 font-mono text-[12px] tracking-wider text-fg outline-none"
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
        <div className="mt-4 border border-ink px-4 py-3">
          <div className="flex items-center gap-2 text-[12px] text-fg">
            <CheckIcon size={14} />
            已登录 {account.username}
          </div>
          <p className="mt-1 text-[12px] leading-5 text-muted">
            手机端用同一个账户登录后，就能在这台电脑的会话里远程操作。
          </p>
        </div>
      )}

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
                available
                  ? chosen
                    ? 'border-ink bg-ink text-bg'
                    : 'border-ink hover:border-2'
                  : 'cursor-not-allowed border-dashed border-ink',
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

      {selected.has('dsh') ? <DshPluginHint className="mt-3" /> : null}

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

      <div className="mt-4 divide-y divide-ink border border-ink">
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

/** How this computer behaves once it is closed and reopened. */
function PreferenceStep({ shell }: { shell: ShellSettingsState }) {
  return (
    <section>
      <h2 className="text-[24px] leading-8">这台电脑怎么启动</h2>
      <p className="mt-1.5 text-[12px] leading-5 text-muted">
        三项现在都是关的，随时可以在「设置 → 启动与窗口」里改。改动立即生效。
      </p>
      <div className="mt-4">
        <ShellSettingsList shell={shell} />
      </div>
    </section>
  )
}

interface DoneStepProps {
  account: AccountUser | null
  engines: EngineStatus[]
  selected: Set<EngineId>
  model: ModelState | null
  shell: ShellSettingsState
}

/** What is in place, so the window does not leave the guide quietly wrong. */
function DoneStep({ account, engines, selected, model, shell }: DoneStepProps) {
  const configured = AGENTS.filter(
    ({ engine }) =>
      selected.has(engine) ||
      engines.find((candidate) => candidate.engine === engine)?.configured === true,
  )
  const active = model?.active ?? null

  const preferences: string[] = []
  if (shell.settings !== null) {
    if (shell.settings.launch_at_login) preferences.push('开机静默启动')
    if (shell.settings.show_dock_icon) preferences.push('程序坞图标')
    if (shell.settings.tray_menu_only) preferences.push('托盘只开快捷菜单')
  }
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
      <div className="mt-4 divide-y divide-ink border border-ink">
        <DoneRow label="登录账户" value={account?.username ?? '未登录'} />
        <DoneRow
          label="Agent"
          value={configured.length === 0 ? '未选择' : configured.map(({ name }) => name).join('、')}
        />
        <DoneRow label="模型" value={active === null ? '未下载' : `Mizarllm ${active.version}`} />
        <DoneRow label="偏好" value={preferenceText} />
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
