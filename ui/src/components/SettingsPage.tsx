import { useEffect, useState } from 'react'
import type {
  AccountUser,
  DatasetState,
  DeviceStatus,
  Kernel,
  ModelCheck,
  ModelRuntime,
  ModelState,
  ServerMode,
  ServerProbeResult,
  ServerView,
} from '../api'
import { THEMES, type ThemePreference } from '../theme'
import { formatEta, formatGigabytes, formatModelVersion, formatSpeed } from '../format'
import { useDownloadSpeed } from '../lib/use-download-speed'
import { Spinner } from '../icons'
import { Button } from './ui/button'
import { Progress } from './ui/progress'
import { Switch } from './ui/switch'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select'
import { Toolbar } from './Toolbar'
import { ShellSettingsList } from './ShellSettingsList'
import { RemoteAgentSettings } from './RemoteAgentSettings'
import { LoginPage } from './LoginPage'
import { ServerChoice } from './ServerChoice'
import { useShellSettings } from '../lib/use-shell-settings'
import type { DesktopPlatform } from '../platform'
import {
  shellAvailable,
  shellFailure,
  updateCheck,
  updateInstall,
  updateState,
  type UpdateErrorCode,
  type UpdateStatus,
} from '../shell'
import { cn, failureMessage } from '../lib/utils'
import { probeFailureCopy, probeSuccessCopy } from '../lib/server'

interface SettingsPageProps {
  version: string
  /** Which desktop's words the shell switches use. */
  platform: DesktopPlatform
  account: AccountUser | null
  onlineDesktops: Set<string>
  /** This computer's phones and whether each one is reachable right now. */
  devices: DeviceStatus[]
  /** Whether any session is running, which is when live activity matters. */
  running: boolean
  serverConnected: boolean
  /** The relay this install talks to; the 服务器 tab edits it. */
  server: ServerView
  /** Called after a saved server choice so the window can reread and reconnect. */
  onServerChanged: () => Promise<void>
  theme: ThemePreference
  onThemeChange: (theme: ThemePreference) => void
  kernel: Kernel
  model: ModelState | null
  /** Whether this install shares turns to improve Mizarllm. */
  dataset: DatasetState
  accountBusy: boolean
  accountError: string | null
  onLogin: (identifier: string, password: string) => Promise<void>
  onLogout: () => Promise<void>
  onGithubStart: () => Promise<void>
  onGithubExchange: (handoff: string) => Promise<void>
  onAppleStart: () => Promise<void>
  onAppleExchange: (handoff: string) => Promise<void>
}

/**
 * Everything that is not about one agent.
 *
 * Device lists are read-only and empty until the account service exists — the
 * page says so rather than drawing devices this computer cannot know about.
 */
