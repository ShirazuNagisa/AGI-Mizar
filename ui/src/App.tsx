import { useCallback, useEffect, useRef, useState } from 'react'
import {
  Kernel,
  type AccountUser,
  type ApiEvent,
  type DshPluginView,
  type EngineId,
  type StateSnapshot,
  type UsageOverview,
} from './api'
import { agentTab, type Page } from './navigation'
import { applyPlatform, platformOf } from './platform'
import { applyTheme, onSystemThemeChange, storeTheme, storedTheme, type ThemePreference } from './theme'
import { OledScope } from './lib/oled'
import { Sidebar } from './components/Sidebar'
import { HomePage } from './components/HomePage'
import { AgentPage } from './components/AgentPage'
import { DshPluginUpdateDialog } from './components/DshPluginUpdateDialog'
import { LoginPage } from './components/LoginPage'
import { Onboarding } from './components/Onboarding'
import { SettingsPage } from './components/SettingsPage'
import { TrayPanel } from './components/TrayPanel'

const kernel = new Kernel()

/** How often the dashboard rereads the logs while it is open. */
const USAGE_INTERVAL_MS = 60_000

export function App() {
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null)
  const [page, setPage] = useState<Page>({ kind: 'home' })
  const [usage, setUsage] = useState<UsageOverview | null>(null)
  const [usageLoading, setUsageLoading] = useState(true)
  const [usageError, setUsageError] = useState<string | null>(null)
  const [theme, setTheme] = useState<ThemePreference>(() => storedTheme())
  // What is painted right now, so the first render does not animate and a real
  // change does.
  const painted = useRef<ThemePreference | null>(null)
  const [account, setAccount] = useState<AccountUser | null>(null)
  const [accountBusy, setAccountBusy] = useState(false)
  const [accountError, setAccountError] = useState<string | null>(null)
  const [banner, setBanner] = useState<string | null>(null)
  const [serverConnected, setServerConnected] = useState(false)
  const [onlineDesktops, setOnlineDesktops] = useState<Set<string>>(new Set())
  // The DSH plugin update popup. The release it was dismissed for is kept in a
  // ref, not state: forgetting it on the next snapshot poll would reopen the
  // dialog every five seconds.
  const [pluginPrompt, setPluginPrompt] = useState<DshPluginView | null>(null)
  const pluginDismissed = useRef<string | null>(null)

  // The window's appearance is a class; following the system means listening for
  // macOS changing its mind while this window stays open.
  useEffect(() => {
    const animate = painted.current !== null && painted.current !== theme
    painted.current = theme
    applyTheme(theme, { animate })
    storeTheme(theme)
    if (theme !== 'system') return
    // The system changing under the window is not a choice being made here, so
    // it is not animated: the desktop around the window changes instantly too.
    return onSystemThemeChange(() => applyTheme(theme))
  }, [theme])

  // The tray cannot navigate a React page from outside the window, so it asks:
  // it shows the window and dispatches this event.
  useEffect(() => {
    const onNavigate = (event: Event) => {
      const detail = (event as CustomEvent<string>).detail
      if (detail === 'settings') setPage({ kind: 'settings' })
      if (detail === 'home') setPage({ kind: 'home' })
    }
    window.addEventListener('mizar:navigate', onNavigate)
    return () => window.removeEventListener('mizar:navigate', onNavigate)
  }, [])

  const refreshAccount = useCallback(async () => {
    try {
      const answer = await kernel.send<{ user: AccountUser | null } | null>({ op: 'account' })
      setAccount(answer?.user ?? null)
      setAccountError(null)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : '账户读取失败')
    }
  }, [])

  useEffect(() => {
    void refreshAccount()
  }, [refreshAccount])

  // Which computers of the account are reachable right now. Read when the
  // settings page is opened, and every half minute while it is on screen.
  useEffect(() => {
    if (page.kind !== 'settings' || account === null) {
      setOnlineDesktops(new Set())
      return
    }
    const read = () => {
      void kernel
        .send<{ desktops: { deviceId: string; online: boolean }[] }>({ op: 'account_desktops' })
        .then((answer) =>
          setOnlineDesktops(
            new Set(answer.desktops.filter((desk) => desk.online).map((desk) => desk.deviceId)),
          ),
        )
        .catch(() => setOnlineDesktops(new Set()))
    }
    read()
    const timer = window.setInterval(read, 30_000)
    return () => window.clearInterval(timer)
  }, [page.kind, account])

  // The tunnel comes and goes without the window asking; reading it on a slow
  // timer is enough for a status light.
  useEffect(() => {
    const read = () => {
      void kernel
        .send<{ connected: boolean }>({ op: 'relay_status' })
        .then((answer) => setServerConnected(answer.connected))
        .catch(() => setServerConnected(false))
    }
    read()
    const timer = window.setInterval(read, 5_000)
    return () => window.clearInterval(timer)
  }, [])

  useEffect(() => {
    let cancelled = false
    void kernel
      .state()
      .then((next) => {
        if (cancelled) return
        // The URL already carried the platform for the first paint; this keeps
        // `<html data-platform>` honest if a browser page was opened without it.
        applyPlatform(next.platform)
        setSnapshot(next)
      })
      .catch((error: unknown) => {
        if (!cancelled) setBanner(error instanceof Error ? error.message : '内核没有应答')
      })
    return () => {
      cancelled = true
    }
  }, [])

  // The lamp is drawn from a snapshot, and the plugin attaches seconds after
  // the window opens. A status event covers most of that; this covers the case
  // where the event arrives before the subscription does.
  useEffect(() => {
    const read = () => {
      void kernel
        .state()
        .then((next) => {
          applyPlatform(next.platform)
          setSnapshot(next)
        })
        .catch(() => {})
    }
    // The first minute is when the engines are still finding their sessions;
    // after that, a slow poll is only there to catch a machine going to sleep.
    const fast = window.setInterval(read, 5_000)
    const slow = window.setInterval(read, 30_000)
    const settle = window.setTimeout(() => window.clearInterval(fast), 60_000)
    return () => {
      window.clearInterval(fast)
      window.clearInterval(slow)
      window.clearTimeout(settle)
    }
  }, [])

  useEffect(() => {
    return kernel.subscribe(
      (event: ApiEvent) => {
        switch (event.type) {
          case 'engine_status':
            setSnapshot((current) => {
              if (current === null) return current
              const engines = current.engines.filter(
                (entry) => entry.engine !== event.status.engine,
              )
              engines.push(event.status)
              return { ...current, engines }
            })
            return
          case 'model_progress':
            // A download moves faster than the poll; the event carries the bar.
            setSnapshot((current) =>
              current === null
                ? current
                : { ...current, model: { ...current.model, download: event.download } },
            )
            return
          case 'model_changed':
            setSnapshot((current) =>
              current === null ? current : { ...current, model: event.model },
            )
            return
          case 'onboarding_changed':
            setSnapshot((current) =>
              current === null ? current : { ...current, onboarding: { done: event.done } },
            )
            return
          case 'dsh_plugin_changed':
            setSnapshot((current) =>
              current === null ? current : { ...current, dshPlugin: event.plugin },
            )
            return
          default:
            return
        }
      },
      () => setBanner('与内核的连接已断开'),
    )
  }, [])

  // The background check lands a few seconds after launch and then every six
  // hours. Only a release newer than the one on disk opens the dialog, and only
  // for a person who is actually using DSH: a machine that has never configured
  // DSH gets the button on the agent page, not a popup at every launch.
  useEffect(() => {
    const plugin = snapshot?.dshPlugin
    if (!plugin || !plugin.updateAvailable || snapshot?.onboarding?.done === false) {
      // Do not close an open dialog here: after a successful update the event
      // arrives before the person has read the result, and the dialog owns its
      // own "done" state. It closes when they dismiss it.
      return
    }
    const dsh = snapshot?.engines.find((engine) => engine.engine === 'dsh')
    if (plugin.installed === null && dsh?.configured !== true) return
    if (pluginDismissed.current === (plugin.latest ?? '')) return
    setPluginPrompt((current) => current ?? plugin)
  }, [snapshot?.dshPlugin, snapshot?.engines, snapshot?.onboarding?.done])

  const dismissPluginPrompt = useCallback(() => {
    pluginDismissed.current = pluginPrompt?.latest ?? ''
    setPluginPrompt(null)
  }, [pluginPrompt])

  const pluginUpdated = useCallback((plugin: DshPluginView) => {
    setSnapshot((current) =>
      current === null ? current : { ...current, dshPlugin: plugin },
    )
  }, [])

  const refreshUsage = useCallback(async () => {
    setUsageLoading(true)
    try {
      const next = await kernel.send<UsageOverview>({ op: 'usage_overview' })
      setUsage(next)
      setUsageError(null)
    } catch (error) {
      setUsageError(error instanceof Error ? error.message : '统计读取失败')
    } finally {
      setUsageLoading(false)
    }
  }, [])

  // The dashboard is the first screen, so it asks before the engines have
  // finished listing their sessions and would otherwise show the first fold
  // only. When the counts change, ask again — debounced, because a listing of
  // three hundred sessions arrives one event at a time.
  const sessionCounts = (snapshot?.engines ?? [])
    .map((engine) => `${engine.engine}:${engine.sessions}`)
    .join(',')
  useEffect(() => {
    if (page.kind !== 'home') return
    const timer = window.setTimeout(() => void refreshUsage(), 1500)
    return () => window.clearTimeout(timer)
  }, [sessionCounts, page.kind, refreshUsage])

  // Read once on opening the dashboard, keep it current while it is on screen,
  // and take a fresh reading when the window comes back to the front.
  useEffect(() => {
    if (page.kind !== 'home') return
    void refreshUsage()
    const timer = window.setInterval(() => void refreshUsage(), USAGE_INTERVAL_MS)
    // The first fold takes a while on a host with hundreds of sessions; asking
    // every thirty seconds until it settles is what turns a partial folder into
    // the real number without a reload.
    const warmup = window.setInterval(() => void refreshUsage(), 30_000)
    const settle = window.setTimeout(() => window.clearInterval(warmup), 180_000)
    const onFocus = () => void refreshUsage()
    window.addEventListener('focus', onFocus)
    return () => {
      window.clearInterval(timer)
      window.clearInterval(warmup)
      window.clearTimeout(settle)
      window.removeEventListener('focus', onFocus)
    }
  }, [page.kind, refreshUsage])

  const refreshState = useCallback(async () => {
    const next = await kernel.state()
    applyPlatform(next.platform)
    setSnapshot(next)
  }, [])

  const reconnect = useCallback(
    async (_engine: EngineId) => {
      // Until the engines expose a real reconnect, a fresh snapshot is the honest
      // thing this window can do: it re-asks the kernel what it can reach now.
      try {
        await refreshState()
        setBanner(null)
      } catch (error) {
        setBanner(error instanceof Error ? error.message : '内核没有应答')
      }
    },
    [refreshState],
  )

  // The guide is driven by the snapshot, so re-arming it is one command and one
  // reread: the window then shows the guide because `done` is false again.
  const rerunOnboarding = useCallback(async () => {
    await kernel.send({ op: 'onboarding_reset' })
    await refreshState()
    setPage({ kind: 'home' })
  }, [refreshState])

  const finishOnboarding = useCallback(async () => {
    await refreshState()
    setPage({ kind: 'home' })
  }, [refreshState])

  const chooseTheme = (next: ThemePreference) => {
    setTheme(next)
    // The kernel remembers it for the next launch; the local copy only smooths
    // over a reload of this same window.
    void kernel.send({ op: 'set_theme', theme: next }).catch(() => {})
  }

  const login = useCallback(
    async (identifier: string, password: string) => {
      setAccountBusy(true)
      setAccountError(null)
      try {
        const answer = await kernel.send<{ user: AccountUser }>({
          op: 'account_login',
          identifier,
          password,
        })
        setAccount(answer.user)
      } catch (error) {
        setAccountError(error instanceof Error ? error.message : '登录失败')
      } finally {
        setAccountBusy(false)
      }
    },
    [],
  )

  const githubStart = useCallback(async () => {
    setAccountBusy(true)
    setAccountError(null)
    try {
      const answer = await kernel.send<{ authorizeUrl: string; state: string }>({
        op: 'account_github_start',
      })
      await kernel.send({ op: 'open_external', url: answer.authorizeUrl })
      // The browser cannot call into this window, so the kernel waits on the
      // relay for the round-trip to finish and signs in by itself.
      const done = await kernel.send<{ user: AccountUser }>({
        op: 'account_github_await',
        state: answer.state,
      })
      setAccount(done.user)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : '无法打开 GitHub')
    } finally {
      setAccountBusy(false)
    }
  }, [])

  const githubExchange = useCallback(async (handoff: string) => {
    setAccountBusy(true)
    setAccountError(null)
    try {
      const answer = await kernel.send<{ user: AccountUser }>({
        op: 'account_github_exchange',
        handoff,
      })
      setAccount(answer.user)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : 'GitHub 登录失败')
    } finally {
      setAccountBusy(false)
    }
  }, [])

  const appleStart = useCallback(async () => {
    setAccountBusy(true)
    setAccountError(null)
    try {
      const answer = await kernel.send<{ authorizeUrl: string; state: string }>({
        op: 'account_apple_start',
      })
      await kernel.send({ op: 'open_external', url: answer.authorizeUrl })
      const done = await kernel.send<{ user: AccountUser }>({
        op: 'account_apple_await',
        state: answer.state,
      })
      setAccount(done.user)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : '无法打开 Apple 登录')
    } finally {
      setAccountBusy(false)
    }
  }, [])

  const appleExchange = useCallback(async (handoff: string) => {
    setAccountBusy(true)
    setAccountError(null)
    try {
      const answer = await kernel.send<{ user: AccountUser }>({
        op: 'account_apple_exchange',
        handoff,
      })
      setAccount(answer.user)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : 'Apple 登录失败')
    } finally {
      setAccountBusy(false)
    }
  }, [])

  const logout = useCallback(async () => {
    setAccountBusy(true)
    try {
      await kernel.send({ op: 'account_logout' })
      setAccount(null)
    } catch (error) {
      setAccountError(error instanceof Error ? error.message : '退出失败')
    } finally {
      setAccountBusy(false)
    }
  }, [])

  // The platform the shell reported, for the copy that differs between the two
  // desktops. The pre-render attribute already answered it; this is the answer
  // the kernel owns, and it updates if a browser page was opened without one.
  const platform = platformOf(snapshot?.platform)

  // The tray page is a window of its own, loading this same bundle. It draws
  // nothing but the three lists.
  if (new URLSearchParams(window.location.search).get('panel') === 'tray') {
    return <TrayPanel kernel={kernel} />
  }

  // The guide owns the whole window while it runs: no sidebar, no toolbar, and
  // no route around it — the snapshot says whether it is still owed.
  if (snapshot !== null && snapshot.onboarding?.done === false) {
    return (
      <OledScope oled={theme === 'oled'}>
      <Onboarding
        kernel={kernel}
        account={account}
        serverConnected={serverConnected}
        engines={snapshot.engines}
        model={snapshot.model ?? null}
        dataset={snapshot.dataset ?? { enabled: false, policyVersion: 0, since: null, excludedProjects: [] }}
        platform={platform}
        accountBusy={accountBusy}
        accountError={accountError}
        onLogin={login}
        onGithubStart={githubStart}
        onGithubExchange={githubExchange}
        onAppleStart={appleStart}
        onAppleExchange={appleExchange}
        onRefreshState={refreshState}
        onFinished={finishOnboarding}
      />
      </OledScope>
    )
  }

  return (
    <OledScope oled={theme === 'oled'}>
    <div className="flex h-full w-full overflow-hidden bg-bg text-fg">
      <Sidebar
        engines={snapshot?.engines ?? []}
        page={page}
        serverConnected={serverConnected}
        onNavigate={setPage}
        onRerunOnboarding={rerunOnboarding}
      />

      <main className="flex min-w-0 flex-1 flex-col">
        {banner === null ? null : (
          <div className="border-b border-ink bg-surface px-6 py-1.5 text-[12px] text-muted">
            {banner}
          </div>
        )}
        {page.kind === 'home' ? (
          <HomePage overview={usage} loading={usageLoading} error={usageError} />
        ) : null}
        {page.kind === 'agent' ? (
          (() => {
            const tab = agentTab(page.tab)
            return (
              <AgentPage
                tab={tab}
                status={snapshot?.engines.find((entry) => entry.engine === tab.engine)}
                kernel={kernel}
                plugin={snapshot?.dshPlugin ?? null}
                sessions={snapshot?.sessions ?? []}
                onReconnect={reconnect}
              />
            )
          })()
        ) : null}
        {page.kind === 'login' ? (
          <LoginPage
            account={account}
            serverConnected={serverConnected}
            busy={accountBusy}
            error={accountError}
            onLogin={login}
            onLogout={logout}
            onGithubStart={githubStart}
            onGithubExchange={githubExchange}
            onAppleStart={appleStart}
            onAppleExchange={appleExchange}
          />
        ) : null}
        {page.kind === 'settings' ? (
          <SettingsPage
            version={snapshot?.version ?? ''}
            platform={platform}
            account={account}
            onlineDesktops={onlineDesktops}
            serverConnected={serverConnected}
            theme={theme}
            onThemeChange={chooseTheme}
            kernel={kernel}
            model={snapshot?.model ?? null}
            dataset={snapshot?.dataset ?? { enabled: false, policyVersion: 0, since: null, excludedProjects: [] }}
            accountBusy={accountBusy}
            accountError={accountError}
            onLogin={login}
            onLogout={logout}
            onGithubStart={githubStart}
            onGithubExchange={githubExchange}
            onAppleStart={appleStart}
            onAppleExchange={appleExchange}
          />
        ) : null}
      </main>
    </div>
    {pluginPrompt === null ? null : (
      <DshPluginUpdateDialog
        plugin={pluginPrompt}
        kernel={kernel}
        onDismiss={dismissPluginPrompt}
        onUpdated={pluginUpdated}
      />
    )}
    </OledScope>
  )
}
