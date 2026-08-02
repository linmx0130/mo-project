import { useCallback, useEffect, useState } from 'react'
import type { Session } from './api'
import { listSessions } from './api'
import NewSessionForm from './components/NewSessionForm'
import SessionList from './components/SessionList'
import SessionView from './components/SessionView'

function App() {
  const [sessions, setSessions] = useState<Session[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)

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

  const selected = sessions.find((s) => s.id === selectedId) ?? null

  return (
    <div className="app">
      <aside className="sidebar">
        <header className="sidebar-header">
          <h1>
            mo <span className="subtitle">agent harness</span>
          </h1>
        </header>
        <NewSessionForm
          onCreated={(s) => {
            setSelectedId(s.id)
            void refresh()
          }}
        />
        <SessionList
          sessions={sessions}
          selectedId={selectedId}
          onSelect={setSelectedId}
        />
      </aside>
      <main className="main">
        {selected ? (
          <SessionView
            key={selected.id}
            session={selected}
            onStatusChange={refresh}
          />
        ) : (
          <div className="empty muted">
            Select a session on the left, or start a new one.
          </div>
        )}
      </main>
    </div>
  )
}

export default App
