import { useEffect, useRef, useState } from 'react'
import type { TurnRecord, Session } from '../api'

/**
 * The transcript, and the box you type into.
 *
 * The transcript draws turns, not a flat list: a rule between turns is the only
 * ornament, and it carries information (where one instruction ended and the next
 * began). Tool calls stay collapsed to one line — they are evidence that
 * something ran, not something to read.
 */
export function Conversation(props: {
  session: Session
  records: TurnRecord[]
  onSend: (text: string) => void
  onStop: () => void
}) {
  const [draft, setDraft] = useState('')
  const bottom = useRef<HTMLDivElement | null>(null)

  // Follow the tail, but only when the tail is already near the viewport: a
  // reader scrolled up to an earlier turn should not be yanked back.
  useEffect(() => {
    bottom.current?.scrollIntoView({ block: 'end' })
  }, [props.records.length, props.session.key])

  const submit = () => {
    const text = draft.trim()
    if (text === '') return
    props.onSend(text)
    setDraft('')
  }

  return (
    <>
      <div className="panel-head">
        <span style={{ overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>
          {props.session.title ?? 'Untitled'}
        </span>
        <span className="spacer" style={{ flex: 1 }} />
        {props.session.running ? <span style={{ color: 'var(--accent)' }}>running</span> : null}
      </div>

      <div className="conversation">
        {props.records.length === 0 ? (
          <div className="empty">
            <strong>Nothing in this session yet</strong>
            <span>Type below to send the first message.</span>
          </div>
        ) : (
          props.records.map((record, index) => (
            <Row key={`${record.seq}-${index}`} record={record} />
          ))
        )}
        <div ref={bottom} />
      </div>

      <div className="composer">
        <textarea
          value={draft}
          placeholder="Message this session…"
          onChange={(event) => setDraft(event.target.value)}
          onKeyDown={(event) => {
            if ((event.metaKey || event.ctrlKey) && event.key === 'Enter') {
              event.preventDefault()
              submit()
            }
          }}
        />
        <div className="row">
          <span className="hint">⌘↵ to send</span>
          <span className="spacer" style={{ flex: 1 }} />
          {props.session.running ? (
            <button className="btn" onClick={props.onStop}>
              Stop
            </button>
          ) : null}
          <button className="btn primary" disabled={draft.trim() === ''} onClick={submit}>
            Send
          </button>
        </div>
      </div>
    </>
  )
}

function Row(props: { record: TurnRecord }) {
  const { record } = props

  switch (record.kind) {
    case 'turn_start':
      return (
        <div className="turn-rule">
          Turn {record.turn ?? '?'}
        </div>
      )
    case 'user_message':
      return (
        <div className="message" data-role="user">
          <span className="who">You</span>
          <div className="bubble">{record.text ?? ''}</div>
        </div>
      )
    case 'assistant_message':
      return (
        <div className="message" data-role="assistant">
          <span className="who">Agent</span>
          <div className="bubble">{record.text ?? ''}</div>
        </div>
      )
    case 'tool_call':
      return (
        <div className="tool">
          <span className="name">{record.tool?.name ?? 'tool'}</span>
          <span className="args">{firstLine(record.tool?.arguments ?? '')}</span>
        </div>
      )
    case 'tool_result':
      return (
        <div className="tool" style={{ opacity: 0.7 }}>
          <span className="name">↳</span>
          <span className="args">{firstLine(record.tool?.output ?? '')}</span>
        </div>
      )
    case 'turn_end':
      return <div className="hint">{record.text ?? 'done'}</div>
    default:
      return null
  }
}

function firstLine(text: string): string {
  const line = text.split('\n').find((candidate) => candidate.trim() !== '') ?? ''
  return line.length > 140 ? `${line.slice(0, 137)}…` : line
}
