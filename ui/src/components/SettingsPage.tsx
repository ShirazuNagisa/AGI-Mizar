import { useEffect, useState } from 'react'
import type {
  AccountUser,
  DatasetState,
  Kernel,
  ModelCheck,
  ModelRuntime,
  ModelState,
} from '../api'
import { THEMES, type ThemePreference } from '../theme'
import { formatGigabytes, formatModelVersion } from '../format'
import { Spinner } from '../icons'
import { Button } from './ui/button'
import { Progress } from './ui/progress'
import { Switch } from './ui/switch'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select'
import { Toolbar } from './Toolbar'
import { ShellSettingsList } from './ShellSettingsList'
import { RemoteAgentSettings } from './RemoteAgentSettings'
import { LoginPage } from './LoginPage'
import { useShellSettings } from '../lib/use-shell-settings'
import type { DesktopPlatform } from '../platform'
import {
  shellAvailable,
  shellFailure,
  updateCheck,
  updateInstall,
  updateState,
  type UpdateStatus,
} from '../shell'
import { cn, failureMessage } from '../lib/utils'

interface SettingsPageProps {
  version: string
  /** Which desktop's words the shell switches use. */
  platform: DesktopPlatform
  account: AccountUser | null
  onlineDesktops: Set<string>
  serverConnected: boolean
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
  serverConnected,
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
  const [tab, setTab] = useState<SettingsTabId>('account')
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="设置" />
      <SettingsTabs tab={tab} onChange={setTab} />
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

          {tab === 'devices' ? (
            <>
              <Section title="设备">
                <Group title="AGI Mizar 桌面端">
                  <DeviceList
                    devices={account?.devices.filter((device) => device.platform !== 'ios') ?? []}
                    online={onlineDesktops}
                    empty={account === null ? '登录后可查看同一账户下的桌面端' : '这个账户下还没有桌面端'}
                  />
                </Group>
                <Group title="Mizar 手机端">
                  <DeviceList
                    devices={account?.devices.filter((device) => device.platform === 'ios') ?? []}
                    online={new Set()}
                    empty={account === null ? '登录后可查看同一账户下的手机' : '这个账户下还没有手机'}
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
                    {account === null ? '登录后自动连接' : '中继由账户决定'}
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
  tab,
  onChange,
}: {
  tab: SettingsTabId
  onChange: (tab: SettingsTabId) => void
}) {
  return (
    <div className="mz-divide-b shrink-0 overflow-x-auto px-6">
      <div role="tablist" aria-label="设置页面" className="flex min-w-max items-center gap-1 py-2">
        {SETTINGS_TABS.map((option) => (
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
 * not start blank every time the page is visited.
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
      setError(shellFailure(failure))
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
      setError(shellFailure(failure))
    } finally {
      // A successful install relaunches the app; if the call returns instead,
      // the button must not stay grey for the rest of the session.
      setInstalling(false)
    }
  }

  const newer = status !== null && status.available && status.latest !== null

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
            {newer && status.latest !== null ? (
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
  if (status.error !== null) return <span className="bg-ink px-1 text-bg">{status.error}</span>
  if (status.available) {
    return <span className="text-fg">可更新到 {status.latest ?? '新版本'}</span>
  }
  return <span className="text-muted">已是最新</span>
}