export function SettingsPage({
  version,
  platform,
  account,
  onlineDesktops,
  devices,
  running,
  serverConnected,
  server,
  onServerChanged,
  theme,
  onThemeChange,
  kernel,
  model,
  dataset,
  accountBusy,
  accountError,
  onLogin,
  onLogout,
  onGithubStart,
  onGithubExchange,
  onAppleStart,
  onAppleExchange,
}: SettingsPageProps) {
  // A self-hosted relay has no account service and no dataset endpoint, so the
  // account page would be a sign-in form that cannot work and the privacy page
  // would offer a switch that never uploads. Both tabs are hidden rather than
  // disabled: there is nothing on them this install can use.
  const selfHosted = server.mode === 'selfHosted'
  const tabs = SETTINGS_TABS.filter(
    (option) =>
      !selfHosted || (option.id !== 'account' && option.id !== 'privacy'),
  )
  const [tab, setTab] = useState<SettingsTabId>(() => (selfHosted ? 'server' : 'account'))
  useEffect(() => {
    if (!tabs.some((option) => option.id === tab)) setTab('server')
  }, [tabs, tab])
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="设置" />
      <SettingsTabs tabs={tabs} tab={tab} onChange={setTab} />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
          {tab === 'account' ? (
            <LoginPage
              embedded
              account={account}
              serverConnected={serverConnected}
              busy={accountBusy}
              error={accountError}
              onLogin={onLogin}
              onLogout={onLogout}
              onGithubStart={onGithubStart}
              onGithubExchange={onGithubExchange}
              onAppleStart={onAppleStart}
              onAppleExchange={onAppleExchange}
            />
          ) : null}

          {tab === 'server' ? (
            <ServerSection kernel={kernel} server={server} onChanged={onServerChanged} />
          ) : null}

          {tab === 'devices' ? (
            <>
              <Section title="设备">
                <Group title="AGI Mizar 桌面端">
                  <DeviceList
                    devices={
                      selfHosted
                        ? []
                        : (account?.devices.filter((device) => device.platform !== 'ios') ?? [])
                    }
                    online={onlineDesktops}
                    empty={
                      selfHosted
                        ? '自建服务器不提供账户设备列表'
                        : account === null
                          ? '登录后可查看同一账户下的桌面端'
                          : '这个账户下还没有桌面端'
                    }
                  />
                </Group>
                <Group title="Mizar 手机端">
                  <DeviceList
                    devices={
                      selfHosted
                        ? // The local roster knows the paired phones without an
                          // account; that is what a self-hosted install has.
                          devices.map((device, index) => ({
                            id: `local-${index}`,
                            name: device.name,
                            model: device.model ?? '',
                            platform: 'ios',
                            loginAt: 0,
                            lastSeenAt: device.lastSeenMs,
                            current: false,
                          }))
                        : (account?.devices.filter((device) => device.platform === 'ios') ?? [])
                    }
                    online={
                      selfHosted
                        ? new Set(
                            devices.flatMap((device, index) =>
                              device.online ? [`local-${index}`] : [],
                            ),
                          )
                        : phoneOnlineIds(account, devices, running)
                    }
                    empty={
                      selfHosted
                        ? '还没有配对的手机'
                        : account === null
                          ? '登录后可查看同一账户下的手机'
                          : '这个账户下还没有手机'
                    }
                  />
                </Group>
              </Section>

              <Section title="连接状态">
                <div className="flex items-center justify-between mz-border px-4 py-3">
                  <span className="flex items-center gap-2 text-[12px] text-fg">
                    <span
                      className={cn(
                        'size-2',
                        serverConnected ? 'bg-fg' : 'border border-fg',
                      )}
                    />
                    {serverConnected ? '已连接' : '未连接'}
                  </span>
                  <span className="text-[12px] text-muted">
                    {selfHosted
                      ? '自建服务器'
                      : account === null
                        ? '登录后自动连接'
                        : '中继由账户决定'}
                  </span>
                </div>
              </Section>
            </>
          ) : null}

          {tab === 'remote' ? <RemoteAgentSettings kernel={kernel} /> : null}

          {tab === 'approval' ? (
            <Section title="审批模型">
              <p className="text-[12px] leading-6 text-muted">
                审批模型是用于判断用户轮次是否结束的端侧模型，旨在实现更好的手机端会话查看体验。
              </p>
              <ModelSection kernel={kernel} model={model} />
            </Section>
          ) : null}

          {tab === 'appearance' ? (
            <Section title="外观">
              <div className="flex items-center justify-between mz-border px-4 py-2.5">
                <span className="text-[12px] text-fg">主题</span>
                <Select
                  value={theme}
                  onValueChange={(value) => onThemeChange(value as ThemePreference)}
                >
                  <SelectTrigger aria-label="主题">
                    <SelectValue />
                  </SelectTrigger>
                  <SelectContent>
                    {THEMES.map((option) => (
                      <SelectItem key={option.value} value={option.value}>
                        {option.label}
                      </SelectItem>
                    ))}
                  </SelectContent>
                </Select>
              </div>
            </Section>
          ) : null}

          {tab === 'startup' ? (
            <Section title="启动与窗口">
              <StartupSection platform={platform} />
            </Section>
          ) : null}

          {tab === 'privacy' ? (
            <Section title="数据与隐私">
              <PrivacySection kernel={kernel} dataset={dataset} />
            </Section>
          ) : null}

          {tab === 'about' ? (
            <Section title="关于">
              <AboutSection version={version} kernel={kernel} />
            </Section>
          ) : null}
        </div>
      </div>
    </div>
  )
}

const SETTINGS_TABS = [
  { id: 'account', label: '账户' },
  { id: 'server', label: '服务器' },
  { id: 'devices', label: '设备与连接' },
  { id: 'remote', label: '远程Agent' },
  { id: 'approval', label: '审批模型' },
  { id: 'appearance', label: '外观' },
  { id: 'startup', label: '启动与窗口' },
  { id: 'privacy', label: '数据与隐私' },
  { id: 'about', label: '关于' },
] as const

type SettingsTabId = (typeof SETTINGS_TABS)[number]['id']

function SettingsTabs({
  tabs,
  tab,
  onChange,
}: {
  tabs: readonly { id: SettingsTabId; label: string }[]
  tab: SettingsTabId
  onChange: (tab: SettingsTabId) => void
}) {
  return (
    <div className="mz-divide-b shrink-0 overflow-x-auto px-6">
      <div role="tablist" aria-label="设置页面" className="flex min-w-max items-center gap-1 py-2">
        {tabs.map((option) => (
          <button
            key={option.id}
            type="button"
            role="tab"
            aria-selected={tab === option.id}
            onClick={() => onChange(option.id)}
            className={cn(
              'h-7 shrink-0 px-2.5 text-[12px] outline-none transition-colors',
              tab === option.id ? 'bg-ink text-bg' : 'text-muted hover:text-fg',
            )}
          >
            {option.label}
          </button>
        ))}
      </div>
    </div>
  )
}

