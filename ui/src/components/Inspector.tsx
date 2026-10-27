import type { EngineStatus, Interaction, Session } from '../api'
import { when } from './SessionList'

/**
 * What the conversation screen does not need: identifiers, the engine's own
 * status, and anything the agent is blocked on.
 *
 * Approvals come first and are impossible to miss. An agent that is waiting is
 * not doing anything else, so a card that requires scrolling is a card that
 * costs minutes.
 */
export function Inspector(props: {
  session: Session | null
  approvals: Interaction[]
  engines: EngineStatus[]
  version: string
  onAnswer: (id: string, outcome: string) => void
}) {
  return (
    <>
      <div className="panel-head">Details</div>
      <div className="inspector">
        {props.approvals.map((approval) => (
          <div className="approval" key={approval.id}>
            <div className="head">
              <span className="badge">Approval</span>
              <span>{approval.title}</span>
            </div>
            {approval.detail !== null ? <pre>{approval.detail}</pre> : null}
            <div style={{ display: 'flex', gap: 8 }}>
              <button className="btn primary" onClick={() => props.onAnswer(approval.id, 'allowed-once')}>
                Allow once
              </button>
              <button className="btn" onClick={() => props.onAnswer(approval.id, 'rejected')}>
                Reject
              </button>
            </div>
            <span className="hint">asked {when(approval.requested_at)} ago</span>
          </div>
        ))}

        {props.session !== null ? (
          <>
            <Field label="Session" value={props.session.key} />
            {props.session.model !== null ? <Field label="Model" value={props.session.model} /> : null}
            {props.session.cwd !== null ? <Field label="Directory" value={props.session.cwd} /> : null}
            <Field label="Turns" value={String(props.session.turns)} />
            {props.session.context !== null ? (
              <div className="field">
                <span className="label">
                  Context · {Math.round(props.session.context.percent)}%
                </span>
                <div
                  style={{
                    height: 6,
                    borderRadius: 999,
                    background: 'color-mix(in srgb, var(--text) 10%, transparent)',
                    overflow: 'hidden',
                  }}
                >
                  <div
                    style={{
                      width: `${Math.min(100, Math.max(1, props.session.context.percent))}%`,
                      height: '100%',
                      borderRadius: 999,
                      background:
                        props.session.context.percent > 85 ? 'var(--warn)' : 'var(--accent)',
                    }}
                  />
                </div>
                <span className="value">
                  {(props.session.context.used / 1000).toFixed(1)}k /{' '}
                  {(props.session.context.window / 1000).toFixed(0)}k tokens
                </span>
              </div>
            ) : null}
            <Field label="Updated" value={when(props.session.updated_at)} />
          </>
        ) : (
          <span className="hint">No session selected.</span>
        )}

        <div className="field">
          <span className="label">Engines</span>
          {props.engines.map((engine) => (
            <div key={engine.engine} style={{ display: 'flex', gap: 8, alignItems: 'center' }}>
              <span
                className="dot"
                data-running={engine.connected}
                style={{
                  width: 6,
                  height: 6,
                  borderRadius: '50%',
                  background: engine.connected ? 'var(--ok)' : 'var(--text-3)',
                }}
              />
              <span style={{ fontSize: 12 }}>{engine.engine}</span>
              <span className="hint" style={{ marginLeft: 'auto' }}>
                {engine.detail ?? ''}
              </span>
            </div>
          ))}
        </div>

        <Field label="Kernel" value={props.version} />
      </div>
    </>
  )
}

function Field(props: { label: string; value: string }) {
  return (
    <div className="field">
      <span className="label">{props.label}</span>
      <span className="value">{props.value}</span>
    </div>
  )
}
