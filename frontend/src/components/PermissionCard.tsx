import { useState } from 'react'

/** One held file-tool call of a batched permission request. */
export interface PermissionItem {
  call_id: string
  /** The tool name that requested access (read_file / edit_file /
   *  create_file / remove_file). */
  tool: string
  /** 'read' or 'write'. */
  operation: string
  /** The path as the model passed it. */
  path: string
}

interface Props {
  requestId: string
  /** Every held call of the pending batched request (one row per path). */
  items: PermissionItem[]
  /** True for a legacy single-item request (pre-batch journals): the card
   *  answers with a plain Allow / Deny instead of a decisions map. */
  legacy: boolean
  /** A send request is in flight: the buttons are disabled. */
  busy: boolean
  /** `decisions` maps each held call's id to Allow (true) / Deny (false)
   *  and covers every item; legacy requests pass a plain boolean. */
  onSubmit: (decisions: Record<string, boolean> | boolean) => void
}

/** The frozen-composer card shown while a `permission_request` is pending:
 *  file tools asked to access paths outside the auto-allowed roots (the
 *  working directory, the session scratch dir, and — for reads — global
 *  skill folders). Every held call of the message is listed with its own
 *  Allow / Deny; the card submits once every path is decided (Allow all /
 *  Deny all decide everything in one click). The backend journals the
 *  decisions and resumes the run — the held calls then complete (allowed →
 *  real result, denied → denial error). */
export default function PermissionCard({
  items,
  legacy,
  busy,
  onSubmit,
}: Props) {
  // Per-call decisions as the user picks them; submitted once complete.
  const [decided, setDecided] = useState<Record<string, boolean>>({})
  const decidedCount = Object.keys(decided).length
  const allDecided = items.length > 0 && decidedCount === items.length

  const decide = (callId: string, allowed: boolean) => {
    const next = { ...decided, [callId]: allowed }
    setDecided(next)
    if (items.length > 0 && Object.keys(next).length === items.length) {
      onSubmit(next)
    }
  }
  const decideAll = (allowed: boolean) => {
    const all: Record<string, boolean> = {}
    for (const item of items) all[item.call_id] = allowed
    if (items.length === 0) {
      // Legacy single-item request: a plain Allow / Deny.
      onSubmit(allowed)
    } else {
      onSubmit(all)
    }
  }

  const head = legacy
    ? 'The agent requests permission for this file'
    : `The agent requests permission for ${items.length} file${
        items.length === 1 ? '' : 's'
      }`
  const rows = legacy
    ? [{ call_id: '', tool: items[0]?.tool ?? '', operation: items[0]?.operation ?? '', path: items[0]?.path ?? '' }]
    : items

  return (
    <div className="mode-request-banner">
      <div className="mode-request-head">
        <span className="badge">→ file access requested</span>
        <span className="mode-request-title">{head}</span>
      </div>
      <ul className="permission-list">
        {rows.map((item) => (
          <li key={item.call_id || 'legacy'} className="permission-row">
            <div className="permission-row-main">
              <code className="permission-path">{item.path}</code>
              <span className="muted">
                {' '}
                via the <code>{item.tool}</code> tool ({item.operation})
              </span>
              {!legacy && item.call_id in decided && (
                <span
                  className={`tool-status ${decided[item.call_id] ? 'ok' : 'err'}`}
                >
                  {decided[item.call_id] ? 'allowed' : 'denied'}
                </span>
              )}
            </div>
            {legacy ? (
              <span className="muted">
                The path is outside the working directory, the session
                scratch dir and the skill folders.
              </span>
            ) : (
              <span className="permission-row-actions">
                <button
                  type="button"
                  className="send"
                  onClick={() => decide(item.call_id, true)}
                  disabled={busy}
                >
                  Allow
                </button>
                <button
                  type="button"
                  className="stop"
                  onClick={() => decide(item.call_id, false)}
                  disabled={busy}
                >
                  Deny
                </button>
              </span>
            )}
          </li>
        ))}
      </ul>
      <div className="mode-request-actions">
        {!legacy && (
          <>
            <button
              type="button"
              className="send"
              onClick={() => decideAll(true)}
              disabled={busy}
            >
              {busy ? 'Working…' : 'Allow all'}
            </button>
            <button
              type="button"
              className="stop"
              onClick={() => decideAll(false)}
              disabled={busy}
            >
              Deny all
            </button>
          </>
        )}
        {legacy && (
          <>
            <button
              type="button"
              className="send"
              onClick={() => decideAll(true)}
              disabled={busy}
            >
              {busy ? 'Working…' : 'Allow'}
            </button>
            <button
              type="button"
              className="stop"
              onClick={() => decideAll(false)}
              disabled={busy}
            >
              Deny
            </button>
          </>
        )}
        {!legacy && !allDecided && (
          <span className="muted permission-hint">
            {decidedCount}/{items.length} decided — the request is sent once
            every path is decided
          </span>
        )}
      </div>
    </div>
  )
}