/**
 * Which relay this install talks to, and the check that makes a self-hosted one
 * trustworthy.
 *
 * The current choice is drawn first, because the common visit is "which server
 * am I on?" rather than "change it". Saving says what will happen to the tunnel
 * and to the remote agent: switching servers is a reconnect, and a phone or peer
 * that paired on the old origin may have to pair again.
 */
function ServerSection({
  kernel,
  server,
  onChanged,
}: {
  kernel: Kernel
  server: ServerView
  onChanged: () => Promise<void>
}) {
  const [mode, setMode] = useState<ServerMode>(server.mode)
  const [origin, setOrigin] = useState(server.origin)
  const [pin, setPin] = useState('')
  const [probing, setProbing] = useState(false)
  const [probe, setProbe] = useState<ServerProbeResult | null>(null)
  const [probeError, setProbeError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)

  // A saved choice refreshes the snapshot; the form follows it without losing
  // the unsaved PIN field (which the snapshot never carries).
  useEffect(() => {
    setMode(server.mode)
    setOrigin(server.origin)
  }, [server.mode, server.origin])

  const selfHosted = mode === 'selfHosted'
  const dirty = mode !== server.mode || origin.trim() !== server.origin || pin !== ''
  const switching = dirty && (mode !== server.mode || origin.trim() !== server.origin)
  const verified = !selfHosted || probe?.ok === true

  function forgetCheck() {
    setProbe(null)
    setProbeError(null)
    setError(null)
    setNotice(null)
  }

  function chooseMode(next: ServerMode) {
    setMode(next)
    forgetCheck()
  }

  function changeOrigin(next: string) {
    setOrigin(next)
    forgetCheck()
  }

  async function verify() {
    setProbing(true)
    setProbe(null)
    setProbeError(null)
    setError(null)
    try {
      const result = await kernel.send<ServerProbeResult>({
        op: 'server_probe',
        origin,
        pin: pin.trim(),
      })
      setProbe(result)
    } catch (failure) {
      setProbeError(failureMessage(failure, '验证失败'))
    } finally {
      setProbing(false)
    }
  }

  async function save() {
    setSaving(true)
    setError(null)
    setNotice(null)
    try {
      await kernel.send({
        op: 'set_server_prefs',
        mode,
        origin,
        // An empty field means "the PIN did not change"; the kernel keeps the
        // stored one.
        ...(pin.trim() === '' ? {} : { pin: pin.trim() }),
      })
      setPin('')
      setProbe(null)
      setNotice('已保存。正在重新连接服务器；远程 Agent 与手机可能需要重新配对。')
      await onChanged()
    } catch (failure) {
      setError(failureMessage(failure, '没有保存服务器设置'))
    } finally {
      setSaving(false)
    }
  }

  return (
    <Section title="服务器">
      <div className="mz-border">
        <div className="divide-y divide-ink">
          <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
            <span className="shrink-0 text-muted">当前服务器</span>
            <span className="truncate text-fg">
              {server.mode === 'selfHosted'
                ? `自建 · ${server.origin === '' ? '未填写域名' : server.origin}`
                : '官方'}
            </span>
          </div>

          <div className="grid grid-cols-2 gap-2 px-4 py-3">
            <ServerChoice
              title="官方服务器"
              detail="dshmizar-1.shirazu-nagisa.com"
              hint="账户、订阅与匿名数据共享"
              selected={mode === 'official'}
              onClick={() => chooseMode('official')}
            />
            <ServerChoice
              title="自建服务器"
              detail="你自己的 relay"
              hint="无需账户"
              selected={selfHosted}
              onClick={() => chooseMode('selfHosted')}
            />
          </div>

          {selfHosted ? (
            <div className="px-4 py-3">
              <div className="flex flex-col gap-2">
                <label className="text-[12px] text-muted" htmlFor="settings-server-origin">
                  服务器域名
                </label>
                <input
                  id="settings-server-origin"
                  value={origin}
                  onChange={(event) => changeOrigin(event.target.value)}
                  placeholder="relay.example.com"
                  autoCapitalize="off"
                  autoComplete="off"
                  spellCheck={false}
                  className="h-9 mz-border bg-transparent px-3 font-mono text-[12px] text-fg outline-none"
                />
                <label className="mt-2 text-[12px] text-muted" htmlFor="settings-server-pin">
                  服务器 PIN
                </label>
                <input
                  id="settings-server-pin"
                  type="password"
                  value={pin}
                  onChange={(event) => {
                    setPin(event.target.value)
                    forgetCheck()
                  }}
                  autoComplete="off"
                  placeholder={
                    server.hasPin ? '已保存 PIN，留空保持不变' : '服务器没有 PIN 可以留空'
                  }
                  className="h-9 mz-border bg-transparent px-3 font-mono text-[12px] tracking-wider text-fg outline-none"
                />
              </div>
              <div className="mt-3 flex items-center justify-between gap-4">
                <Button
                  size="sm"
                  variant="outline"
                  onClick={() => void verify()}
                  disabled={probing || origin.trim() === ''}
                >
                  {probing ? <Spinner /> : null}
                  验证连通性
                </Button>
                <span className="text-right text-[12px] leading-5">
                  {probing ? (
                    <span className="flex items-center gap-1.5 text-muted">
                      <Spinner size={12} />
                      正在验证…
                    </span>
                  ) : probeError !== null ? (
                    <span className="bg-ink px-1.5 text-bg">{probeError}</span>
                  ) : probe === null ? (
                    <span className="text-muted">修改域名或 PIN 后需要重新验证</span>
                  ) : probe.ok ? (
                    <span className="text-fg">{probeSuccessCopy(probe)}</span>
                  ) : (
                    <span className="bg-ink px-1.5 text-bg">{probeFailureCopy(probe)}</span>
                  )}
                </span>
              </div>
            </div>
          ) : null}

          <div className="flex items-start justify-between gap-4 px-4 py-2.5">
            <span className="text-[12px] leading-5 text-muted">
              {switching
                ? '切换服务器会重新连接，远程 Agent 与手机可能需要重新配对。'
                : selfHosted
                  ? '自建服务器：无需登录，不启用账户、订阅和匿名数据共享。'
                  : '官方服务器：账户、订阅和匿名数据共享按现有方式工作。'}
            </span>
            <Button
              size="sm"
              variant="primary"
              className="shrink-0"
              onClick={() => void save()}
              disabled={!dirty || saving || (selfHosted && !verified)}
            >
              {saving ? <Spinner /> : null}
              保存并切换
            </Button>
          </div>
        </div>
        {notice === null ? null : (
          <p className="mz-divide-t px-4 py-1.5 text-[12px] leading-5 text-muted">{notice}</p>
        )}
        {error === null ? null : (
          <p className="mz-divide-t bg-ink px-4 py-1.5 text-[12px] leading-5 text-bg">{error}</p>
        )}
      </div>
    </Section>
  )
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="mb-7">
      <h2 className="mb-2 flex items-center gap-2 text-[12px] tracking-[1px]">
        <span className="h-0.5 w-3 shrink-0 bg-ink" aria-hidden />
        {title}
      </h2>
      <div className="flex flex-col gap-2">{children}</div>
    </section>
  )
}

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="mz-border">
      <div className="mz-divide-b px-4 py-2 text-[12px] text-muted">{title}</div>
      <div className="px-4 py-3">{children}</div>
    </div>
  )
}

