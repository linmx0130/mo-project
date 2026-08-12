import { useState } from 'react'
import type { AskUserQuestion } from '../api'

interface Props {
  question: AskUserQuestion
  busy: boolean
  onSubmit: (answers: Record<string, string>) => void
}

/** The frozen-composer question card shown while an `ask_user_request` is
 *  pending: the agent's question (title + explanation), every option as a
 *  selectable card, and a final empty free-text input box. The user answers
 *  by picking an option (the answer is that option's title) or typing their
 *  own text; picking an option clears the typed text and vice versa. */
export default function AskUserCard({ question, busy, onSubmit }: Props) {
  const [selected, setSelected] = useState<string | null>(null)
  const [freestyle, setFreestyle] = useState('')
  const answered = (freestyle.trim() || selected) !== null
  const answer = (freestyle.trim() || (selected ?? '')).trim()

  const submit = () => {
    if (!answer || busy) return
    onSubmit({ [question.question_id]: answer })
  }

  return (
    <div className="ask-card">
      <div className="ask-card-head">
        <span className="ask-card-title">The agent is asking you a question</span>
      </div>
      <div className="ask-question">
        <div className="ask-question-title">{question.question_title}</div>
        {question.question_text && (
          <p className="ask-question-text">{question.question_text}</p>
        )}
      </div>
      {question.options.length > 0 && (
        <div className="ask-options">
          {question.options.map((opt) => {
            const active = selected === opt.option_title
            return (
              <button
                type="button"
                key={opt.option_title}
                className={`ask-option${active ? ' selected' : ''}`}
                onClick={() => {
                  setSelected(active ? null : opt.option_title)
                  setFreestyle('')
                }}
              >
                <span className="ask-option-title">{opt.option_title}</span>
                {opt.option_text && (
                  <span className="ask-option-text">{opt.option_text}</span>
                )}
              </button>
            )
          })}
        </div>
      )}
      <input
        type="text"
        className="ask-freestyle"
        placeholder={
          question.options.length > 0
            ? 'Or type your own answer…'
            : 'Type your answer…'
        }
        value={freestyle}
        onChange={(e) => {
          setFreestyle(e.target.value)
          if (e.target.value.trim()) setSelected(null)
        }}
        onKeyDown={(e) => {
          // Enter submits the free-text answer.
          if (e.key === 'Enter') {
            e.preventDefault()
            submit()
          }
        }}
        spellCheck={false}
      />
      <div className="ask-card-actions">
        <button
          type="button"
          className="send"
          disabled={busy || !answered}
          onClick={submit}
        >
          {busy ? 'Working…' : 'Send answer'}
        </button>
      </div>
    </div>
  )
}
