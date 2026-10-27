import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  Kernel,
  type ActivityEntry,
  type ApiEvent,
  type DeliveredFile,
  type EngineId,
  type Interaction,
  type ModelInfo,
  type PermissionPreset,
  type Session,
  type SessionStats,
  type StateSnapshot,
  type TurnRecord,
} from './api'
import { SessionList } from './components/SessionList'
import { Conversation } from './components/Conversation'
import { Inspector } from './components/Inspector'
import { Pairing } from './components/Pairing'

const kernel = new Kernel()

type Filter = 'all' | EngineId

export function App() {
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null)
  const [selected, setSelected] = useState<string | null>(null)
  const [records, setRecords] = useState<TurnRecord[]>([])
  const [filter, setFilter] = useState<Filter>('all')
  const [inspector, setInspector] = useState(true)
  const [error, setError] = useState<string | null>(null)
  const [live, setLive] = useState(false)
  const [pairing, setPairing] = useState(false)
  const [stats, setStats] = useState<SessionStats | null>(null)
  const [activity, setActivity] = useState<ActivityEntry[]>([])
  const [deliverables, setDeliverables] = useState<DeliveredFile[]>([])
  const [models, setModels] = useState<ModelInfo[]>([])
  const [permissions, setPermissions] = useState<PermissionPreset[]>([])
  const selectedRef = useRef<string | null>(null)

  selectedRef.current = selected

  const applyEvent = useCallback((event: ApiEvent) => {
    setSnapshot((current) => {
      if (current === null) return current
      switch (event.type) {
        case 'session_updated': {
          const sessions = current.sessions.filter((session) => session.key !== event.session.key)
          sessions.push(event.session)
          sessions.sort((a, b) => b.updated_at - a.updated_at)
          return { ...current, sessions }
        }
        case 'session_removed':
          return { ...current, sessions: current.sessions.filter((s) => s.key !== event.session) }
        case 'interaction_requested':
          return {
            ...current,
            interactions: [...current.interactions.filter((i) => i.id !== event.interaction.id), event.interaction],
          }
        case 'interaction_resolved':
          return { ...current, interactions: current.interactions.filter((i) => i.id !== event.id) }
        case 'engine_status': {
          const engines = current.engines.map((engine) =>
            engine.engine === event.status.engine ? event.status : engine,
          )
          return { ...current, engines }
        }
        default:
          return current
      }
    })

    if (event.type === 'record_appended' && event.session === selectedRef.current) {
      setRecords((current) =>
        current.some((record) => record.seq === event.record.seq) ? current : [...current, event.record],
      )
    }
  }, [])

  // Load once, then keep up with the stream.
  useEffect(() => {
    let cancelled = false
    kernel
      .state()
      .then((state) => {
        if (cancelled) return
        setSnapshot(state)
        setSelected((current) => current ?? state.sessions[0]?.key ?? null)
      })
      .catch((cause: Error) => setError(cause.message))

    const close = kernel.subscribe(
      (event) => {
        setLive(true)
        applyEvent(event)
      },
      () => setLive(false),
    )
    return () => {
      cancelled = true
      close()
    }
  }, [applyEvent])

  // Re-read the transcript whenever the selection changes. Records arrive live
  // afterwards, so this is the only full read.
  useEffect(() => {
    if (selected === null) {
      setRecords([])
      return
    }
    let cancelled = false
    kernel
      .send<TurnRecord[]>({ op: 'messages', session: selected, limit: 400 })
      .then((value) => {
        if (!cancelled) setRecords(value)
      })
      .catch((cause: Error) => {
        if (!cancelled) setError(cause.message)
      })
    return () => {
      cancelled = true
    }
  }, [selected])

  const running = active_is_running(snapshot, selected)

  useEffect(() => {
    if (selected === null) {
      setStats(null)
      setActivity([])
      setDeliverables([])
      setModels([])
      setPermissions([])
      return
    }
    const engine = selected.split(':')[0] as EngineId
    let cancelled = false
    void kernel.send<SessionStats>({ op: 'stats', session: selected }).then((value) => {
      if (!cancelled) setStats(value)
    }).catch(() => {})
    void kernel.send<DeliveredFile[]>({ op: 'deliverables', session: selected }).then((value) => {
      if (!cancelled) setDeliverables(value)
    }).catch(() => {})
    void kernel.send<ModelInfo[]>({ op: 'models', engine }).then((value) => {
      if (!cancelled) setModels(value)
    }).catch(() => setModels([]))
    void kernel.send<PermissionPreset[]>({ op: 'permissions', engine }).then((value) => {
      if (!cancelled) setPermissions(value)
    }).catch(() => setPermissions([]))
    return () => {
      cancelled = true
    }
  }, [selected])

  useEffect(() => {
    if (selected === null || !running) {
      setActivity([])
      return
    }
    let cancelled = false
    const poll = () => {
      void kernel
        .send<ActivityEntry[]>({ op: 'activity', session: selected, limit: 120 })
        .then((value) => {
          if (!cancelled) setActivity(value)
        })
        .catch(() => {})
    }
    poll()
    const timer = setInterval(poll, 1500)
    return () => {
      cancelled = true
      clearInterval(timer)
    }
  }, [selected, running])

  const sessions = useMemo(() => {
    if (snapshot === null) return []
    return snapshot.sessions.filter(
      (session) => filter === 'all' || session.key.startsWith(`${filter}:`),
    )
  }, [snapshot, filter])

  const active = sessions.find((session) => session.key === selected) ?? null
  const waiting = useMemo(() => snapshot?.interactions ?? [], [snapshot])

  const act = useCallback(async (run: () => Promise<unknown>) => {
    try {
      await run()
      setError(null)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }, [])

  if (snapshot === null) {
    return (
      <div className="shell">
        <header className="toolbar">
          <span className="title">AGI Mizar</span>
        </header>
        <div className="main" data-inspector="closed">
          <div className="empty" style={{ gridColumn: '1 / -1' }}>
            <strong>{error ?? 'Connecting to the kernel…'}</strong>
            {error === null ? <span>It starts with the window; this takes a moment.</span> : null}
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="shell">
      <header className="toolbar">
        <span className="title">AGI Mizar</span>
        <div className="engines">
          <EngineTab label="All" count={snapshot.sessions.length} active={filter === 'all'} onClick={() => setFilter('all')} />
          {snapshot.engines.map((engine) => (
            <EngineTab
              key={engine.engine}
              label={labelOf(engine.engine)}
              count={engine.sessions}
              offline={!engine.connected}
              active={filter === engine.engine}
              onClick={() => setFilter(engine.engine)}
            />
          ))}
        </div>
        <span className="spacer" />
        <span className="hint">{live ? 'live' : 'reconnecting…'}</span>
        <button
          className="btn ghost"
          onClick={() =>
            act(async () => {
              const cwd = active?.cwd ?? undefined
              const created = await kernel.send<string>({
                op: 'create',
                engine: (filter === 'all' ? 'dsh' : filter) as EngineId,
                cwd,
              })
              setSelected(created)
            })
          }
        >
          New session
        </button>
        <button className="btn ghost" onClick={() => setPairing(true)}>
          Pair a phone
        </button>
        <button className="btn ghost" onClick={() => setInspector((open) => !open)}>
          {inspector ? 'Hide details' : 'Details'}
        </button>
      </header>

      <div className="main" data-inspector={inspector ? 'open' : 'closed'}>
        <SessionList
          sessions={sessions}
          selected={selected}
          onSelect={setSelected}
          approvals={waiting}
        />

        <section className="panel">
          {active === null ? (
            <div className="empty">
              <strong>No session selected</strong>
              <span>Pick one on the left, or start one from an engine's own window.</span>
            </div>
          ) : (
            <Conversation
              session={active}
              records={records}
              onSend={(text) => act(() => kernel.send({ op: 'prompt', session: active.key, text }))}
              onStop={() => act(() => kernel.send({ op: 'cancel', session: active.key }))}
            />
          )}
        </section>

        {inspector ? (
          <aside className="panel">
            <Inspector
              session={active}
              stats={stats}
              activity={activity}
              deliverables={deliverables}
              models={models}
              permissions={permissions}
              onModel={(id) => {
                if (active === null) return
                act(() => kernel.send({ op: 'set_model', session: active.key, model: id }))
              }}
              onPermission={(id) => {
                if (active === null) return
                act(() => kernel.send({ op: 'set_permission', session: active.key, preset: id }))
              }}
              onRename={() => {
                if (active === null) return
                const title = window.prompt('Rename this session', active.title ?? '')
                if (title !== null && title.trim() !== '') {
                  act(() => kernel.send({ op: 'rename', session: active.key, title: title.trim() }))
                }
              }}
              onArchive={() => {
                if (active === null) return
                if (window.confirm('Archive this session? It stays on the computer.')) {
                  act(() => kernel.send({ op: 'archive', session: active.key }))
                }
              }}
              interactions={waiting.filter(
                (entry) => entry.session === active?.key || entry.session === null,
              )}
              onAnswer={(id, outcome, answers) =>
                act(() => kernel.send({ op: 'answer', id, outcome, answers }))
              }
              version={snapshot.version}
              engines={snapshot.engines}
            />
          </aside>
        ) : null}
      </div>

      {pairing ? (
        <Pairing
          onClose={() => setPairing(false)}
          request={(refresh) => kernel.send({ op: 'pairing', refresh })}
        />
      ) : null}

      {error !== null ? (
        <div className="notice" style={{ position: 'fixed', left: 16, right: 16, bottom: 16 }}>
          {error}
        </div>
      ) : null}
    </div>
  )
}

function EngineTab(props: {
  label: string
  count: number
  active: boolean
  offline?: boolean
  onClick: () => void
}) {
  return (
    <button data-active={props.active} onClick={props.onClick} title={props.offline ? 'not connected' : undefined}>
      {props.label}
      <span className="count">{props.offline ? '—' : props.count}</span>
    </button>
  )
}

/** Whether the selected session is running, for the activity poll. */
function active_is_running(snapshot: StateSnapshot | null, key: string | null): boolean {
  if (snapshot === null || key === null) return false
  return snapshot.sessions.some((session) => session.key === key && session.running)
}

function labelOf(engine: EngineId): string {
  return engine === 'dsh' ? 'DSH' : engine === 'claude' ? 'Claude' : 'Codex'
}

export type { Interaction, Session }
