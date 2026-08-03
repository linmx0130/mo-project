import { useCallback, useEffect, useState } from 'react'
import type { Session } from './api'
import { listSessions } from './api'
import DraftSession from './components/DraftSession'
import SessionList from './components/SessionList'
import SessionView from './components/SessionView'

function App() {
  const [sessions, setSessions] = useState<Session[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [draftOpen, setDraftOpen] = useState(false)
  const [lastWorkdir, setLastWorkdir] = useState('')

  const refresh = useCallback(async () => {
    try {
      setSessions(await listSessions())
    } catch {
      // gateway not reachable; the interval will retry
    }
  }, [])

  useEffect(() => {
    // Deferred initial load (set-state-in-effect wants no synchronous
    // setState inside the effect body), then poll every 3s.
    const first = setTimeout(() => void refresh(), 0)
    const timer = setInterval(() => void refresh(), 3000)
    return () => {
      clearTimeout(first)
      clearInterval(timer)
    }
  }, [refresh])

  const openDraft = () => {
    setDraftOpen(true)
    setSelectedId(null)
  }

  const selectSession = (id: string) => {
    setDraftOpen(false)
    setSelectedId(id)
  }

  const handleCreated = (s: Session) => {
    setLastWorkdir(s.workdir)
    selectSession(s.id)
    void refresh()
  }

  const selected = sessions.find((s) => s.id === selectedId) ?? null

  return (
    <div className="app">
      <aside className="sidebar">
        <header className="sidebar-header">
          <h1>
            mo <span className="subtitle">agent harness</span>
          </h1>
        </header>
        <button type="button" className="new-session-btn" onClick={openDraft}>
          + New session
        </button>
        <SessionList
          sessions={sessions}
          selectedId={selectedId}
          onSelect={selectSession}
        />
      </aside>
      <main className="main">
        {draftOpen ? (
          <DraftSession defaultWorkdir={lastWorkdir} onCreated={handleCreated} />
        ) : selected ? (
          <SessionView
            key={selected.id}
            session={selected}
            onStatusChange={refresh}
          />
        ) : (
          <div className="empty muted">
            Start a new session, or pick one from the list.
          </div>
        )}
      </main>
    </div>
  )
}

export default App
