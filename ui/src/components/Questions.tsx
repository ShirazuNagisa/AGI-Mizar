import { useState } from 'react'
import type { Interaction } from '../api'

/**
 * A question batch, answered in one gesture.
 *
 * DSH rejects a reply that does not name every question in the batch exactly
 * once, so the card collects a selection for each and sends them together. A
 * per-question "send" would be a reply the host throws away — and a card that
 * looks answered but was not.
 */
export function Questions(props: {
  interaction: Interaction
  onAnswer: (id: string, answers: unknown[]) => void
}) {
  const [picked, setPicked] = useState<Record<string, string[]>>({})

  const choose = (questionId: string, label: string, multi: boolean) => {
    setPicked((current) => {
      const chosen = current[questionId] ?? []
      if (!multi) return { ...current, [questionId]: [label] }
      return {
        ...current,
        [questionId]: chosen.includes(label)
          ? chosen.filter((entry) => entry !== label)
          : [...chosen, label],
      }
    })
  }

  const complete = props.interaction.questions.every(
    (question) => (picked[question.id] ?? []).length > 0,
  )

  return (
    <div className="approval">
      <div className="head">
        <span className="badge" style={{ background: 'var(--accent-soft)', color: 'var(--accent)' }}>
          Question
        </span>
        <span>waiting on you</span>
      </div>

      {props.interaction.questions.map((question) => (
        <div key={question.id} style={{ display: 'flex', flexDirection: 'column', gap: 6 }}>
          {question.header !== null ? <span className="label">{question.header}</span> : null}
          <span style={{ fontWeight: 500 }}>{question.text}</span>
          {question.detail !== null ? <span className="hint">{question.detail}</span> : null}
          <div style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
            {question.options.map((option) => {
              const active = (picked[question.id] ?? []).includes(option)
              return (
                <button
                  key={option}
                  className="btn"
                  data-active={active}
                  style={
                    active
                      ? { background: 'var(--accent)', borderColor: 'transparent', color: '#fff' }
                      : undefined
                  }
                  onClick={() => choose(question.id, option, question.multi_select)}
                >
                  {option}
                </button>
              )
            })}
          </div>
          {question.multi_select ? <span className="hint">Choose any.</span> : null}
        </div>
      ))}

      <div style={{ display: 'flex', gap: 8 }}>
        <button
          className="btn primary"
          disabled={!complete}
          onClick={() =>
            props.onAnswer(
              props.interaction.id,
              props.interaction.questions.map((question) => ({
                id: question.id,
                selected: picked[question.id] ?? [],
              })),
            )
          }
        >
          Answer
        </button>
      </div>
    </div>
  )
}
