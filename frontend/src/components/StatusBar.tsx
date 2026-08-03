import type { SessionStatus } from '../api'

interface Props {
  status: SessionStatus
  /** Latest API-reported context length in tokens; null before the first
   *  LLM call (or when the provider reports no usage). */
  tokens: number | null
  /** The model's configured context window at session time; null =
   *  unlimited (only the current length is shown). */
  contextWindow: number | null
}

const nf = new Intl.NumberFormat('en-US')

/** The session status bar pinned to the bottom of the session view.
 *
 *  Currently shows the session status badge plus the context length in
 *  tokens (from the LLM API's `usage.prompt_tokens`, journaled by the
 *  worker as `context_usage` events). When a context window is configured
 *  the length is rendered against it with a thin progress bar; additional
 *  status items (model, elapsed time, ...) can be appended as siblings. */
export default function StatusBar({ status, tokens, contextWindow }: Props) {
  const pct =
    tokens !== null && contextWindow !== null && contextWindow > 0
      ? Math.min(100, Math.round((tokens / contextWindow) * 100))
      : null
  const high = pct !== null && pct >= 90

  return (
    <footer className="status-bar">
      <span className={`badge badge-${status}`}>{status}</span>
      <span className="context-usage" title="Context length reported by the LLM API (prompt tokens)">
        {tokens === null ? (
          <>
            <span className="context-label">context</span>
            <span className="muted">— tokens</span>
          </>
        ) : contextWindow === null ? (
          <>
            <span className="context-label">context</span>
            {nf.format(tokens)} tokens
          </>
        ) : (
          <>
            <span className="context-label">context</span>
            {nf.format(tokens)} / {nf.format(contextWindow)} tokens
            {pct !== null && (
              <span className={`context-pct ${high ? 'context-high' : ''}`}>
                {pct}%
              </span>
            )}
            <span className={`context-track ${high ? 'context-high' : ''}`}>
              <span
                className="context-fill"
                style={{ width: `${pct ?? 0}%` }}
              />
            </span>
          </>
        )}
      </span>
    </footer>
  )
}
