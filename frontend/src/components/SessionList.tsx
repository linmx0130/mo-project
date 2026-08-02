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
  onSelect: (id: string) => void
}

export default function SessionList({ sessions, selectedId, onSelect }: Props) {
  if (sessions.length === 0) {
    return <p className="muted list-empty">No sessions yet.</p>
  }
  return (
    <ul className="session-list">
      {sessions.map((s) => (
        <li key={s.id}>
          <button
            type="button"
            className={`session-item ${s.id === selectedId ? 'selected' : ''}`}
            onClick={() => onSelect(s.id)}
          >
            <span className={`badge badge-${s.status}`}>{STATUS_LABEL[s.status]}</span>
            <span className="session-prompt">{s.prompt.slice(0, 80)}</span>
            <span className="session-time">{formatTime(s.created_at)}</span>
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
