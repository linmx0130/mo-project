import { useEffect, useMemo, useRef, useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { JournalEvent, JournalMessage, Mode, Session, SessionStatus } from '../api'
import { cancelSession, getHistory, getSession, postMessage, switchMode } from '../api'
import Composer from './Composer'
import CopyButton from './CopyButton'
import StatusBar from './StatusBar'

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
  /** True while output deltas are still arriving (tool still running). */
  streaming?: boolean
  /** Epoch ms of the `tool_call_start` event, for the elapsed badge. */
  startedAt?: number
}

/** An assistant message as rendered; `streaming` marks a message whose
 *  content is still being assembled from `message_delta` events. */
type MessageBlock = JournalMessage & { streaming?: boolean }

type TimelineItem =
  | { type: 'message'; message: MessageBlock }
  | { type: 'tool'; block: ToolBlock }
  | { type: 'event'; event: JournalEvent }

/** Fold the journal event stream into renderable items.
 *
 * - `message_delta` events append to the assistant message currently being
 *   assembled (token-by-token preview of both the visible text and the
 *   reasoning content); the following `message` event replaces its content
 *   with the canonical assembled text — which also repairs the transient
 *   state when a retried LLM call left partial deltas behind.
 * - `tool_output_delta` events append to the open tool block with the same
 *   id (bash output streaming); the following `tool_result` event replaces
 *   the preview with the complete, capped output.
 * - `tool_call_start`/`tool_result` are paired into one block as before.
 */
