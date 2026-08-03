import { useState } from 'react'
import type { FormEvent, KeyboardEvent } from 'react'

interface Props {
  /** A session run is in progress: the input is frozen and Stop is shown. */
  running: boolean
  /** A send request is in flight: Send is disabled. */
  busy?: boolean
  placeholder?: string
  onStop?: () => void
  /** Resolve `false` to keep the typed text (e.g. validation failed). */
  onSubmit: (text: string) => Promise<boolean | void> | boolean | void
}

/** Bottom-of-view chat input: textarea + Send, or Stop while running. */
export default function Composer({
  running,
  busy = false,
  placeholder,
  onStop,
  onSubmit,
}: Props) {
  const [text, setText] = useState('')

  const send = async () => {
    const trimmed = text.trim()
    if (!trimmed || running || busy) return
    const keep = await onSubmit(trimmed)
    if (keep === false) return
    setText('')
  }

  const submit = (e: FormEvent) => {
    e.preventDefault()
    void send()
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends (Shift+Enter inserts a newline); ignore IME composition.
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      void send()
    }
  }

  return (
    <form className="composer" onSubmit={submit}>
      <textarea
        value={text}
        onChange={(e) => setText(e.target.value)}
        onKeyDown={onKeyDown}
        placeholder={placeholder ?? 'Type a message…'}
        rows={3}
        disabled={running}
        spellCheck={false}
      />
      <div className="composer-actions">
        {running ? (
          <button
            type="button"
            className="stop"
            onClick={() => onStop?.()}
            disabled={busy}
          >
            Stop
          </button>
        ) : (
          <button type="submit" className="send" disabled={busy || !text.trim()}>
            {busy ? 'Sending…' : 'Send'}
          </button>
        )}
      </div>
    </form>
  )
}
