import { useEffect, useState } from 'react'
import type { Mode, ModeInfo, ModelInfo, Session } from '../api'
import { createSession, getModels, getModes } from '../api'
import Composer from './Composer'

interface Props {
  /** Last used workdir, so consecutive new sessions don't retype it. */
  defaultWorkdir: string
  onCreated: (session: Session) => void
}

/** Placeholder session shown after clicking "New session": a workdir field,
 *  a model picker, a mode picker plus the chat composer. The backend session
 *  is only created on Send. */
export default function DraftSession({ defaultWorkdir, onCreated }: Props) {
  const [workdir, setWorkdir] = useState(defaultWorkdir)
  const [models, setModels] = useState<ModelInfo[]>([])
  const [model, setModel] = useState('')
  const [modes, setModes] = useState<ModeInfo[]>([])
  const [mode, setMode] = useState<Mode>('build')
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Load the configured models; the first one is the default and is
  // pre-selected so "just type and send" uses the default model. The modes
  // are static (build is the default) but fetched for their descriptions.
  useEffect(() => {
    let cancelled = false
    getModels()
      .then((list) => {
        if (cancelled) return
        setModels(list)
        if (list.length > 0) setModel(list[0].name)
      })
      .catch(() => {
        // Gateway unreachable; sending will surface the error.
      })
    getModes()
      .then((list) => {
        if (cancelled) return
        setModes(list)
        const build = list.find((m) => m.name === 'build')
        if (build) setMode(build.name)
      })
      .catch(() => {
        // Gateway unreachable; build stays selected.
      })
    return () => {
      cancelled = true
    }
  }, [])

  const selectedMode = modes.find((m) => m.name === mode)

  const handleSend = async (text: string): Promise<boolean> => {
    if (!workdir.trim()) {
      setError('Working directory is required.')
      return false
    }
    setSending(true)
    setError(null)
    try {
      const session = await createSession(workdir.trim(), text, model || undefined, mode)
      onCreated(session)
      return true
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      setSending(false)
      return false
    }
  }

  return (
    <div className="draft">
      <header className="session-header">
        <div>
          <h2>New session</h2>
          <p className="muted">
            Set the working directory, model and mode, then send your first message.
          </p>
        </div>
      </header>
      <div className="draft-body">
        <label className="field">
          <span className="field-label">Working directory</span>
          <input
            value={workdir}
            onChange={(e) => setWorkdir(e.target.value)}
            placeholder="/absolute/path/to/workdir"
            spellCheck={false}
            disabled={sending}
          />
        </label>
        <label className="field">
          <span className="field-label">Model</span>
          <select
            value={model}
            onChange={(e) => setModel(e.target.value)}
            disabled={sending || models.length === 0}
          >
            {models.length === 0 ? (
              <option value="">no models configured</option>
            ) : (
              models.map((m) => (
                <option key={m.name} value={m.name}>
                  {m.nickname ? `${m.nickname} (${m.name})` : m.name}
                  {m.default ? ' · default' : ''}
                </option>
              ))
            )}
          </select>
        </label>
        <label className="field">
          <span className="field-label">Mode</span>
          <select
            value={mode}
            onChange={(e) => setMode(e.target.value as Mode)}
            disabled={sending}
          >
            {modes.length === 0 ? (
              <option value="build">build</option>
            ) : (
              modes.map((m) => (
                <option key={m.name} value={m.name}>
                  {m.label}
                </option>
              ))
            )}
          </select>
          {selectedMode && (
            <span className="mode-hint">
              {selectedMode.description}
              {selectedMode.writable === 'scratch only'
                ? ' Codebase stays read-only.'
                : ''}
            </span>
          )}
        </label>
        {error && <p className="form-error">{error}</p>}
      </div>
      <Composer
        running={false}
        busy={sending}
        onSubmit={handleSend}
        placeholder="Type your first message…"
      />
    </div>
  )
}
