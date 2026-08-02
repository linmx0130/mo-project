import { useState } from 'react'
import type { FormEvent } from 'react'
import type { Session } from '../api'
import { createSession } from '../api'

interface Props {
  onCreated: (session: Session) => void
}

export default function NewSessionForm({ onCreated }: Props) {
  const [workdir, setWorkdir] = useState('')
  const [prompt, setPrompt] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    if (!workdir.trim() || !prompt.trim() || busy) return
    setBusy(true)
    setError(null)
    try {
      const session = await createSession(workdir.trim(), prompt.trim())
      setPrompt('')
      onCreated(session)
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
    } finally {
      setBusy(false)
    }
  }

  return (
    <form className="new-session" onSubmit={submit}>
      <input
        value={workdir}
        onChange={(e) => setWorkdir(e.target.value)}
        placeholder="/absolute/path/to/workdir"
        spellCheck={false}
      />
      <textarea
        value={prompt}
        onChange={(e) => setPrompt(e.target.value)}
        placeholder="Describe a task for the agent…"
        rows={3}
      />
      <button
        type="submit"
        disabled={busy || !workdir.trim() || !prompt.trim()}
      >
        {busy ? 'Starting…' : 'New session'}
      </button>
      {error && <p className="form-error">{error}</p>}
    </form>
  )
}
