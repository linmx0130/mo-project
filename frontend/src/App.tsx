import { useCallback, useEffect, useState } from 'react'
import type { Session } from './api'
import { getMeta, listSessions } from './api'
import DraftSession from './components/DraftSession'
import SessionList from './components/SessionList'
import SessionView from './components/SessionView'

type Theme = 'dark' | 'light'

const THEME_KEY = 'mo-theme'

function initialTheme(): Theme {
  const saved = localStorage.getItem(THEME_KEY)
  if (saved === 'dark' || saved === 'light') return saved
  // No explicit choice yet: follow the OS preference.
  return window.matchMedia('(prefers-color-scheme: light)').matches
    ? 'light'
    : 'dark'
}

function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme
  localStorage.setItem(THEME_KEY, theme)
}

function App() {
  const [sessions, setSessions] = useState<Session[]>([])
  const [selectedId, setSelectedId] = useState<string | null>(null)
  const [draftOpen, setDraftOpen] = useState(false)
  const [lastWorkdir, setLastWorkdir] = useState('')
  const [theme, setTheme] = useState<Theme>(initialTheme)

  // Theme: applied to <html data-theme> so CSS vars switch; persisted so the
  // choice survives reloads.
  useEffect(() => {
    applyTheme(theme)
  }, [theme])

  const toggleTheme = () => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))

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

  // Pre-fill the draft workdir with the gateway's startup directory (the
  // cwd the gateway process was launched from), so new sessions don't make
  // the user retype it.
  useEffect(() => {
    let cancelled = false
    getMeta()
      .then((meta) => {
        if (!cancelled && meta.cwd) setLastWorkdir(meta.cwd)
      })
      .catch(() => {
        // gateway not reachable; fall back to an empty workdir
      })
    return () => {
      cancelled = true
    }
  }, [])

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
        <div className="sidebar-list">
          <SessionList
            sessions={sessions}
            selectedId={selectedId}
            onSelect={selectSession}
          />
        </div>
        <footer className="sidebar-footer">
          <button
            type="button"
            className="theme-toggle"
            onClick={toggleTheme}
            aria-label={`Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`}
            title={`Switch to ${theme === 'dark' ? 'light' : 'dark'} mode`}
          >
            <span className="theme-icon" aria-hidden="true">
              {theme === 'dark' ? '☀️' : '🌙'}
            </span>
            {theme === 'dark' ? 'Light mode' : 'Dark mode'}
          </button>
        </footer>
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