function buildTimeline(events: JournalEvent[]): TimelineItem[] {
  const items: TimelineItem[] = []
  const pending = new Map<string, ToolBlock>()
  let openMessage: MessageBlock | null = null

  for (const ev of events) {
    const kind = ev.kind
    switch (kind.kind) {
      case 'message_delta': {
        const reasoning = kind.reasoning_content ?? ''
        if (openMessage) {
          if (kind.content) openMessage.content += kind.content
          if (reasoning) {
            openMessage.reasoning_content =
              (openMessage.reasoning_content ?? '') + reasoning
          }
        } else {
          const block: MessageBlock = {
            role: 'assistant',
            content: kind.content,
            reasoning_content: reasoning || null,
            streaming: true,
          }
          openMessage = block
          items.push({ type: 'message', message: block })
        }
        break
      }
      case 'message': {
        if (kind.role === 'assistant' && openMessage) {
          // Finalize the delta-built preview with the canonical message.
          openMessage.content = kind.content
          openMessage.reasoning_content = kind.reasoning_content ?? null
          openMessage.tool_call_id = kind.tool_call_id ?? null
          openMessage.tool_calls = kind.tool_calls ?? null
          openMessage.streaming = false
          openMessage = null
        } else {
          items.push({
            type: 'message',
            message: {
              role: kind.role,
              content: kind.content,
              reasoning_content: kind.reasoning_content ?? null,
              tool_call_id: kind.tool_call_id ?? null,
              tool_calls: kind.tool_calls ?? null,
            },
          })
        }
        break
      }
      case 'tool_call_start': {
        const block: ToolBlock = {
          id: kind.id,
          name: kind.name,
          arguments: kind.arguments,
          startedAt: new Date(ev.ts).getTime(),
        }
        pending.set(kind.id, block)
        items.push({ type: 'tool', block })
        break
      }
      case 'tool_output_delta': {
        const block = pending.get(kind.id)
        if (block) {
          block.output = (block.output ?? '') + kind.output
          block.streaming = true
        }
        break
      }
      case 'tool_result': {
        const block = pending.get(kind.id)
        if (block) {
          block.ok = kind.ok
          block.streaming = false
          // The canonical result is capped at ~1 MB while the delta stream
          // is not; keep whichever is longer so the tail is never lost when
          // the result lands (small commands still get the canonical text,
          // including the exit-code line).
          if (!block.output || kind.output.length >= block.output.length) {
            block.output = kind.output
          } else {
            block.output = `${block.output}\n\n[tool result was truncated by the harness — showing the full streamed output]`
          }
          pending.delete(kind.id)
        }
        break
      }
      case 'system_prompt':
        // Session metadata (the system prompt journaled on the first run);
        // never rendered as a chat message.
        break
      default:
        items.push({ type: 'event', event: ev })
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
  // The current mode, as shown/switched in the status bar. Switching only
  // changes the write sandbox of subsequent runs — the journaled system
  // prompt never changes. Local state is the source of truth while the
  // switcher is used; when the prop reports a different mode (e.g. the
  // sidebar refresh after a switch lands), adjust state during render.
  const [mode, setMode] = useState<Mode>(session.mode)
  const [prevMode, setPrevMode] = useState<Mode>(session.mode)
  if (prevMode !== session.mode) {
    setPrevMode(session.mode)
    setMode(session.mode)
  }
  // Bumped on every send: a terminal session's SSE stream is closed, so a
  // followup must re-arm it to watch the new run.
  const [runId, setRunId] = useState(0)
  const lastSeqRef = useRef<number | null>(null)
  const terminalSeenRef = useRef(isTerminal(session.status))
  // Tick while any tool block is streaming, so running badges show how
  // long a command has been executing (a long silent command no longer
  // looks dead).
  const [now, setNow] = useState(() => Date.now())

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

  /** Switch the session's mode (status-bar picker, terminal sessions only).
   *  Only the write sandbox of subsequent runs changes — the journaled
   *  system prompt never does. */
  const handleModeSwitch = async (next: Mode) => {
    if (next === mode || running) return
    try {
      const updated = await switchMode(session.id, next)
      setMode(updated.mode)
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    }
  }

  const running = status === 'running' || status === 'pending'
  const timeline = buildTimeline(events)
  // The status bar shows the latest context_usage event (the worker journals
  // one per LLM call; the last one reflects the deepest context, tool
  // outputs included). Null until the first call reports usage.
  const contextUsage = useMemo(() => {
    let latest: { tokens: number; context_window?: number | null } | null = null
    for (const ev of events) {
      if (ev.kind.kind === 'context_usage') {
        latest = { tokens: ev.kind.tokens, context_window: ev.kind.context_window ?? null }
      }
    }
    return latest
  }, [events])
  // While any tool is still streaming, re-render once per second so the
  // elapsed badge ticks; stop when everything settles.
  const anyToolStreaming = timeline.some(
    (item) => item.type === 'tool' && item.block.streaming,
  )
  useEffect(() => {
    if (!anyToolStreaming) return
    const id = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(id)
  }, [anyToolStreaming])
  // Stick to the bottom while events stream in, unless the user scrolled up.
  const timelineRef = useRef<HTMLDivElement | null>(null)
  const stickToBottomRef = useRef(true)
  useEffect(() => {
    const el = timelineRef.current
    if (el && stickToBottomRef.current) {
      el.scrollTop = el.scrollHeight
    }
  }, [events, now])

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

      <div
        className="timeline"
        ref={timelineRef}
        onScroll={() => {
          const el = timelineRef.current
          if (!el) return
          stickToBottomRef.current =
            el.scrollHeight - el.scrollTop - el.clientHeight < 80
        }}
      >
        {timeline.map((item, i) => {
          switch (item.type) {
            case 'message':
              return <MessageRow key={`msg-${i}`} message={item.message} />
            case 'tool':
              // Tool-call ids come from the model and repeat across followup
              // runs (e.g. a mock replaying `call_slow`), so disambiguate.
              return (
                <ToolBlockRow
                  key={`${item.block.id}-${i}`}
                  block={item.block}
                  now={now}
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

      <Composer
        running={running}
        busy={sending || cancelling}
        onStop={() => void stop()}
        onSubmit={send}
      />

      <StatusBar
        status={status}
        tokens={contextUsage?.tokens ?? null}
        contextWindow={contextUsage?.context_window ?? null}
        mode={mode}
        modeEnabled={!running}
        onSwitchMode={(next) => void handleModeSwitch(next)}
      />
    </div>
  )
}

function EventRow({ event }: { event: JournalEvent }) {
  const kind = event.kind
  switch (kind.kind) {
    case 'status_change':
      return (
        <div className="status-change">
          <span className={`badge badge-${kind.status}`}>→ {kind.status}</span>
          {kind.error && <span className="muted">{kind.error}</span>}
        </div>
      )
    default:
      // message / tool events are folded into dedicated items by
      // buildTimeline; anything else is not rendered.
      return null
  }
}

function MessageRow({ message }: { message: MessageBlock }) {
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
  // assistant — rendered as Markdown (the worker's LLM output is Markdown)
  return (
    <div className="msg msg-assistant">
      <div className="msg-head">
        <div className="msg-label">assistant</div>
        {message.content && (
          // The button copies the raw Markdown source, not the rendered
          // HTML. Disabled while the message is still streaming so the
          // user can't grab a partial reply.
          <CopyButton
            content={message.content}
            disabled={message.streaming}
          />
        )}
      </div>
      {message.reasoning_content && (
        // While reasoning is streaming, force the block open so the tokens
        // are visible as they arrive; once the run settles the `open` prop
        // is removed and the details stay in whatever state the user left.
        <details className="reasoning" open={message.streaming || undefined}>
          <summary>reasoning</summary>
          <pre>{message.reasoning_content}</pre>
        </details>
      )}
      {message.content && (
        <div className="msg-content markdown">
          <ReactMarkdown remarkPlugins={[remarkGfm]}>
            {message.content}
          </ReactMarkdown>
        </div>
      )}
      {message.streaming && (
        <span className="stream-cursor" aria-hidden="true" />
      )}
    </div>
  )
}

function ToolBlockRow({ block, now }: { block: ToolBlock; now: number }) {
  const elapsedSecs =
    block.startedAt !== undefined
      ? Math.max(0, Math.floor((now - block.startedAt) / 1000))
      : 0
  // While output is streaming, force the block open so the user sees it
  // fill in live; once the result lands the `open` prop is removed and the
  // details become uncontrolled again, staying open until the user
  // collapses it.
  return (
    <details
      className={`tool-block ${block.ok === false ? 'tool-failed' : ''}`}
      open={block.streaming || undefined}
    >
      <summary>
        <span className="tool-name">{block.name}</span>
        <code className="tool-args">{block.arguments}</code>
        {block.streaming && (
          <span className="tool-status running">
            running{elapsedSecs > 0 ? ` ${elapsedSecs}s` : ''}
          </span>
        )}
        {block.streaming && !block.output && elapsedSecs >= 5 && (
          <span className="tool-status muted">no output yet</span>
        )}
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
