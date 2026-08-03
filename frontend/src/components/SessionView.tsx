import { useEffect, useRef, useState } from 'react'
import type { JournalEvent, JournalMessage, Session, SessionStatus } from '../api'
import { cancelSession, getHistory, getSession, postMessage } from '../api'
import Composer from './Composer'

interface Props {
  session: Session
  onStatusChange: () => void
}

function isTerminal(status: SessionStatus): boolean {
  return status === 'completed' || status === 'failed' || status === 'cancelled'
}

interface ToolBlock {
  id: string
  name: string
  arguments: string
  output?: string
  ok?: boolean
}

type TimelineItem = JournalEvent | ToolBlock

/** Pair each tool_call_start with its tool_result into one block. */
function buildTimeline(events: JournalEvent[]): TimelineItem[] {
  const items: TimelineItem[] = []
  const pending = new Map<string, ToolBlock>()
  for (const ev of events) {
    const kind = ev.kind
    if (kind.kind === 'tool_call_start') {
      const block: ToolBlock = {
        id: kind.id,
        name: kind.name,
        arguments: kind.arguments,
      }
      pending.set(kind.id, block)
      items.push(block)
    } else if (kind.kind === 'tool_result' && pending.has(kind.id)) {
      const block = pending.get(kind.id)!
      block.output = kind.output
      block.ok = kind.ok
      pending.delete(kind.id)
    } else {
      items.push(ev)
    }
  }
  return items
}

export default function SessionView({ session, onStatusChange }: Props) {
  const [status, setStatus] = useState<SessionStatus>(session.status)
  const [events, setEvents] = useState<JournalEvent[]>([])
  const [error, setError] = useState<string | null>(null)
  const [cancelling, setCancelling] = useState(false)
  const [sending, setSending] = useState(false)
  // Bumped on every send: a terminal session's SSE stream is closed, so a
  // followup must re-arm it to watch the new run.
  const [runId, setRunId] = useState(0)
  const lastSeqRef = useRef<number | null>(null)
  const terminalSeenRef = useRef(isTerminal(session.status))

  useEffect(() => {
    // Per-instance flag: StrictMode double-mounts effects in dev; a shared
    // ref would let the first mount's late async work slip through and
    // duplicate events.
    let cancelled = false
    let es: EventSource | null = null
    terminalSeenRef.current = false

    ;(async () => {
      try {
        const [current, history] = await Promise.all([
          getSession(session.id),
          getHistory(session.id),
        ])
        if (cancelled) return
        setStatus(current.status)
        setEvents(history)
        lastSeqRef.current =
          history.length > 0 ? history[history.length - 1].seq : null
        if (isTerminal(current.status)) terminalSeenRef.current = true

        const cursor =
          lastSeqRef.current !== null
            ? `?after_seq=${lastSeqRef.current}`
            : ''
        es = new EventSource(`/api/sessions/${session.id}/events${cursor}`)
        es.onmessage = (msg) => {
          const ev = JSON.parse(msg.data) as JournalEvent
          if (typeof ev.seq === 'number') lastSeqRef.current = ev.seq
          setEvents((prev) => [...prev, ev])
          if (ev.kind.kind === 'status_change') {
            setStatus(ev.kind.status)
            if (isTerminal(ev.kind.status)) {
              terminalSeenRef.current = true
              onStatusChange()
            }
          }
        }
        es.onerror = () => {
          // The gateway closes the stream once the session is terminal and
          // the journal is drained; stop the auto-reconnect loop.
          if (terminalSeenRef.current && es) {
            es.close()
          }
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
  }, [session.id, onStatusChange, runId])

  /** Stop the running worker (SIGTERM → SIGKILL the process group). */
  const stop = async () => {
    setCancelling(true)
    try {
      const updated = await cancelSession(session.id)
      setStatus(updated.status)
      terminalSeenRef.current = true
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setCancelling(false)
    }
  }

  /** Send a followup message; the backend respawns the worker on the same
   *  session, continuing from the journal history. */
  const send = async (text: string): Promise<boolean> => {
    setSending(true)
    try {
      const updated = await postMessage(session.id, text)
      setStatus(updated.status)
      // The previous SSE stream closed at the terminal status; re-arm it so
      // the new run streams in.
      setRunId((r) => r + 1)
      onStatusChange()
      return true
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      return false
    } finally {
      setSending(false)
    }
  }

  const running = status === 'running' || status === 'pending'
  const timeline = buildTimeline(events)

  return (
    <div className="session-view">
      <header className="session-header">
        <div>
          <h2>{session.prompt || 'Untitled session'}</h2>
          <p className="muted">
            {session.workdir}
            {session.model ? ` · ${session.model}` : ''} ·{' '}
            {new Date(session.created_at).toLocaleString()}
            {session.error ? ` · error: ${session.error}` : ''}
          </p>
        </div>
        <div className="header-actions">
          <span className={`badge badge-${status}`}>{status}</span>
        </div>
      </header>

      {error && <p className="form-error">{error}</p>}

      <div className="timeline">
        {timeline.map((item, i) =>
          'kind' in item ? (
            <EventRow key={item.seq ?? `synth-${i}`} event={item} />
          ) : (
            // Tool-call ids come from the model and repeat across followup
            // runs (e.g. a mock replaying `call_slow`), so disambiguate.
            <ToolBlockRow key={`${item.id}-${i}`} block={item} />
          ),
        )}
        {timeline.length === 0 && (
          <p className="muted list-empty">Waiting for events…</p>
        )}
      </div>

      <Composer
        running={running}
        busy={sending || cancelling}
        onStop={() => void stop()}
        onSubmit={send}
      />
    </div>
  )
}

function EventRow({ event }: { event: JournalEvent }) {
  const kind = event.kind
  switch (kind.kind) {
    case 'message':
      return <MessageRow message={kind} />
    case 'status_change':
      return (
        <div className="status-change">
          <span className={`badge badge-${kind.status}`}>→ {kind.status}</span>
          {kind.error && <span className="muted">{kind.error}</span>}
        </div>
      )
    default:
      return null
  }
}

function MessageRow({ message }: { message: JournalMessage }) {
  const role = message.role
  if (role === 'tool') {
    return (
      <div className="msg msg-tool">
        <pre className="tool-output">{message.content}</pre>
      </div>
    )
  }
  if (role === 'user') {
    return (
      <div className="msg msg-user">
        <div className="msg-label">user</div>
        <div className="msg-content">{message.content}</div>
      </div>
    )
  }
  // assistant
  return (
    <div className="msg msg-assistant">
      <div className="msg-label">assistant</div>
      {message.reasoning_content && (
        <details className="reasoning">
          <summary>reasoning</summary>
          <pre>{message.reasoning_content}</pre>
        </details>
      )}
      {message.content && <div className="msg-content">{message.content}</div>}
    </div>
  )
}

function ToolBlockRow({ block }: { block: ToolBlock }) {
  return (
    <details className={`tool-block ${block.ok === false ? 'tool-failed' : ''}`}>
      <summary>
        <span className="tool-name">{block.name}</span>
        <code className="tool-args">{block.arguments}</code>
        {block.ok !== undefined && (
          <span className={`tool-status ${block.ok ? 'ok' : 'err'}`}>
            {block.ok ? 'ok' : 'error'}
          </span>
        )}
      </summary>
      {block.output !== undefined && (
        <pre className="tool-output">{block.output}</pre>
      )}
    </details>
  )
}
