import { useEffect, useMemo, useRef, useState } from 'react'
import type {
  AskUserQuestion,
  JournalEvent,
  Mode,
  ModelInfo,
  PermissionRequestItem,
  Session,
  SessionStatus,
} from '../api'
import {
  answerAskUser,
  answerPermission,
  approveModeChange,
  cancelSession,
  getHistory,
  getModels,
  getSession,
  postMessage,
  rejectModeChange,
  switchMode,
  switchModel,
} from '../api'
import AskUserCard from './AskUserCard'
import Composer from './Composer'
import PermissionCard from './PermissionCard'
import StatusBar from './StatusBar'
import SubagentModal from './SubagentModal'
import { buildTimeline, isTerminal } from '../timeline'
import { EventRow, MessageRow, ToolBlockRow } from './Timeline'

interface Props {
  session: Session
  onStatusChange: () => void
}

export default function SessionView({ session, onStatusChange }: Props) {
  const [status, setStatus] = useState<SessionStatus>(session.status)
  const [events, setEvents] = useState<JournalEvent[]>([])
  const [error, setError] = useState<string | null>(null)
  const [cancelling, setCancelling] = useState(false)
  const [sending, setSending] = useState(false)
  // A subagent session whose messages are shown in a read-only modal
  // (opened from a `spawn_subagent` tool block's "view subagent" button).
  const [subagentId, setSubagentId] = useState<string | null>(null)
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
  // The current model, as shown/switched in the status bar. Switching only
  // affects the next run — the worker respawned for the next followup is
  // spawned with the new model and receives the full journal history. Local
  // state is the source of truth while the switcher is used; when the prop
  // reports a different model (e.g. the sidebar refresh after a switch
  // lands), adjust state during render — same pattern as `mode`.
  const [model, setModel] = useState<string>(session.model)
  const [prevModel, setPrevModel] = useState<string>(session.model)
  if (prevModel !== session.model) {
    setPrevModel(session.model)
    setModel(session.model)
  }
  // The configured models (GET /api/models), for the status-bar picker.
  const [models, setModels] = useState<ModelInfo[]>([])

  useEffect(() => {
    let cancelled = false
    getModels()
      .then((list) => {
        if (!cancelled) setModels(list)
      })
      .catch(() => {
        // Gateway unreachable; the picker stays disabled.
      })
    return () => {
      cancelled = true
    }
  }, [])
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

  /** Stop the running worker (SIGTERM → SIGKILL the process group; the
   *  session's subagents die with it). */
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

  /** Switch the session's model (status-bar picker, terminal sessions
   *  only). Only the next run is affected: the worker respawned for the
   *  next followup is spawned with the new model, and the backend journals
   *  a `model_change` notice right before it. */
  const handleModelSwitch = async (next: string) => {
    if (next === model || running) return
    try {
      const updated = await switchModel(session.id, next)
      setModel(updated.model)
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
  // A pending mode-change request: the agent called `request_mode_change`
  // (journaled as `mode_change_request`) and the user has not answered yet.
  // Resolved by a `mode_change` (approved) or `mode_change_request_declined`
  // (rejected) event after it. While pending, the composer is frozen and the
  // Agree / Reject banner is shown instead.
  const pendingRequest = useMemo(() => {
    let lastRequest: { mode: Mode; message: string; seq: number } | null = null
    let lastResolutionSeq: number | null = null
    for (const ev of events) {
      if (ev.kind.kind === 'mode_change_request') {
        lastRequest = {
          mode: ev.kind.mode,
          message: ev.kind.message,
          seq: ev.seq ?? -1,
        }
      } else if (
        ev.kind.kind === 'mode_change' ||
        ev.kind.kind === 'mode_change_request_declined'
      ) {
        lastResolutionSeq = ev.seq ?? -1
      }
    }
    if (!lastRequest) return null
    if (lastResolutionSeq !== null && lastResolutionSeq > lastRequest.seq) {
      return null
    }
    return lastRequest
  }, [events])
  // A pending clarification question: the agent called `ask_user` (journaled
  // as `ask_user_request`) and the user has not answered yet. Resolved by an
  // `ask_user_answered` event after it. While pending, the composer is frozen
  // and the question card (every option plus a free-text input) is shown.
  const pendingQuestion = useMemo(() => {
    let lastQuestion: { question: AskUserQuestion; seq: number } | null = null
    let lastAnswerSeq: number | null = null
    for (const ev of events) {
      if (ev.kind.kind === 'ask_user_request') {
        lastQuestion = {
          question: ev.kind.question,
          seq: ev.seq ?? -1,
        }
      } else if (ev.kind.kind === 'ask_user_answered') {
        lastAnswerSeq = ev.seq ?? -1
      }
    }
    if (!lastQuestion) return null
    if (lastAnswerSeq !== null && lastAnswerSeq > lastQuestion.seq) {
      return null
    }
    return lastQuestion
  }, [events])
  // A pending file-access permission request: file tools asked to read or
  // write paths outside the auto-allowed roots (journaled as a batched
  // `permission_request` with one item per held call) and the user has not
  // decided yet. Resolved by a `permission_answered` event after it. While
  // pending, the composer is frozen and the Allow / Deny card is shown
  // instead. Legacy single-item requests (pre-batch journals) are treated
  // as a one-item batch.
  const pendingPermission = useMemo(() => {
    let lastRequest: {
      request_id: string
      items: PermissionRequestItem[]
      legacy?: { tool: string; operation: string; path: string }
      seq: number
    } | null = null
    let lastAnswerSeq: number | null = null
    for (const ev of events) {
      if (ev.kind.kind === 'permission_request') {
        const kind = ev.kind
        lastRequest = {
          request_id: kind.request_id,
          items: kind.items ?? [],
          legacy:
            kind.tool !== undefined &&
            kind.operation !== undefined &&
            kind.path !== undefined
              ? { tool: kind.tool, operation: kind.operation, path: kind.path }
              : undefined,
          seq: ev.seq ?? -1,
        }
      } else if (ev.kind.kind === 'permission_answered') {
        lastAnswerSeq = ev.seq ?? -1
      }
    }
    if (!lastRequest) return null
    if (lastAnswerSeq !== null && lastAnswerSeq > lastRequest.seq) {
      return null
    }
    return lastRequest
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

  /** Approve the pending mode-change request: the backend switches the
   *  session's mode to the requested one and continues the run with a
   *  single mode-change message. */
  const approveRequest = async () => {
    setSending(true)
    try {
      const updated = await approveModeChange(session.id)
      setStatus(updated.status)
      // Re-arm the SSE stream (the run continues in the new mode); the
      // effect also refetches the history, which now resolves the request.
      setRunId((r) => r + 1)
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSending(false)
    }
  }

  /** Reject the pending mode-change request: the backend journals a
   *  `mode_change_request_declined` marker (no mode switch, nothing sent to
   *  the agent) and the composer unfreezes. */
  const rejectRequest = async () => {
    setSending(true)
    try {
      const updated = await rejectModeChange(session.id)
      setStatus(updated.status)
      // Re-arm the SSE stream / refetch history so the declined marker
      // lands in the timeline and the request stops being pending.
      setRunId((r) => r + 1)
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSending(false)
    }
  }

  /** Answer the pending clarification question: the backend journals the
   *  answers (a JSON object keyed by question_id) and resumes the run; the
   *  model receives them as a user message. */
  const submitAnswer = async (answers: Record<string, string>) => {
    setSending(true)
    try {
      const updated = await answerAskUser(session.id, answers)
      setStatus(updated.status)
      // Re-arm the SSE stream (the run continues with the answer); the
      // effect also refetches the history, which now resolves the request.
      setRunId((r) => r + 1)
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSending(false)
    }
  }

  /** Decide the pending file-access permission request: `decisions` maps
   *  each held call's id to Allow (true) / Deny (false) and must cover
   *  every item of the batched request; the backend journals the decisions
   *  and resumes the run — the held calls then complete (allowed → real
   *  result, denied → denial error). Legacy single-item requests are
   *  answered with `allowed` instead. */
  const submitPermission = async (
    decisions: Record<string, boolean> | boolean,
  ) => {
    if (!pendingPermission) return
    setSending(true)
    try {
      const updated = await answerPermission(
        session.id,
        pendingPermission.request_id,
        typeof decisions === 'boolean' ? { allowed: decisions } : { decisions },
      )
      setStatus(updated.status)
      // Re-arm the SSE stream (the run continues with the decisions); the
      // effect also refetches the history, which now resolves the request.
      setRunId((r) => r + 1)
      onStatusChange()
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setSending(false)
    }
  }

  return (
    <div className="session-view">
      <header className="session-header">
        <div>
          <h2>{session.prompt || 'Untitled session'}</h2>
          <p className="muted">
            {session.workdir}
            {model ? ` · ${model}` : ''} ·{' '}
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
                  onOpenSubagent={(childId) => setSubagentId(childId)}
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

      {pendingQuestion && !running ? (
        // A clarification question from the agent is awaiting the user's
        // answer: freeze the composer and show the question card (every
        // option plus a free-text input box) instead.
        <AskUserCard
          question={pendingQuestion.question}
          busy={sending}
          onSubmit={(answers) => void submitAnswer(answers)}
        />
      ) : pendingRequest && !running ? (
        // A mode-change request from the agent is awaiting the user's
        // decision: freeze the input box and show Agree / Reject instead.
        <ModeChangeRequestBanner
          request={pendingRequest}
          busy={sending}
          onApprove={() => void approveRequest()}
          onReject={() => void rejectRequest()}
        />
      ) : pendingPermission && !running ? (
        // A file-access request from the agent is awaiting the user's
        // decision: freeze the input box and show the Allow / Deny card
        // listing every requested path instead.
        <PermissionCard
          requestId={pendingPermission.request_id}
          items={
            pendingPermission.items.length > 0
              ? pendingPermission.items
              : pendingPermission.legacy
                ? [
                    {
                      call_id: '',
                      tool: pendingPermission.legacy.tool,
                      operation: pendingPermission.legacy.operation,
                      path: pendingPermission.legacy.path,
                    },
                  ]
                : []
          }
          legacy={
            pendingPermission.items.length === 0 &&
            pendingPermission.legacy !== undefined
          }
          busy={sending}
          onSubmit={(decisions) => void submitPermission(decisions)}
        />
      ) : (
        <Composer
          running={running}
          busy={sending || cancelling}
          onStop={() => void stop()}
          onSubmit={send}
        />
      )}

      <StatusBar
        status={status}
        tokens={contextUsage?.tokens ?? null}
        contextWindow={contextUsage?.context_window ?? null}
        mode={mode}
        modeEnabled={!running}
        onSwitchMode={(next) => void handleModeSwitch(next)}
        models={models}
        model={model}
        modelEnabled={!running}
        onSwitchModel={(next) => void handleModelSwitch(next)}
      />

      {subagentId && (
        <SubagentModal
          childId={subagentId}
          onClose={() => setSubagentId(null)}
        />
      )}
    </div>
  )
}

/** The frozen-composer banner shown while a `mode_change_request` is
 *  pending: the agent's request message (in the user's language) plus the
 *  Agree / Reject buttons. */
function ModeChangeRequestBanner({
  request,
  busy,
  onApprove,
  onReject,
}: {
  request: { mode: Mode; message: string }
  busy: boolean
  onApprove: () => void
  onReject: () => void
}) {
  return (
    <div className="mode-request-banner">
      <div className="mode-request-head">
        <span className={`badge mode-${request.mode}`}>
          → mode: {request.mode}
        </span>
        <span className="mode-request-title">
          The agent requests to switch the session mode
        </span>
      </div>
      <p className="mode-request-message">{request.message}</p>
      <div className="mode-request-actions">
        <button type="button" className="send" onClick={onApprove} disabled={busy}>
          {busy ? 'Working…' : 'Agree'}
        </button>
        <button type="button" className="stop" onClick={onReject} disabled={busy}>
          Reject
        </button>
      </div>
    </div>
  )
}