function Empty({ children }: { children: React.ReactNode }) {
  return <p className="text-[12px] text-muted">{children}</p>
}

/** The devices under the account, split by the group they are drawn in. */
/**
 * Which of the account's phones this computer can reach right now.
 *
 * The account list knows names and models, not sockets; the kernel knows
 * sockets and live-activity registrations, not account ids. The model is the
 * honest join (`iPhone17,1`), with the name as the fallback for a phone that
 * never reported one. A known model that disagrees blocks the name fallback:
 * two iPhones are both called "iPhone".
 */
function phoneOnlineIds(
  account: AccountUser | null,
  devices: DeviceStatus[],
  running: boolean,
): Set<string> {
  const reachable = devices.filter(
    (device) => device.online || (running && device.liveActivity === true),
  )
  const ids = new Set<string>()
  for (const device of account?.devices ?? []) {
    if (device.platform !== 'ios') continue
    const model = device.model?.toLowerCase()
    const name = device.name?.toLowerCase()
    const matches = reachable.some((local) => {
      const localModel = local.model?.toLowerCase()
      if (model !== undefined && localModel !== undefined) {
        return model === localModel
      }
      return name !== undefined && local.name.toLowerCase() === name
    })
    if (matches) ids.add(device.id)
  }
  return ids
}

function DeviceList({
  devices,
  online,
  empty,
}: {
  devices: import('../api').AccountDevice[]
  online: Set<string>
  empty: string
}) {
  if (devices.length === 0) return <Empty>{empty}</Empty>
  return (
    <div className="flex flex-col gap-2">
      {devices.map((device) => (
        <div key={device.id} className="flex items-center gap-3">
          <div className="min-w-0 flex-1">
            <div className="flex items-center gap-1.5 text-[12px] text-fg">
              <span
                className={
                  online.has(device.id)
                    ? 'size-2 shrink-0 bg-fg'
                    : 'size-2 shrink-0 border border-fg'
                }
              />
              <span className="truncate">{device.name}</span>
              {device.current ? (
                <span className="mz-border px-1 text-[12px]">本机</span>
              ) : null}
            </div>
            <div className="truncate text-[12px] text-muted">
              {device.model} · 最近 {relative(device.lastSeenAt)}
            </div>
          </div>
        </div>
      ))}
    </div>
  )
}

