import type {
  ActivityEntry,
  Balance,
  DeliveredFile,
  EngineStatus,
  Interaction,
  ModelInfo,
  PermissionPreset,
  Session,
  SessionStats,
} from '../api'
import { Questions } from './Questions'
import { SessionDetails } from './SessionDetails'
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
  interactions: Interaction[]
  engines: EngineStatus[]
  version: string
  stats: SessionStats | null
  activity: ActivityEntry[]
  deliverables: DeliveredFile[]
  models: ModelInfo[]
  permissions: PermissionPreset[]
  balance: Balance | null
  onOpenFile: (file: DeliveredFile) => void
  onAnswer: (id: string, outcome?: string, answers?: unknown[]) => void
  onModel: (id: string) => void
  onPermission: (id: string) => void
  onRename: () => void
  onArchive: () => void
}) {
  return (
    <>
      <div className="panel-head">Details</div>
      <div className="inspector">
        {props.interactions
          .filter((entry) => entry.kind === 'question')
          .map((question) => (
            <Questions
              key={question.id}
              interaction={question}
              onAnswer={(id, answers) => props.onAnswer(id, undefined, answers)}
            />
          ))}

        {props.interactions
          .filter((entry) => entry.kind === 'approval')
          .map((approval) => (
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
          <SessionDetails
            session={props.session}
            stats={props.stats}
            activity={props.activity}
            deliverables={props.deliverables}
            models={props.models}
            permissions={props.permissions}
            balance={props.balance}
            onOpenFile={props.onOpenFile}
            onModel={props.onModel}
            onPermission={props.onPermission}
            onRename={props.onRename}
            onArchive={props.onArchive}
          />
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
