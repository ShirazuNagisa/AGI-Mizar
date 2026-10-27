import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { Kernel, type ApiEvent, type EngineId, type Interaction, type TurnRecord, type Session, type StateSnapshot } from './api'
import { SessionList } from './components/SessionList'
import { Conversation } from './components/Conversation'
import { Inspector } from './components/Inspector'

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

  const sessions = useMemo(() => {
    if (snapshot === null) return []
    return snapshot.sessions.filter(
      (session) => filter === 'all' || session.key.startsWith(`${filter}:`),
    )
  }, [snapshot, filter])

  const active = sessions.find((session) => session.key === selected) ?? null
  const approvals = useMemo(
    () => (snapshot?.interactions ?? []).filter((i) => i.kind === 'approval'),
    [snapshot],
  )

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
        <button className="btn ghost" onClick={() => setInspector((open) => !open)}>
          {inspector ? 'Hide details' : 'Details'}
        </button>
      </header>

      <div className="main" data-inspector={inspector ? 'open' : 'closed'}>
        <SessionList
          sessions={sessions}
          selected={selected}
          onSelect={setSelected}
          approvals={approvals}
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
              approvals={approvals.filter((approval) => approval.session === active?.key || approval.session === null)}
              onAnswer={(id, outcome) => act(() => kernel.send({ op: 'answer', id, outcome }))}
              version={snapshot.version}
              engines={snapshot.engines}
            />
          </aside>
        ) : null}
      </div>

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

function labelOf(engine: EngineId): string {
  return engine === 'dsh' ? 'DSH' : engine === 'claude' ? 'Claude' : 'Codex'
}

export type { Interaction, Session }