function relative(at: number): string {
  const minutes = Math.round((Date.now() - at) / 60_000)
  if (minutes < 1) return '刚刚'
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.round(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  return new Date(at).toLocaleDateString('zh-CN')
}

function AboutRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
      <span className="shrink-0 text-muted">{label}</span>
      <span className="selectable truncate text-fg">{value}</span>
    </div>
  )
}

/** One row of 关于 that opens a page in the browser. */
function AboutLink({
  kernel,
  label,
  value,
  url,
}: {
  kernel: Kernel
  label: string
  value: string
  url: string
}) {
  return (
    <button
      type="button"
      onClick={() => void kernel.send({ op: 'open_external', url })}
      className="flex w-full items-center justify-between gap-4 px-4 py-2.5 text-left text-[12px] outline-none hover:bg-surface-2 focus-visible:ring-2 focus-visible:ring-accent/60"
    >
      <span className="shrink-0 text-muted">{label}</span>
      <span className="selectable truncate text-fg">{value}</span>
    </button>
  )
}

const RUNTIME_LABELS: Record<ModelRuntime['state'], string> = {
  stopped: '未运行',
  starting: '启动中',
  ready: '就绪',
  failed: '启动失败',
}

const RUNTIME_DOTS: Record<ModelRuntime['state'], string> = {
  stopped: 'mz-border',
  starting: 'mz-border dither-50',
  ready: 'bg-ink',
  failed: 'bg-ink',
}

/**
 * The model: what is installed, whether the sidecar that runs it is up, and the
 * two things a person does here — ask for a newer version, and go back one.
 *
 * Progress does not live on this page: the kernel pushes `model_progress` and
 * the window hands the snapshot's download down, so closing the page mid-way
 * and coming back shows the same bar.
 */
function ModelSection({ kernel, model }: { kernel: Kernel; model: ModelState | null }) {
  const [check, setCheck] = useState<ModelCheck | null>(null)
  const [checking, setChecking] = useState(false)
  const [busy, setBusy] = useState<'download' | 'cancel' | 'rollback' | null>(null)
  const [error, setError] = useState<string | null>(null)

  const runtime: ModelRuntime = model?.runtime ?? { state: 'stopped', port: null, detail: null }
  const active = model?.active ?? null
  const previous = model?.previous ?? null
  const download = model?.download ?? null
  const latest = check?.latest ?? null
  const percent =
    download === null || download.total === 0
      ? 0
      : Math.round((download.done / download.total) * 100)
  const { speed, eta } = useDownloadSpeed(download)
  // The source is the difference between "slow" and "slow because this machine
  // is on the release CDN"; speed and the time left say the same thing for a
  // download that is moving at all.
  const moving = [
    download?.source ?? null,
    download !== null && download.state === 'running' && speed > 0 ? formatSpeed(speed) : null,
    formatEta(eta),
  ].filter((part): part is string => part !== null)

  async function checkLatest() {
    setChecking(true)
    setError(null)
    try {
      setCheck(await kernel.send<ModelCheck>({ op: 'model_check' }))
    } catch (failure) {
      setError(failureMessage(failure, '检查模型更新失败'))
    } finally {
      setChecking(false)
    }
  }

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

  async function rollback() {
    setBusy('rollback')
    setError(null)
    try {
      await kernel.send({ op: 'model_rollback' })
      setCheck(null)
    } catch (failure) {
      setError(failureMessage(failure, '回滚没有成功'))
    } finally {
      setBusy(null)
    }
  }

  return (
    <div className="mz-border">
      <div className="divide-y divide-ink">
        <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
          <span className="shrink-0 text-muted">模型</span>
          <span className="selectable truncate text-fg">
            {active === null ? '未安装' : `Mizarllm ${formatModelVersion(active.version)}`}
          </span>
        </div>

        <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
          <span className="shrink-0 text-muted">运行状态</span>
          <span className="flex items-center gap-2 text-fg">
            <span className={cn('size-2', RUNTIME_DOTS[runtime.state])} />
            {RUNTIME_LABELS[runtime.state]}
          </span>
        </div>

        {runtime.state === 'failed' && runtime.detail !== null ? (
          <p className="mz-divide-t bg-ink px-4 py-1.5 text-[12px] leading-5 text-bg">{runtime.detail}</p>
        ) : null}

        <div className="flex items-start justify-between gap-4 px-4 py-2.5">
          <div className="min-w-0 text-[12px] leading-5">
            <CheckSummary
              checking={checking}
              check={check}
              active={active === null ? null : active.version}
              downloading={download !== null}
            />
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {latest !== null && latest.newer && download === null ? (
              <Button
                size="sm"
                variant="primary"
                onClick={() => void startDownload(latest.version)}
                disabled={busy !== null}
              >
                {busy === 'download' ? <Spinner /> : null}
                下载模型
              </Button>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              onClick={() => void checkLatest()}
              disabled={checking || busy !== null || download !== null}
            >
              {checking ? <Spinner /> : null}
              检查模型更新
            </Button>
          </div>
        </div>

        {download === null ? null : (
          <div className="px-4 py-3">
            <div className="flex items-center justify-between gap-4 text-[12px]">
              <span className="text-fg">
                {download.state === 'verifying'
                  ? `正在校验 ${formatModelVersion(download.version)}`
                  : download.state === 'failed'
                    ? `下载失败 ${formatModelVersion(download.version)}`
                    : `正在下载 ${formatModelVersion(download.version)}`}
              </span>
              <span className="tabular shrink-0 text-muted">{percent}%</span>
            </div>
            <Progress
              className="mt-2"
              value={percent}
              label={`下载模型 ${formatModelVersion(download.version)}`}
            />
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
              <div className="mt-1 flex flex-wrap items-center gap-x-2 text-[12px] text-muted">
                {moving.map((part) => (
                  <span key={part} className="tabular">
                    {part}
                  </span>
                ))}
              </div>
            )}
            {download.error === null ? null : (
              <p className="mt-1.5 bg-ink px-2 py-1 text-[12px] leading-5 text-bg">{download.error}</p>
            )}
          </div>
        )}

        {previous === null ? null : (
          <div className="flex items-center justify-between gap-4 px-4 py-2.5 text-[12px]">
            <span className="text-muted">上一版 {previous.version}</span>
            <Button
              size="sm"
              variant="outline"
              onClick={() => void rollback()}
              disabled={busy !== null}
            >
              {busy === 'rollback' ? <Spinner /> : null}
              回滚到 {previous.version}
            </Button>
          </div>
        )}
      </div>

      {error === null ? null : (
        <p className="mz-divide-t bg-ink px-4 py-1.5 text-[12px] leading-5 text-bg">{error}</p>
      )}
    </div>
  )
}

