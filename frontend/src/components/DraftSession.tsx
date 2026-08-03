import { useState } from 'react'
import type { Session } from '../api'
import { createSession } from '../api'
import Composer from './Composer'

interface Props {
  /** Last used workdir, so consecutive new sessions don't retype it. */
  defaultWorkdir: string
  onCreated: (session: Session) => void
}

/** Placeholder session shown after clicking "New session": a workdir field
 *  plus the chat composer. The backend session is only created on Send. */
export default function DraftSession({ defaultWorkdir, onCreated }: Props) {
  const [workdir, setWorkdir] = useState(defaultWorkdir)
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const handleSend = async (text: string): Promise<boolean> => {
    if (!workdir.trim()) {
      setError('Working directory is required.')
      return false
    }
    setSending(true)
    setError(null)
    try {
      const session = await createSession(workdir.trim(), text)
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
            Set the working directory, then send your first message.
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
