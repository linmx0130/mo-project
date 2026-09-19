import { useEffect, useRef, useState } from 'react'
import type { Session } from '../api'
import { listSessions, regenerateTitle } from '../api'

/** Mirrors the backend's `title::MAX_TITLE_CHARS` (characters, not bytes). */
const MAX_TITLE_CHARS = 256
/** Fallback when the server answers 202 (generation still running): how long
 *  to watch the session list for the new title before giving up. Slightly
 *  shorter than the server's own wait, so this rarely triggers. */
const REGEN_TIMEOUT_MS = 25_000
const REGEN_POLL_MS = 1000
/** Safety net for a hung connection; the server answers (200/202/500)
 *  within ~50s of its own accord. */
const REGEN_ABORT_MS = 65_000

interface Props {
  session: Session
  onClose: () => void
  /** Persists the new title; the parent closes the modal on success. */
  onSave: (id: string, title: string) => Promise<void>
}

/** Modal for editing a session's title: a plain text field (capped at 256
 *  characters) plus a "regenerate with AI" option that re-runs the gateway's
 *  title generation from the session's first user message.
 *
 *  The regenerate request itself blocks (bounded, server-side) until
 *  generation finishes, so the spinner always resolves: the response either
 *  carries the final title (possibly identical to the old one — at
 *  temperature 0 the model often regenerates the same title, which is a
 *  finished result, not a pending one) or an error. Only if the server gives
 *  up waiting first (202) does the modal fall back to polling
 *  `listSessions` for a title change. The user always confirms with Save. */
export default function EditTitleModal({ session, onClose, onSave }: Props) {
  const [title, setTitle] = useState(session.prompt)
  const [saving, setSaving] = useState(false)
  const [regenerating, setRegenerating] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const closedRef = useRef(false)
  const inputRef = useRef<HTMLInputElement>(null)
  const abortRef = useRef<AbortController | null>(null)

  // Mark the component alive on mount and closed on unmount. StrictMode
  // (dev) runs the cleanup and re-runs the setup on the initial mount, so
  // the flag must be set here — not just cleared in the cleanup — or it
  // would stay "closed" forever and the regenerate spinner would never
  // clear.
  useEffect(() => {
    closedRef.current = false
    return () => {
      closedRef.current = true
      abortRef.current?.abort()
    }
  }, [])

  // Close on Escape (the ✕ and the overlay click are handled in the JSX).
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [onClose])

  // Autofocus the input with the current title selected, ready to be
  // retyped or confirmed.
  useEffect(() => {
    inputRef.current?.focus()
    inputRef.current?.select()
  }, [])

  const handleRegenerate = async () => {
    if (regenerating || saving) return
    setRegenerating(true)
    setError(null)
    const before = session.prompt
    const controller = new AbortController()
    abortRef.current = controller
    const abortTimer = window.setTimeout(
      () => controller.abort(),
      REGEN_ABORT_MS,
    )
    try {
      const { status, session: result } = await regenerateTitle(
        session.id,
        controller.signal,
      )
      if (status !== 202) {
        // 200: generation finished (the title may be identical to the old
        // one — that is a completed result, not a pending one); 500 already
        // threw. Fill the editbox only when the title actually changed.
        if (result.prompt !== before) setTitle(result.prompt)
        return
      }
      // 202: the server stopped waiting before generation finished — fall
      // back to watching the session list for the title change.
      const deadline = Date.now() + REGEN_TIMEOUT_MS
      for (;;) {
        await new Promise((r) => setTimeout(r, REGEN_POLL_MS))
        if (closedRef.current) return
        if (Date.now() >= deadline) {
          setError("Title generation didn't finish in time — try again.")
          return
        }
        const sessions = await listSessions()
        if (closedRef.current) return
        const updated = sessions.find((s) => s.id === session.id)
        if (updated && updated.prompt !== before) {
          setTitle(updated.prompt)
          return
        }
      }
    } catch (err) {
      if (!closedRef.current)
        setError(err instanceof Error ? err.message : String(err))
    } finally {
      window.clearTimeout(abortTimer)
      if (!closedRef.current) setRegenerating(false)
    }
  }

  const trimmed = title.trim()
  const unchanged = trimmed === session.prompt

  const handleSave = async () => {
    if (!trimmed || unchanged || saving || regenerating) return
    setSaving(true)
    setError(null)
    try {
      await onSave(session.id, trimmed)
      onClose()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      setSaving(false)
    }
  }

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog edit-title-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Edit session title"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog-head">
          <span className="dialog-title">Edit session title</span>
          <button
            type="button"
            className="dialog-close"
            onClick={onClose}
            aria-label="Close"
            title="Close"
          >
            ✕
          </button>
        </div>
        <input
          ref={inputRef}
          type="text"
          className="dialog-input edit-title-input"
          value={title}
          maxLength={MAX_TITLE_CHARS}
          disabled={regenerating}
          onChange={(e) => setTitle(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') void handleSave()
          }}
          aria-label="Session title"
        />
        <div className="edit-title-foot">
          <span className="dialog-hint">
            {title.length}/{MAX_TITLE_CHARS}
          </span>
          {error && (
            <span className="dialog-error" role="alert">
              {error}
            </span>
          )}
        </div>
        <div className="dialog-actions">
          <button
            type="button"
            className="dialog-btn"
            disabled={regenerating || saving}
            onClick={() => void handleRegenerate()}
          >
            {regenerating ? 'Generating…' : '✨ Regenerate with AI'}
          </button>
          <button
            type="button"
            className="dialog-btn"
            disabled={saving || regenerating}
            onClick={onClose}
          >
            Cancel
          </button>
          <button
            type="button"
            className="dialog-btn primary"
            disabled={!trimmed || unchanged || saving || regenerating}
            onClick={() => void handleSave()}
          >
            {saving ? 'Saving…' : 'Save'}
          </button>
        </div>
      </div>
    </div>
  )
}
