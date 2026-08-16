import { useEffect, useState } from 'react'
import type { Mode, ModeInfo, ModelInfo, Session, ToolInfo } from '../api'
import { createSession, getModels, getModes, getTools } from '../api'
import type { Draft } from '../draft'
import Composer from './Composer'

interface Props {
  /** The shared new-session form state (owned by App so it survives
   *  navigating between sessions); null until the first change. */
  draft: Draft | null
  /** Merge a patch into the draft; the first patch materializes it from
   *  defaults. */
  onDraftChange: (patch: Partial<Draft>) => void
  /** Last used workdir, shown until the draft carries its own. */
  defaultWorkdir: string
  onCreated: (session: Session) => void
}

/** Placeholder session shown after clicking "New session": a workdir field,
 *  a model picker, a mode picker, a tool checkbox list plus the chat
 *  composer. The backend session is only created on Send. All field values
 *  live in the shared `draft` (App) and are only cleared once a session is
 *  actually created. */
export default function DraftSession({
  draft,
  onDraftChange,
  defaultWorkdir,
  onCreated,
}: Props) {
  const [models, setModels] = useState<ModelInfo[]>([])
  const [modes, setModes] = useState<ModeInfo[]>([])
  const [tools, setTools] = useState<ToolInfo[]>([])
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Load the configured models, modes and tools once. The first model is
  // the default; it is pre-selected so "just type and send" uses the
  // default model. The modes are static (build is the default) but fetched
  // for their descriptions. The tool registry drives the checkbox list
  // (which tools may be disabled for this session). A draft's stored pick
  // wins when it is still valid (the lists may have changed between
  // visits); otherwise it is reset to the default. These checks are
  // idempotent, so re-running on every draft change is harmless.
  useEffect(() => {
    let cancelled = false
    getModels()
      .then((list) => {
        if (cancelled) return
        setModels(list)
      })
      .catch(() => {
        // Gateway unreachable; sending will surface the error.
      })
    getModes()
      .then((list) => {
        if (cancelled) return
        setModes(list)
      })
      .catch(() => {
        // Gateway unreachable; build stays selected.
      })
    getTools()
      .then((list) => {
        if (cancelled) return
        setTools(list)
      })
      .catch(() => {
        // Gateway unreachable; the backend default (all tools enabled)
        // applies on send.
      })
    return () => {
      cancelled = true
    }
  }, [])

  useEffect(() => {
    if (models.length === 0) return
    if (!draft?.model || !models.some((m) => m.name === draft.model)) {
      onDraftChange({ model: models[0].name })
    }
  }, [models, draft, onDraftChange])

  useEffect(() => {
    if (modes.length === 0) return
    if (!draft?.mode || !modes.some((m) => m.name === draft.mode)) {
      onDraftChange({ mode: 'build' })
    }
  }, [modes, draft, onDraftChange])

  const selectedMode = modes.find((m) => m.name === (draft?.mode ?? 'build'))

  /** The toggleable tools the user turned off (draft state; `[]` = all
   *  enabled, the default). Fixed tools are never banned. */
  const bannedTools = draft?.bannedTools ?? []

  /** Toggle one tool's checkbox: checking removes it from the ban list,
   *  unchecking adds it. */
  const toggleTool = (name: string, enabled: boolean) => {
    const next = enabled
      ? bannedTools.filter((t) => t !== name)
      : [...bannedTools, name]
    onDraftChange({ bannedTools: next })
  }

  const handleSend = async (text: string): Promise<boolean> => {
    if (!draft?.workdir.trim()) {
      setError('Working directory is required.')
      return false
    }
    setSending(true)
    setError(null)
    try {
      const session = await createSession(
        draft.workdir.trim(),
        text,
        draft.model || undefined,
        draft.mode,
        bannedTools,
      )
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
            Set the working directory, model, mode and tools, then send your
            first message.
          </p>
        </div>
      </header>
      <div className="draft-body">
        <label className="field">
          <span className="field-label">Working directory</span>
          <input
            value={draft?.workdir ?? defaultWorkdir}
            onChange={(e) => onDraftChange({ workdir: e.target.value })}
            placeholder="/absolute/path/to/workdir"
            spellCheck={false}
            disabled={sending}
          />
        </label>
        <label className="field">
          <span className="field-label">Model</span>
          <select
            value={draft?.model ?? ''}
            onChange={(e) => onDraftChange({ model: e.target.value })}
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
            value={draft?.mode ?? 'build'}
            onChange={(e) => onDraftChange({ mode: e.target.value as Mode })}
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
        <fieldset className="field tools-field">
          <legend className="field-label">Tools</legend>
          <p className="muted tools-hint">
            Bash and file tools are always available. Disable optional tools
            for this session — the agent won&apos;t be able to use them.
          </p>
          <div className="tools-grid">
            {tools.map((tool) => {
              const checked = tool.fixed || !bannedTools.includes(tool.name)
              return (
                <label
                  key={tool.name}
                  className={
                    tool.fixed ? 'tool-option tool-fixed' : 'tool-option'
                  }
                  title={tool.description}
                >
                  <input
                    type="checkbox"
                    checked={checked}
                    disabled={tool.fixed || sending}
                    onChange={(e) => toggleTool(tool.name, e.target.checked)}
                  />
                  <span className="tool-name">{tool.label}</span>
                  <span className="tool-desc">{tool.description}</span>
                </label>
              )
            })}
          </div>
        </fieldset>
        {error && <p className="form-error">{error}</p>}
      </div>
      <Composer
        running={false}
        busy={sending}
        value={draft?.text ?? ''}
        onTextChange={(text) => onDraftChange({ text })}
        onSubmit={handleSend}
        placeholder="Type your first message…"
      />
    </div>
  )
}
