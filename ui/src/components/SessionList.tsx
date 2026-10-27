import { useMemo, useState } from 'react'
import type { Interaction, Session, Workspace } from '../api'

/**
 * The list, grouped by the directory the session works in.
 *
 * A flat list is fine for four sessions and useless for three hundred. The phone
 * grouped by workspace because that is how DSH itself is organised; a directory
 * is the same idea and is the one thing all three engines report, so grouping by
 * it gives one list rather than three.
 *
 * A group remembers being folded, because the operator who collapsed
 * `~/Documents/books` does not want it reopened by the next refresh.
 */
export function SessionList(props: {
  sessions: Session[]
  selected: string | null
  approvals: Interaction[]
  workspaces: Workspace[]
  onSelect: (key: string) => void
}) {
  const [query, setQuery] = useState('')
  const [folded, setFolded] = useState<Record<string, boolean>>({})

  const waiting = useMemo(
    () => new Set(props.approvals.map((entry) => entry.session)),
    [props.approvals],
  )

  const groups = useMemo(() => {
    const needle = query.trim().toLowerCase()
    const visible = props.sessions.filter((session) => {
      if (needle === '') return true
      return [session.title, session.cwd, session.model, session.key]
        .filter((value): value is string => value !== null)
        .some((value) => value.toLowerCase().includes(needle))
    })

    // A workspace is the host's own grouping and beats a directory guess; a
    // session the host has not filed falls back to its directory, which is the
    // only thing all three engines report.
    const byDirectory = new Map<string, Session[]>()
    for (const session of visible) {
      const key = workspaceLabel(session, props.workspaces) ?? session.cwd ?? 'No folder'
      const bucket = byDirectory.get(key)
      if (bucket === undefined) byDirectory.set(key, [session])
      else bucket.push(session)
    }

    // Newest group first, and newest row first inside it: the same order the
    // flat list had, kept at both levels.
    return [...byDirectory.entries()]
      .map(([directory, sessions]) => ({
        directory,
        sessions: sessions.sort((a, b) => b.updated_at - a.updated_at),
        newest: Math.max(...sessions.map((session) => session.updated_at)),
      }))
      .sort((a, b) => b.newest - a.newest)
  }, [props.sessions, props.workspaces, query])

  return (
    <section className="panel">
      <div className="panel-head">
        Sessions
        <span className="spacer" style={{ flex: 1 }} />
        <span style={{ fontWeight: 400, color: 'var(--text-3)' }}>{props.sessions.length}</span>
      </div>

      <div className="search">
        <input
          value={query}
          placeholder="Filter by title or folder"
          onChange={(event) => setQuery(event.target.value)}
        />
      </div>

      <div className="sessions">
        {groups.length === 0 ? (
          <div className="empty" style={{ padding: 24 }}>
            <strong>{query === '' ? 'Nothing here yet' : 'Nothing matches'}</strong>
            <span>
              {query === '' ? 'Start a session in DSH, Claude Code or Codex.' : 'Try a shorter filter.'}
            </span>
          </div>
        ) : (
          groups.map((group) => {
            const closed = folded[group.directory] === true
            return (
              <div key={group.directory}>
                <button
                  className="group"
                  onClick={() =>
                    setFolded((current) => ({ ...current, [group.directory]: !closed }))
                  }
                >
                  <span className="chevron" data-closed={closed}>
                    ▾
                  </span>
                  <span className="path">{shortPath(group.directory)}</span>
                  <span className="count">{group.sessions.length}</span>
                </button>
                {closed
                  ? null
                  : group.sessions.map((session) => (
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
                          {waiting.has(session.key) ? (
                            <span style={{ color: 'var(--warn)' }}>waiting on you</span>
                          ) : null}
                        </span>
                      </button>
                    ))}
              </div>
            )
          })
        )}
      </div>
    </section>
  )
}

function engineLabel(key: string): string {
  const engine = key.split(':')[0]
  return engine === 'dsh' ? 'DSH' : engine === 'claude' ? 'Claude' : 'Codex'
}

/** The workspace a session belongs to, as the host named it. */
function workspaceLabel(session: Session, workspaces: Workspace[]): string | null {
  const raw = session.key.slice(session.key.indexOf(':') + 1)
  const workspace = workspaces.find((candidate) => candidate.session_ids.includes(raw))
  if (workspace === undefined) return null
  return workspace.name ?? workspace.path ?? null
}

/** The last two path segments, which is what tells two folders apart. */
function shortPath(path: string): string {
  if (path === 'No folder') return path
  const parts = path.split('/').filter((part) => part !== '')
  if (parts.length <= 2) return parts.join('/')
  return `…/${parts.slice(-2).join('/')}`
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