/** The sentence beside 检查模型更新: what was found, or why nothing was. */
function CheckSummary({
  checking,
  check,
  active,
  downloading,
}: {
  checking: boolean
  check: ModelCheck | null
  active: string | null
  downloading: boolean
}) {
  if (checking) {
    return (
      <span className="flex items-center gap-1.5 text-muted">
        <Spinner size={12} />
        检查中…
      </span>
    )
  }
  if (check === null) {
    return (
      <span className="text-muted">
        {active === null ? '还没有安装模型。' : '可检查是否有新版本。'}
      </span>
    )
  }
  if (check.error !== null) return <span className="bg-ink px-1 text-bg">{check.error}</span>
  if (check.latest === null) return <span className="text-muted">没有取到最新版本。</span>
  if (check.latest.newer) {
    return (
      <span className="text-fg">
        可更新到 {check.latest.version}
        <span className="text-muted">
          {' '}
          · {formatGigabytes(check.latest.size_bytes)}
          {downloading ? ' · 正在下载' : ''}
        </span>
      </span>
    )
  }
  return <span className="text-muted">已是最新（{check.latest.version}）。</span>
}

/** The three switches the shell owns; each one takes effect on click. */
function StartupSection({ platform }: { platform: DesktopPlatform }) {
  const shell = useShellSettings()
  return <ShellSettingsList shell={shell} platform={platform} />
}

/**
 * The anonymous dataset switch.
 *
 * Deliberately quiet: no uploaded count and no progress bar. The operator asked
 * for a background feature, and a number here would either invite anxiety or
 * turn a privacy choice into a scoreboard. The switch and the sentence about
 * what cannot be undone are the whole surface.
 */
