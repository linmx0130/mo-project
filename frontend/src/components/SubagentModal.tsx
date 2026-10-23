import { useEffect, useRef, useState } from 'react'
import type { JournalEvent, Mode, Session, SessionStatus } from '../api'
import { getHistory, getFullEvent, getSession } from '../api'
import { TimelineFolder, isTerminal } from '../timeline'
import { EventRow, MessageRow, ToolBlockRow } from './Timeline'

interface Props {
  /** The subagent session id (from the parent's `subagent_started` event). */
  childId: string
  onClose: () => void
}

/** Read-only modal listing a subagent session's messages, live-updating
 *  over SSE.
 *
 *  Subagents don't receive user input directly, so there is no composer —
 *  this is a pure message view (the subagent's task is its first user
 *  message, seeded into its journal by the parent worker). The modal
 *  watches the child's journal exactly like the session view: history +
 *  SSE tail, closing once the status is terminal and the journal drains.
 */
export default function SubagentModal({ childId, onClose }: Props) {
  const [session, setSession] = useState<Session | null>(null)
  // Incremental fold state (same pattern as the session view): the light
  // history seeds the folder once, the SSE tail folds into it in place.
  // The folder lives in state because it is read during render.
  const [folder, setFolder] = useState(() => new TimelineFolder())
  const [version, setVersion] = useState(0)
  const [error, setError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())
  const lastSeqRef = useRef<number | null>(null)
  const terminalSeenRef = useRef(false)
  const timelineRef = useRef<HTMLDivElement | null>(null)

  useEffect(() => {
    let cancelled = false
    let es: EventSource | null = null
    terminalSeenRef.current = false

    // The folder is created lazily on the first event (never synchronously
    // in the effect body): a child switch re-runs the effect with a fresh
    // closure and therefore a fresh folder.
    let activeFolder: TimelineFolder | null = null
    const ensureFolder = () => {
      if (!activeFolder) {
        activeFolder = new TimelineFolder()
        setFolder(activeFolder)
      }
      return activeFolder
    }
    const applyEvent = (ev: JournalEvent) => {
      if (typeof ev.seq === 'number') lastSeqRef.current = ev.seq
      ensureFolder().push(ev)
      if (ev.kind.kind === 'status_change') {
        const newStatus = ev.kind.status
        const newError = ev.kind.error ?? null
        setSession((prev) =>
          prev ? { ...prev, status: newStatus, error: newError } : prev,
        )
        if (isTerminal(newStatus)) terminalSeenRef.current = true
      }
    }

    ;(async () => {
      try {
        // The light history (superseded previews dropped, old reasoning
        // omitted — expandable per message via `getFullEvent`).
        const [current, history] = await Promise.all([
          getSession(childId),
          getHistory(childId),
        ])
        if (cancelled) return
        setSession(current)
        for (const ev of history) applyEvent(ev)
        if (isTerminal(current.status)) terminalSeenRef.current = true
        setVersion((v) => v + 1)

        const cursor =
          lastSeqRef.current !== null
            ? `?after_seq=${lastSeqRef.current}`
            : ''
        es = new EventSource(`/api/sessions/${childId}/events${cursor}`)
        es.onmessage = (msg) => {
          applyEvent(JSON.parse(msg.data) as JournalEvent)
          setVersion((v) => v + 1)
        }
        es.onerror = () => {
          // The gateway closes the stream once the session is terminal and
          // the journal is drained; stop the auto-reconnect loop.
          if (terminalSeenRef.current && es) es.close()
        }
      } catch (err) {
        if (!cancelled) {
          setError(err instanceof Error ? err.message : String(err))
        }
      }
    })()

    return () => {
      cancelled = true
      es?.close()
    }
  }, [childId])

  // Close on Escape (the ✕ and the overlay click are handled in the JSX).
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [onClose])

  // Stick to the bottom while messages stream in.
  useEffect(() => {
    const el = timelineRef.current
    if (el) el.scrollTop = el.scrollHeight
  }, [version])

  const timeline = folder.items()

  /** Load one light-history message's reasoning on demand. */
  const loadReasoning = async (seq: number): Promise<string | null> => {
    const ev = await getFullEvent(childId, seq)
    const full =
      ev.kind.kind === 'message' ? (ev.kind.reasoning_content ?? '') : ''
    folder.patchMessage(seq, { reasoning_content: full })
    setVersion((v) => v + 1)
    return full
  }

  /** Load a light-history tool block's complete output on demand. */
  const loadToolOutput = async (seq: number): Promise<void> => {
    const ev = await getFullEvent(childId, seq)
    if (ev.kind.kind === 'tool_result') {
      folder.patchToolResult(seq, {
        output: ev.kind.output,
        outputOmitted: false,
        outputChars: undefined,
      })
      setVersion((v) => v + 1)
    }
  }
  const anyToolStreaming = timeline.some(
    (item) => item.type === 'tool' && item.block.streaming,
  )
  useEffect(() => {
    if (!anyToolStreaming) return
    const id = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(id)
  }, [anyToolStreaming])

  const status: SessionStatus | null = session?.status ?? null
  const mode: Mode | null = session?.mode ?? null

  return (
    <div className="dialog-overlay" onClick={onClose}>
      <div
        className="dialog subagent-modal"
        role="dialog"
        aria-modal="true"
        aria-label="Subagent messages"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="dialog-head">
          <span className="dialog-title">
            Subagent{status ? ` — ${status}` : ''}
            {mode ? (
              <span className={`badge mode-${mode} subagent-mode`}>{mode}</span>
            ) : null}
          </span>
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
        <p className="subagent-subtitle muted">
          {session?.prompt || 'Subagent session'}
          {session ? ` · ${session.id.slice(0, 8)}` : ''}
        </p>
        {error && <p className="form-error">{error}</p>}
        <div className="subagent-body" ref={timelineRef}>
          {timeline.map((item, i) => {
            switch (item.type) {
              case 'message':
                return (
                  <MessageRow
                    key={`msg-${i}`}
                    message={item.message}
                    onLoadReasoning={(seq) => loadReasoning(seq)}
                  />
                )
              case 'tool':
                return (
                  <ToolBlockRow
                    key={`${item.block.id}-${i}`}
                    block={item.block}
                    now={now}
                    onLoadOutput={(seq) => loadToolOutput(seq)}
                  />
                )
              case 'event':
                return (
                  <EventRow
                    key={item.event.seq ?? `synth-${i}`}
                    event={item.event}
                  />
                )
            }
          })}
          {timeline.length === 0 && (
            <p className="muted list-empty">Waiting for events…</p>
          )}
        </div>
      </div>
    </div>
  )
}
