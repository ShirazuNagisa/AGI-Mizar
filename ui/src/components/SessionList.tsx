import type { Interaction, Session } from '../api'

/**
 * The list, newest first.
 *
 * A row carries three things and no more: what the session is, which product it
 * belongs to, and whether something is waiting. Anything else is a detail the
 * right-hand panel exists for.
 */
export function SessionList(props: {
  sessions: Session[]
  selected: string | null
  approvals: Interaction[]
  onSelect: (key: string) => void
}) {
  const waiting = new Set(props.approvals.map((approval) => approval.session))

  return (
    <section className="panel">
      <div className="panel-head">
        Sessions
        <span className="spacer" style={{ flex: 1 }} />
        <span style={{ fontWeight: 400, color: 'var(--text-3)' }}>{props.sessions.length}</span>
      </div>
      <div className="sessions">
        {props.sessions.length === 0 ? (
          <div className="empty" style={{ padding: 24 }}>
            <strong>Nothing here yet</strong>
            <span>Start a session in DSH, Claude Code or Codex.</span>
          </div>
        ) : (
          props.sessions.map((session) => (
            <button
              key={session.key}
              className="session"
              data-active={session.key === props.selected}
              onClick={() => props.onSelect(session.key)}
            >
              <span className="name">{session.title ?? 'Untitled'}</span>
              <span className="dot" data-running={session.running} />
              <span className="meta">
                <span>{engineLabel(session.key)}</span>
                {session.model !== null ? <span>{session.model}</span> : null}
                <span>{when(session.updated_at)}</span>
                {waiting.has(session.key) ? <span style={{ color: 'var(--warn)' }}>waiting on you</span> : null}
              </span>
            </button>
          ))
        )}
      </div>
    </section>
  )
}

function engineLabel(key: string): string {
  const engine = key.split(':')[0]
  return engine === 'dsh' ? 'DSH' : engine === 'claude' ? 'Claude' : 'Codex'
}

/** A short, human distance in time. */
export function when(at: number): string {
  if (at <= 0) return ''
  const seconds = Math.max(0, Math.round((Date.now() - at) / 1000))
  if (seconds < 60) return 'now'
  if (seconds < 3600) return `${Math.round(seconds / 60)}m`
  if (seconds < 86_400) return `${Math.round(seconds / 3600)}h`
  return `${Math.round(seconds / 86_400)}d`
}