function PrivacySection({
  kernel,
  dataset,
}: {
  kernel: Kernel
  dataset: DatasetState
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [enabled, setEnabled] = useState(dataset.enabled)
  const [projects, setProjects] = useState<string[]>(dataset.excludedProjects)
  const [draft, setDraft] = useState('')

  useEffect(() => {
    setEnabled(dataset.enabled)
  }, [dataset.enabled])

  useEffect(() => {
    setProjects(dataset.excludedProjects)
  }, [dataset.excludedProjects])

  async function choose(next: boolean) {
    setBusy(true)
    setError(null)
    // The snapshot polls on a slow cadence; the switch has to answer the click
    // it just received.
    setEnabled(next)
    try {
      const answer = await kernel.send<DatasetState>({ op: 'dataset_share', enabled: next })
      setEnabled(answer.enabled)
    } catch (failure) {
      setEnabled(!next)
      setError(failureMessage(failure, '没有记住共享选择'))
    } finally {
      setBusy(false)
    }
  }

  async function exclude(next: string[]) {
    setBusy(true)
    setError(null)
    try {
      const answer = await kernel.send<DatasetState>({
        op: 'dataset_exclude',
        projects: next,
      })
      setProjects(answer.excludedProjects)
      setDraft('')
    } catch (failure) {
      setError(failureMessage(failure, '没有记住排除的项目'))
    } finally {
      setBusy(false)
    }
  }

  function addProject() {
    const name = draft.trim()
    if (name === '' || projects.some((project) => project.toLowerCase() === name.toLowerCase())) {
      setDraft('')
      return
    }
    void exclude([...projects, name])
  }

  return (
    <div className="mz-border">
      <div className="flex items-start justify-between gap-6 px-4 py-3">
        <div className="min-w-0">
          <div className="text-[12px] text-fg">匿名共享会话数据</div>
          <p className="mt-0.5 text-[12px] leading-5 text-muted">
            共享最近 12 个月的会话轮次（包括已归档的），上传前会在本机去掉账户、设备名、
            路径、邮箱和密钥。服务器不把数据与你的账户关联，不公开、不出售，只用于训练
            Mizarllm。
          </p>
        </div>
        <Switch
          className="mt-0.5"
          label="匿名共享会话数据"
          checked={enabled}
          disabled={busy}
          onCheckedChange={(next) => void choose(next)}
        />
      </div>
      <div className="mz-divide-t flex items-center justify-between gap-4 px-4 py-2">
        <span className="text-[12px] text-muted">
          {enabled ? '已开启：后台匿名分享中' : '已关闭：不再上传'}
          {dataset.since === null ? '' : ` · ${relative(dataset.since)}`}
        </span>
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
      <div className="mz-divide-t px-4 py-3">
        <div className="text-[12px] text-fg">永不共享的项目</div>
        <p className="mt-0.5 text-[12px] leading-5 text-muted">
          按项目名匹配会话的工作目录；这些项目的轮次不会上传。
        </p>
        {projects.length === 0 ? null : (
          <div className="mt-2 flex flex-wrap gap-1.5">
            {projects.map((project) => (
              <span key={project} className="mz-border flex items-center gap-1 px-2 py-0.5 text-[12px]">
                {project}
                <button
                  type="button"
                  aria-label={`不再排除 ${project}`}
                  disabled={busy}
                  onClick={() => void exclude(projects.filter((item) => item !== project))}
                  className="text-muted outline-none hover:text-fg"
                >
                  ×
                </button>
              </span>
            ))}
          </div>
        )}
        <div className="mt-2 flex gap-2">
          <input
            value={draft}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') {
                event.preventDefault()
                addProject()
              }
            }}
            placeholder="输入项目名，例如 my-secret-project"
            spellCheck={false}
            className="h-8 min-w-0 flex-1 mz-border bg-transparent px-2 text-[12px] text-fg outline-none"
          />
          <Button
            size="sm"
            variant="outline"
            disabled={busy || draft.trim() === ''}
            onClick={addProject}
          >
            添加
          </Button>
        </div>
      </div>
      <p className="mz-divide-t px-4 py-2 text-[12px] leading-5 text-muted">
        关闭会立即停止上传。已上传的数据已经无法与你的账户对应，不能单独删除。
      </p>
      {error === null ? null : (
        <p className="mz-divide-t bg-ink px-4 py-1.5 text-[12px] leading-5 text-bg">{error}</p>
      )}
    </div>
  )
}

/**
 * Version, build, and the one button that asks whether a newer app exists.
 *
 * The check goes through the shell's updater, which is the only thing that knows
 * how this app was installed; the last result is read on open so the row does
 * not start blank every time the page is visited. The updater answers with codes
 * and technical detail rather than sentences, so this page chooses the Chinese
 * words for each state — the updater's own error only ever shows as a tooltip.
 */
