import type { Session, SessionStatus } from '../api'

const STATUS_LABEL: Record<SessionStatus, string> = {
  pending: 'pending',
  running: 'running',
  completed: 'done',
  failed: 'failed',
  cancelled: 'cancelled',
}

interface Props {
  sessions: Session[]
  selectedId: string | null
  /** Session currently being deleted (delete button shows a spinner). */
  deletingId: string | null
  onSelect: (id: string) => void
  onDelete: (id: string) => void
}

export default function SessionList({
  sessions,
  selectedId,
  deletingId,
  onSelect,
  onDelete,
}: Props) {
  if (sessions.length === 0) {
    return <p className="muted list-empty">No sessions yet.</p>
  }
  return (
    <ul className="session-list">
      {sessions.map((s) => (
        <li
          key={s.id}
          className={`session-row ${s.id === selectedId ? 'selected' : ''}`}
        >
          <button
            type="button"
            className="session-item"
            onClick={() => onSelect(s.id)}
          >
            <span className={`badge badge-${s.status}`}>{STATUS_LABEL[s.status]}</span>
            <span className="session-prompt">{s.prompt.slice(0, 80)}</span>
            <span className="session-time">{formatTime(s.created_at)}</span>
          </button>
          <button
            type="button"
            className="session-delete"
            aria-label={`Delete session: ${s.prompt.slice(0, 80)}`}
            title="Delete session"
            disabled={deletingId === s.id}
            onClick={() => onDelete(s.id)}
          >
            {deletingId === s.id ? (
              '…'
            ) : (
              <svg
                width="14"
                height="14"
                viewBox="0 0 24 24"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
              >
                <polyline points="3 6 5 6 21 6" />
                <path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2" />
                <line x1="10" y1="11" x2="10" y2="17" />
                <line x1="14" y1="11" x2="14" y2="17" />
              </svg>
            )}
          </button>
        </li>
      ))}
    </ul>
  )
}

function formatTime(iso: string): string {
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return ''
  return d.toLocaleTimeString([], { hour: '2-digit', minute: '2-digit' })
}
