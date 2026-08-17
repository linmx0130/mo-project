interface Props {
  /** The tool name that requested access (read_file / edit_file /
   *  create_file / remove_file). */
  tool: string
  /** 'read' or 'write'. */
  operation: string
  /** The path as the model passed it. */
  path: string
  /** A send request is in flight: the buttons are disabled. */
  busy: boolean
  onAllow: () => void
  onDeny: () => void
}

/** The frozen-composer card shown while a `permission_request` is pending:
 *  a file tool asked to access a path outside the auto-allowed roots (the
 *  working directory, the session scratch dir, and — for reads — global
 *  skill folders). The user decides with Allow / Deny; the backend journals
 *  the decision and resumes the run (the model retries the tool call when
 *  allowed, or finds another way when denied). */
export default function PermissionCard({
  tool,
  operation,
  path,
  busy,
  onAllow,
  onDeny,
}: Props) {
  return (
    <div className="mode-request-banner">
      <div className="mode-request-head">
        <span className="badge">→ file access requested</span>
        <span className="mode-request-title">
          The agent requests permission to {operation} this file
        </span>
      </div>
      <p className="mode-request-message">
        <code className="permission-path">{path}</code>
        <span className="muted">
          {' '}
          via the <code>{tool}</code> tool. The path is outside the working
          directory, the session scratch dir and the skill folders.
        </span>
      </p>
      <div className="mode-request-actions">
        <button type="button" className="send" onClick={onAllow} disabled={busy}>
          {busy ? 'Working…' : 'Allow'}
        </button>
        <button type="button" className="stop" onClick={onDeny} disabled={busy}>
          Deny
        </button>
      </div>
    </div>
  )
}