function AboutSection({ version, kernel }: { version: string; kernel: Kernel }) {
  const available = shellAvailable()
  const [status, setStatus] = useState<UpdateStatus | null>(null)
  const [checking, setChecking] = useState(false)
  const [installing, setInstalling] = useState(false)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!available) return
    let cancelled = false
    updateState()
      .then((value) => {
        if (!cancelled) setStatus(value)
      })
      .catch(() => {
        // No stored result is the normal case before the first check; the row
        // simply starts at 尚未检查.
      })
    return () => {
      cancelled = true
    }
  }, [available])

  async function check() {
    setChecking(true)
    setError(null)
    try {
      setStatus(await updateCheck())
    } catch (failure) {
      // The bridge's failures are technical English text. The row says what a
      // person can act on; the detail goes to the log for whoever debugs it.
      console.error('update check failed', shellFailure(failure))
      setError('检查更新失败，请稍后再试（详情见日志）')
    } finally {
      setChecking(false)
    }
  }

  async function install() {
    setInstalling(true)
    setError(null)
    try {
      await updateInstall()
    } catch (failure) {
      console.error('update install failed', shellFailure(failure))
      setError('安装更新失败，请稍后再试（详情见日志）')
    } finally {
      // A successful install relaunches the app; if the call returns instead,
      // the button must not stay grey for the rest of the session.
      setInstalling(false)
    }
  }

  const canInstall = status !== null && status.available && status.installable
  // A newer build exists but this channel publishes no signed installer, so the
  // page sends people to the release page instead of offering 下载并安装.
  const needsManualDownload = status !== null && status.available && !status.installable
  // The release page gets a button when the app cannot install by itself, and
  // when a failed check still knows where the release lives.
  const downloadUrl =
    status !== null &&
    status.release_url !== null &&
    (needsManualDownload || (!status.available && status.error_code !== null))
      ? status.release_url
      : null

  return (
    <div className="mz-border">
      <div className="divide-y divide-ink">
        <AboutRow label="版本" value={version === '' ? '—' : version} />
        <AboutRow label="构建" value={status === null || status.current === '' ? '—' : status.current} />
        <AboutLink
          kernel={kernel}
          label="查看源代码"
          value="GitHub ↗"
          url="https://github.com/ShirazuNagisa/AGI-Mizar"
        />
        <AboutLink
          kernel={kernel}
          label="查看文档站"
          value="文档 ↗"
          url="https://dsh-mizar.shirazu-nagisa.com/docs/"
        />
        <div className="flex items-start justify-between gap-4 px-4 py-2.5">
          <div className="min-w-0 text-[12px] leading-5">
            <UpdateSummary checking={checking} status={status} error={error} />
          </div>
          <div className="flex shrink-0 items-center gap-2">
            {canInstall ? (
              <Button
                size="sm"
                variant="primary"
                onClick={() => void install()}
                disabled={installing}
              >
                {installing ? <Spinner /> : null}
                下载并安装
              </Button>
            ) : null}
            {downloadUrl !== null ? (
              <Button
                size="sm"
                variant="outline"
                onClick={() => void kernel.send({ op: 'open_external', url: downloadUrl })}
              >
                打开下载页
              </Button>
            ) : null}
            <Button
              size="sm"
              variant="outline"
              onClick={() => void check()}
              disabled={!available || checking || installing}
            >
              {checking ? <Spinner /> : null}
              检查更新
            </Button>
          </div>
        </div>
      </div>
      {available ? null : (
        <p className="mz-divide-t px-4 py-2 text-[12px] leading-5 text-muted">
          检查更新由桌面窗口负责（仅桌面窗口可用）。
        </p>
      )}
    </div>
  )
}

/** What each failure code means to a person, in this app's words. */
const UPDATE_ERROR_COPY: Record<UpdateErrorCode, string> = {
  feed_missing: '更新通道暂未提供更新清单，请到发布页查看',
  network: '网络不可用，请稍后再试',
  unavailable: '当前构建无法检查更新',
  failed: '检查更新失败，请稍后再试',
}

/**
 * The row's status line: what the updater said, in this page's words.
 *
 * `error` is a page-level failure (the bridge itself threw), already Chinese;
 * `status.error_code` picks the copy for a check that answered with a failure.
 * The updater's technical `status.error` only ever shows as a tooltip.
 */
function UpdateSummary({
  checking,
  status,
  error,
}: {
  checking: boolean
  status: UpdateStatus | null
  error: string | null
}) {
  if (checking) {
    return (
      <span className="flex items-center gap-1.5 text-muted">
        <Spinner size={12} />
        检查中…
      </span>
    )
  }
  if (error !== null) return <span className="bg-ink px-1 text-bg">{error}</span>
  if (status === null) return <span className="text-muted">尚未检查</span>
  if (status.available) {
    if (status.installable) {
      return <span className="text-fg">可更新到 {status.latest ?? '新版本'}</span>
    }
    return (
      <span className="block">
        <span className="block text-fg">
          有新版 {status.latest ?? '新版本'}，需手动下载
        </span>
        <span className="block text-muted">当前渠道没有签名安装包，请到发布页下载。</span>
      </span>
    )
  }
  if (status.error_code !== null) {
    return (
      <span className="block" title={status.error ?? undefined}>
        {UPDATE_ERROR_COPY[status.error_code]}
      </span>
    )
  }
  return <span className="text-muted">已是最新</span>
}
