import { useCallback, useEffect, useState } from 'react'
import { Kernel, type ApiEvent, type EngineId, type StateSnapshot, type UsageOverview } from './api'
import { AGENTS, type Page } from './navigation'
import { applyTheme, onSystemThemeChange, storeTheme, storedTheme, type ThemePreference } from './theme'
import { Sidebar } from './components/Sidebar'
import { HomePage } from './components/HomePage'
import { AgentPage } from './components/AgentPage'
import { LoginPage } from './components/LoginPage'
import { SettingsPage } from './components/SettingsPage'

const kernel = new Kernel()

/** How often the dashboard rereads the logs while it is open. */
const USAGE_INTERVAL_MS = 60_000

/** Whether this window thinks it is signed in. Placeholder until accounts exist. */
const ACCOUNT_KEY = 'agi-mizar.account'

export function App() {
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null)
  const [page, setPage] = useState<Page>({ kind: 'home' })
  const [usage, setUsage] = useState<UsageOverview | null>(null)
  const [usageLoading, setUsageLoading] = useState(true)
  const [usageError, setUsageError] = useState<string | null>(null)
  const [theme, setTheme] = useState<ThemePreference>(() => storedTheme())
  const [loggedIn, setLoggedIn] = useState(() => localStorage.getItem(ACCOUNT_KEY) === '1')
  const [banner, setBanner] = useState<string | null>(null)

  // The window's appearance is a class; following the system means listening for
  // macOS changing its mind while this window stays open.
  useEffect(() => {
    applyTheme(theme)
    storeTheme(theme)
    if (theme !== 'system') return
    return onSystemThemeChange(() => applyTheme(theme))
  }, [theme])

  useEffect(() => {
    let cancelled = false
    void kernel
      .state()
      .then((next) => {
        if (!cancelled) setSnapshot(next)
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
      void kernel.state().then(setSnapshot).catch(() => {})
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
        if (event.type !== 'engine_status') return
        setSnapshot((current) => {
          if (current === null) return current
          const engines = current.engines.filter(
            (entry) => entry.engine !== event.status.engine,
          )
          engines.push(event.status)
          return { ...current, engines }
        })
      },
      () => setBanner('与内核的连接已断开'),
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

  const reconnect = useCallback(async (_engine: EngineId) => {
    // Until the engines expose a real reconnect, a fresh snapshot is the honest
    // thing this window can do: it re-asks the kernel what it can reach now.
    try {
      setSnapshot(await kernel.state())
      setBanner(null)
    } catch (error) {
      setBanner(error instanceof Error ? error.message : '内核没有应答')
    }
  }, [])

  const setAccount = (next: boolean) => {
    setLoggedIn(next)
    if (next) localStorage.setItem(ACCOUNT_KEY, '1')
    else localStorage.removeItem(ACCOUNT_KEY)
  }

  return (
    <div className="flex h-full w-full overflow-hidden bg-bg text-fg">
      <Sidebar
        engines={snapshot?.engines ?? []}
        page={page}
        serverConnected={false}
        loggedIn={loggedIn}
        onNavigate={setPage}
      />

      <main className="flex min-w-0 flex-1 flex-col">
        {banner === null ? null : (
          <div className="border-b border-border bg-surface px-6 py-1.5 text-[11px] text-muted">
            {banner}
          </div>
        )}
        {page.kind === 'home' ? (
          <HomePage overview={usage} loading={usageLoading} error={usageError} />
        ) : null}
        {page.kind === 'agent' ? (
          <AgentPage
            engine={page.engine}
            name={AGENTS.find((agent) => agent.engine === page.engine)?.name ?? page.engine}
            status={snapshot?.engines.find((entry) => entry.engine === page.engine)}
            kernel={kernel}
            onReconnect={reconnect}
          />
        ) : null}
        {page.kind === 'login' ? (
          <LoginPage
            loggedIn={loggedIn}
            serverConnected={false}
            onLogin={() => setAccount(true)}
            onLogout={() => setAccount(false)}
          />
        ) : null}
        {page.kind === 'settings' ? (
          <SettingsPage
            version={snapshot?.version ?? ''}
            loggedIn={loggedIn}
            serverConnected={false}
            theme={theme}
            onThemeChange={setTheme}
            kernel={kernel}
          />
        ) : null}
      </main>
    </div>
  )
}
