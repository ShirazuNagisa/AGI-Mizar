import type {
  ActivityEntry,
  Balance,
  DeliveredFile,
  ModelInfo,
  PermissionPreset,
  Session,
  SessionStats,
} from '../api'
import { when } from './SessionList'

/**
 * Everything about a session that is not the conversation: what it has cost,
 * what it is doing right now, what it produced, and the two switches worth
 * having (model, permission).
 *
 * The activity feed is the reason this panel exists. Without it the window shows
 * a session that has been silent for four minutes and a spinner, and there is no
 * way to tell "thinking" from "stuck".
 */
export function SessionDetails(props: {
  session: Session
  stats: SessionStats | null
  activity: ActivityEntry[]
  deliverables: DeliveredFile[]
  models: ModelInfo[]
  permissions: PermissionPreset[]
  balance: Balance | null
  onOpenFile: (file: DeliveredFile) => void
  onModel: (id: string) => void
  onPermission: (id: string) => void
  onRename: () => void
  onArchive: () => void
}) {
  const { session } = props

  return (
    <>
      <div className="field">
        <span className="label">Session</span>
        <span className="value">{session.key}</span>
      </div>

      {session.cwd !== null ? (
        <div className="field">
          <span className="label">Directory</span>
          <span className="value">{session.cwd}</span>
        </div>
      ) : null}

      {props.stats !== null ? (
        <div className="field">
          <span className="label">Usage</span>
          <span className="value">
            {props.stats.turns} turns · {props.stats.steps} steps ·{' '}
            {(props.stats.output_tokens / 1000).toFixed(1)}k out ·{' '}
            {(props.stats.input_tokens / 1000).toFixed(1)}k in
          </span>
          <span className="value">
            {Math.round(props.stats.llm_ms / 1000)}s model · {Math.round(props.stats.tool_ms / 1000)}s tools
          </span>
        </div>
      ) : null}

      {session.context !== null ? (
        <div className="field">
          <span className="label">Context · {Math.round(session.context.percent)}%</span>
          <div className="meter">
            <div
              style={{
                width: `${Math.min(100, Math.max(1, session.context.percent))}%`,
                background: session.context.percent > 85 ? 'var(--warn)' : 'var(--accent)',
              }}
            />
          </div>
          <span className="value">
            {(session.context.used / 1000).toFixed(1)}k / {(session.context.window / 1000).toFixed(0)}k tokens
          </span>
        </div>
      ) : null}

      {props.models.length > 0 ? (
        <div className="field">
          <span className="label">Model</span>
          <select
            className="picker"
            value={props.models.find((model) => model.id === session.model)?.id ?? ''}
            onChange={(event) => props.onModel(event.target.value)}
          >
            <option value="" disabled>
              {session.model ?? 'choose'}
            </option>
            {props.models.map((model) => (
              <option key={model.id} value={model.id}>
                {model.label}
              </option>
            ))}
          </select>
        </div>
      ) : null}

      {props.permissions.length > 0 ? (
        <div className="field">
          <span className="label">Permissions</span>
          <select
            className="picker"
            value=""
            onChange={(event) => props.onPermission(event.target.value)}
          >
            <option value="" disabled>
              change
            </option>
            {props.permissions.map((preset) => (
              <option key={preset.id} value={preset.id}>
                {preset.label}
              </option>
            ))}
          </select>
        </div>
      ) : null}

      {props.activity.length > 0 ? (
        <div className="field">
          <span className="label">Now</span>
          <div className="feed">
            {props.activity.slice(-14).map((entry) => (
              <div key={entry.id} className="feed-row" data-kind={entry.kind}>
                <span className="feed-title">{entry.title}</span>
                <span className="feed-text">{entry.text}</span>
              </div>
            ))}
          </div>
        </div>
      ) : null}

      {props.deliverables.length > 0 ? (
        <div className="field">
          <span className="label">Delivered</span>
          {props.deliverables.map((file) => (
            <button
              className="delivered"
              key={file.path}
              title={file.path}
              onClick={() => props.onOpenFile(file)}
            >
              <span>{file.name}</span>
              {file.description !== null ? <span className="hint">{file.description}</span> : null}
            </button>
          ))}
        </div>
      ) : null}

      {props.balance !== null && props.balance.status !== 'unsupported' ? (
        <div className="field">
          <span className="label">Balance</span>
          {props.balance.status === 'ready' ? (
            <>
              {props.balance.wallets.map((wallet) => (
                <span className="value" key={wallet.currency}>
                  {wallet.amount} {wallet.currency}
                </span>
              ))}
              {props.balance.bonus.map((wallet) => (
                <span className="hint" key={`bonus-${wallet.currency}`}>
                  + {wallet.amount} {wallet.currency} bonus
                </span>
              ))}
            </>
          ) : (
            <span className="value">
              {props.balance.status === 'signed_out' ? 'signed out' : 'not available'}
            </span>
          )}
        </div>
      ) : null}

      <div className="field">
        <span className="label">Updated</span>
        <span className="value">{when(session.updated_at)} ago</span>
      </div>

      <div style={{ display: 'flex', gap: 8 }}>
        <button className="btn" onClick={props.onRename}>
          Rename
        </button>
        <button className="btn" onClick={props.onArchive}>
          Archive
        </button>
      </div>
    </>
  )
}
