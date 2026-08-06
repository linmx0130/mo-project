import { useCallback, useEffect, useState } from 'react'
import type { Session } from './api'
import { deleteSession, getMeta, listSessions } from './api'
import { clearDraft, loadDraft, saveDraft, type Draft } from './draft'
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
  // The "New session" form, in one place so it survives navigating between
  // sessions (DraftSession unmounts); mirrored to localStorage so it also
  // survives reloads. Only a successful session creation clears it.
  const [draft, setDraft] = useState<Draft | null>(loadDraft)
  // Session currently being deleted (delete button shows a spinner and the
  // button is disabled while the request is in flight).
  const [deletingId, setDeletingId] = useState<string | null>(null)
  // Sidebar-level error, e.g. a failed delete (surfaced above the list).
  const [listError, setListError] = useState<string | null>(null)

  // Theme: applied to <html data-theme> so CSS vars switch; persisted so the
  // choice survives reloads.
  useEffect(() => {
    applyTheme(theme)
  }, [theme])

  // Mirror the draft to localStorage on every change; removing it (session
  // created) drops the stored copy too.
  useEffect(() => {
    if (draft) saveDraft(draft)
    else clearDraft()
  }, [draft])

  const toggleTheme = () => setTheme((t) => (t === 'dark' ? 'light' : 'dark'))

  /** Single source of truth for the new-session form. Patches are merged
   *  into the current draft (materializing one from defaults on the first
   *  interaction), so typing a message never clobbers an edited workdir,
   *  model or mode and vice versa. */
  const updateDraft = useCallback(
    (patch: Partial<Draft>) => {
      setDraft((prev) => {
        const base: Draft = prev ?? {
          workdir: lastWorkdir,
          model: '',
          mode: 'build',
          text: '',
        }
        return { ...base, ...patch }
      })
    },
    [lastWorkdir],
  )

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
    // The draft is only cleared here: the session was actually created, so
    // the next "New session" starts fresh (workdir pre-filled from
    // `lastWorkdir`).
    setDraft(null)
    selectSession(s.id)
    void refresh()
  }

  /** Permanently delete a session: clears the selection if the deleted one
   *  was open, then refreshes the list. Failures are surfaced in the
   *  sidebar (the row reappears on the next poll if the delete didn't go
   *  through). */
  const handleDelete = async (id: string) => {
    if (deletingId) return
    setDeletingId(id)
    setListError(null)
    try {
      await deleteSession(id)
      if (selectedId === id) setSelectedId(null)
      await refresh()
    } catch (err) {
      setListError(err instanceof Error ? err.message : String(err))
    } finally {
      setDeletingId(null)
    }
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
          {listError && (
            <p className="form-error list-error" role="alert">
              {listError}
            </p>
          )}
          <SessionList
            sessions={sessions}
            selectedId={selectedId}
            deletingId={deletingId}
            onSelect={selectSession}
            onDelete={(id) => void handleDelete(id)}
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
          <DraftSession
            draft={draft}
            onDraftChange={updateDraft}
            defaultWorkdir={lastWorkdir}
            onCreated={handleCreated}
          />
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
