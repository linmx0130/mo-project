import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import type { JournalEvent } from '../api'
import type { MessageBlock, ToolBlock } from '../timeline'
import CopyButton from './CopyButton'

/** One rendered timeline row. Components only — the folding logic lives in
 *  `src/timeline.ts` (see `buildTimeline`). */
export function EventRow({ event }: { event: JournalEvent }) {
  const kind = event.kind
  switch (kind.kind) {
    case 'status_change':
      return (
        <div className="status-change">
          <span className={`badge badge-${kind.status}`}>→ {kind.status}</span>
          {kind.error && <span className="muted">{kind.error}</span>}
        </div>
      )
    case 'mode_change':
      // The gateway injected a mode-change notice before a followup user
      // message; render it as a subtle notice row, not a chat bubble.
      return (
        <div className="mode-change">
          <span className={`badge mode-${kind.mode}`}>→ mode: {kind.mode}</span>
          <span className="muted">{kind.content}</span>
        </div>
      )
    case 'mode_change_request':
      // The agent asked (via request_mode_change) to switch the session's
      // mode; the passive timeline record of the request. The actionable
      // Agree / Reject banner is rendered above the composer while the
      // request is pending.
      return (
        <div className="mode-change">
          <span className={`badge mode-${kind.mode}`}>
            → mode: {kind.mode} requested
          </span>
          <span className="muted">{kind.message}</span>
        </div>
      )
    case 'mode_change_request_declined':
      // The user rejected the request; it is resolved (no mode switch).
      return (
        <div className="mode-change">
          <span className={`badge mode-${kind.mode}`}>
            → mode: {kind.mode} rejected
          </span>
          <span className="muted">The user rejected the mode change request.</span>
        </div>
      )
    case 'ask_user_request':
      // The agent asked a clarification question (via ask_user); the
      // actionable question card is rendered above the composer while the
      // request is pending. This is the passive timeline record.
      return (
        <div className="mode-change">
          <span className="badge">→ question asked</span>
          <span className="muted">
            {kind.question.question_title}
            {kind.question.options.length > 0
              ? ` (${kind.question.options.length} option${kind.question.options.length === 1 ? '' : 's'} + free text)`
              : ' (free text)'}
          </span>
        </div>
      )
    case 'ask_user_answered':
      // The user answered the question (picked an option or typed free
      // text); the request is resolved and the answer was sent to the agent.
      return (
        <div className="mode-change">
          <span className="badge">→ answered</span>
          <span className="muted">
            {Object.entries(kind.answers)
              .map(([id, value]) => `${id}: ${value}`)
              .join(' · ')}
          </span>
        </div>
      )
    case 'handoff':
      // Context compression: the model generated a handoff prompt and the
      // events before it are no longer sent to the model (they stay in the
      // journal, above this row). Render the handoff text in a collapsible
      // block so it is inspectable without dominating the timeline.
      return (
        <div className="mode-change handoff">
          <span className={`badge mode-${kind.mode}`}>→ context compressed</span>
          <details>
            <summary>
              <span className="muted">
                handoff prompt for the next session (earlier events stay
                visible above; they are no longer sent to the model)
              </span>
            </summary>
            <div className="msg-content markdown handoff-body">
              <ReactMarkdown remarkPlugins={[remarkGfm]}>
                {kind.content}
              </ReactMarkdown>
            </div>
          </details>
        </div>
      )
    default:
      // message / tool events are folded into dedicated items by
      // buildTimeline; anything else is not rendered.
      return null
  }
}

export function MessageRow({ message }: { message: MessageBlock }) {
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
          <CopyButton content={message.content} disabled={message.streaming} />
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

export function ToolBlockRow({
  block,
  now,
  onOpenSubagent,
}: {
  block: ToolBlock
  now: number
  /** Called with the subagent session id when the "view subagent" button
   *  is clicked (only for `spawn_subagent` blocks that spawned a child). */
  onOpenSubagent?: (childId: string) => void
}) {
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
        {block.childId && (
          <button
            type="button"
            className="subagent-link"
            title="View the subagent's messages"
            onClick={(e) => {
              // Don't toggle the details (the summary row is clickable).
              e.stopPropagation()
              onOpenSubagent?.(block.childId!)
            }}
          >
            view subagent →
          </button>
        )}
      </summary>
      {block.output !== undefined && (
        <pre className="tool-output">{block.output}</pre>
      )}
    </details>
  )
}
