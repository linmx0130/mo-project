import { useEffect, useState } from 'react'
import type { Mode, ModeInfo, ModelInfo, Session, SkillInfo, ToolInfo } from '../api'
import { createSession, getModels, getModes, getSkills, getTools } from '../api'
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
 *  a model picker, a mode picker, a foldable tool checkbox list and a
 *  foldable skill checkbox list (both folded by default — the page is busy
 *  enough without them) plus the chat composer. The backend session is only
 *  created on Send. All field values live in the shared `draft` (App) and
 *  are only cleared once a session is actually created. */
export default function DraftSession({
  draft,
  onDraftChange,
  defaultWorkdir,
  onCreated,
}: Props) {
  const [models, setModels] = useState<ModelInfo[]>([])
  const [modes, setModes] = useState<ModeInfo[]>([])
  const [tools, setTools] = useState<ToolInfo[]>([])
  const [skills, setSkills] = useState<SkillInfo[]>([])
  const [sending, setSending] = useState(false)
  const [error, setError] = useState<string | null>(null)

  // Load the configured models, modes, tools and skills once. The first
  // model is the default; it is pre-selected so "just type and send" uses
  // the default model. The modes are static (build is the default) but
  // fetched for their descriptions. The tool registry drives the checkbox
  // list (which tools may be disabled for this session) and the skill list
  // drives the force-load checkboxes. A draft's stored pick wins when it is
  // still valid (the lists may have changed between visits); otherwise it
  // is reset to the default. These checks are idempotent, so re-running on
  // every draft change is harmless.
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
    getSkills()
      .then((list) => {
        if (cancelled) return
        setSkills(list)
      })
      .catch(() => {
        // Gateway unreachable; no skills are force-loaded on send.
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

  useEffect(() => {
    if (skills.length === 0) return
    const picked = draft?.skills ?? []
    const valid = picked.filter((s) => skills.some((k) => k.name === s))
    if (valid.length !== picked.length) {
      onDraftChange({ skills: valid })
    }
  }, [skills, draft, onDraftChange])

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

  /** The skills the user force-loaded (draft state; `[]` = none). */
  const forcedSkills = draft?.skills ?? []

  /** Toggle one skill's checkbox: checking force-loads it for this
   *  session (its full SKILL.md goes into the system prompt), unchecking
   *  drops it. */
  const toggleSkill = (name: string, enabled: boolean) => {
    const next = enabled
      ? [...forcedSkills, name]
      : forcedSkills.filter((s) => s !== name)
    onDraftChange({ skills: next })
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
        forcedSkills,
      )
      onCreated(session)
      return true
    } catch (err) {
      setError(err instanceof Error ? err.message : String(err))
      setSending(false)
      return false
    }
  }

  const disabledCount = bannedTools.filter((t) =>
    tools.some((tool) => tool.name === t),
  ).length

  return (
    <div className="draft">
      <header className="session-header">
        <div>
          <h2>New session</h2>
          <p className="muted">
            Set the working directory, model, mode, tools and skills, then
            send your first message.
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
        {/* The tools and skills checkbox lists are folded by default: the
            page stays compact, and the defaults (all tools, no forced
            skills) are right for the common "just type and send" flow. */}
        <details className="field fold-section tools-field">
          <summary>
            <span className="fold-title">Tools</span>
            {disabledCount > 0 && (
              <span className="fold-count">{disabledCount} disabled</span>
            )}
          </summary>
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
        </details>
        <details className="field fold-section skills-field">
          <summary>
            <span className="fold-title">Skills</span>
            {forcedSkills.length > 0 && (
              <span className="fold-count">
                {forcedSkills.length} loaded
              </span>
            )}
          </summary>
          <p className="muted tools-hint">
            Force-load global skills for this session: their full SKILL.md
            instructions are injected into the system prompt, so the agent
            has them from the start (no load_skill call needed).
          </p>
          {skills.length === 0 ? (
            <p className="muted tools-hint">No skills discovered.</p>
          ) : (
            <div className="tools-grid">
              {skills.map((skill) => {
                const checked = forcedSkills.includes(skill.name)
                return (
                  <label
                    key={skill.name}
                    className="tool-option"
                    title={skill.description}
                  >
                    <input
                      type="checkbox"
                      checked={checked}
                      disabled={sending}
                      onChange={(e) =>
                        toggleSkill(skill.name, e.target.checked)
                      }
                    />
                    <span className="tool-name">{skill.name}</span>
                    <span className="tool-desc">{skill.description}</span>
                  </label>
                )
              })}
            </div>
          )}
        </details>
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
