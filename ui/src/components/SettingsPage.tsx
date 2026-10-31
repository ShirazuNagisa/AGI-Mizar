import { useEffect, useState } from 'react'
import type {
  AccountUser,
  Kernel,
  ModelCheck,
  ModelRuntime,
  ModelState,
  PairingView,
} from '../api'
import { THEMES, type ThemePreference } from '../theme'
import { formatGigabytes, formatModelVersion } from '../format'
import { ChevronIcon, Spinner } from '../icons'
import { Button } from './ui/button'
import { Progress } from './ui/progress'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select'
import { Toolbar } from './Toolbar'
import { ShellSettingsList } from './ShellSettingsList'
import { useShellSettings } from '../lib/use-shell-settings'
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
  account: AccountUser | null
  onlineDesktops: Set<string>
  serverConnected: boolean
  theme: ThemePreference
  onThemeChange: (theme: ThemePreference) => void
  kernel: Kernel
  model: ModelState | null
  /** Re-arm the first-run guide; the window then shows it instead of this page. */
  onRerunOnboarding: () => Promise<void>
}

/**
 * Everything that is not about one agent.
 *
 * Device lists are read-only and empty until the account service exists — the
 * page says so rather than drawing devices this computer cannot know about.
 * The pairing panel is the one temporary thing here: until accounts arrive it
 * is the only way a phone can attach, so it lives under 开发者.
 */
export function SettingsPage({
  version,
  account,
  onlineDesktops,
  serverConnected,
  theme,
  onThemeChange,
  kernel,
  model,
  onRerunOnboarding,
}: SettingsPageProps) {
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="设置" />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
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
                    serverConnected ? 'bg-ink' : 'mz-border',
                  )}
                />
                {serverConnected ? '已连接' : '未连接'}
              </span>
              <span className="text-[12px] text-muted">
                {account === null ? '登录后自动连接' : '中继由账户决定'}
              </span>
            </div>
          </Section>

          <Section title="模型">
            <ModelSection kernel={kernel} model={model} />
          </Section>

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

          <Section title="启动与窗口">
            <StartupSection />
          </Section>

          <Section title="关于">
            <AboutSection version={version} />
          </Section>

          <DeveloperPairing kernel={kernel} />

          <RerunGuide onRerun={onRerunOnboarding} />
        </div>
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
                    ? 'size-2 shrink-0 bg-ink'
                    : 'size-2 shrink-0 mz-border'
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
function StartupSection() {
  const shell = useShellSettings()
  return <ShellSettingsList shell={shell} />
}

/**
 * Version, build, and the one button that asks whether a newer app exists.
 *
 * The check goes through the shell's updater, which is the only thing that knows
 * how this app was installed; the last result is read on open so the row does
 * not start blank every time the page is visited.
 */
function AboutSection({ version }: { version: string }) {
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

/** The temporary way a phone attaches, until accounts replace it. */
function DeveloperPairing({ kernel }: { kernel: Kernel }) {
  const [open, setOpen] = useState(false)
  const [view, setView] = useState<PairingView | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function reveal() {
    setOpen(true)
    setError(null)
    try {
      setView(await kernel.send<PairingView>({ op: 'pairing', refresh: true }))
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : '配对码读取失败')
    }
  }

  return (
    <section className="mb-6">
      <button
        type="button"
        onClick={() => (open ? setOpen(false) : void reveal())}
        className="flex items-center gap-1.5 text-[12px] text-muted hover:text-fg"
      >
        <ChevronIcon size={13} className={open ? '' : '-rotate-90'} />
        开发者
      </button>
      {open ? (
        <div className="mt-2 mz-border p-4">
          <p className="text-[12px] leading-5 text-muted">
            账户体系上线前的临时入口：手机扫码配对这台电脑。上线后删除。
          </p>
          {error === null ? null : <p className="mt-2 text-[12px] text-muted">{error}</p>}
          {view === null ? null : (
            <div className="mt-3 flex items-center gap-4">
              {view.svg === null ? null : (
                <div
                  className="size-[132px] mz-border bg-white p-2"
                  dangerouslySetInnerHTML={{ __html: view.svg }}
                />
              )}
              <div>
                <div className="selectable text-[12px] font-semibold tracking-wide text-fg">
                  {view.code}
                </div>
                <p className="mt-1 text-[12px] text-muted">在 Mizar 手机端扫码或输入此码</p>
                <Button className="mt-2" size="sm" variant="outline" onClick={() => void reveal()}>
                  换一个码
                </Button>
              </div>
            </div>
          )}
        </div>
      ) : null}
    </section>
  )
}

/** The way back into the first-run guide, for anyone who skipped past a step. */
function RerunGuide({ onRerun }: { onRerun: () => Promise<void> }) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  async function rerun() {
    setBusy(true)
    setError(null)
    try {
      await onRerun()
    } catch (failure) {
      setError(failureMessage(failure, '无法重新运行新手引导'))
    } finally {
      // On success the window swaps to the guide and this page is gone.
      setBusy(false)
    }
  }

  return (
    <section className="mz-divide-t pt-5">
      <div className="flex items-start justify-between gap-6">
        <div className="min-w-0">
          <div className="text-[12px] text-fg">重新运行新手引导</div>
          <p className="mt-0.5 text-[12px] leading-5 text-muted">
            再走一遍欢迎、登录、Agent 与模型的设置；已经登录的账户和已安装的模型会原样保留。
          </p>
        </div>
        <Button size="sm" variant="outline" onClick={() => void rerun()} disabled={busy}>
          {busy ? <Spinner /> : null}
          重新运行
        </Button>
      </div>
      {error === null ? null : (
        <p className="mt-2 bg-ink px-2 py-1 text-[12px] leading-5 text-bg">{error}</p>
      )}
    </section>
  )
}
