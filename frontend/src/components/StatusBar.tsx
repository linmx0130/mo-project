import type { Mode, ModelInfo, SessionStatus } from '../api'

interface Props {
  status: SessionStatus
  /** Latest API-reported context length in tokens; null before the first
   *  LLM call (or when the provider reports no usage). */
  tokens: number | null
  /** The model's configured context window at session time; null =
   *  unlimited (only the current length is shown). */
  contextWindow: number | null
  /** The session's current mode (build / plan / explore). */
  mode: Mode
  /** True when the mode picker may be used (the session is not running);
   *  switching only changes the write sandbox of subsequent runs — the
   *  journaled system prompt never changes. */
  modeEnabled: boolean
  onSwitchMode: (mode: Mode) => void
  /** The configured models (GET /api/models), for the model picker. */
  models: ModelInfo[]
  /** The session's current model (its name from /api/models). */
  model: string
  /** True when the model picker may be used (the session is not running);
   *  switching only affects the next run — it is spawned with the new
   *  model and receives the full journal history. */
  modelEnabled: boolean
  onSwitchModel: (model: string) => void
}

const nf = new Intl.NumberFormat('en-US')

/** The session status bar pinned to the bottom of the session view.
 *
 *  Shows the mode picker (badge + switcher) with the model picker
 *  (badge-like select) side-by-side, the session status badge, and the
 *  context length in tokens (from the LLM API's `usage.prompt_tokens`,
 *  journaled by the worker as `context_usage` events). When a context
 *  window is configured the length is rendered against it with a thin
 *  progress bar; additional status items can be appended as siblings. */
export default function StatusBar({
  status,
  tokens,
  contextWindow,
  mode,
  modeEnabled,
  onSwitchMode,
  models,
  model,
  modelEnabled,
  onSwitchModel,
}: Props) {
  const pct =
    tokens !== null && contextWindow !== null && contextWindow > 0
      ? Math.min(100, Math.round((tokens / contextWindow) * 100))
      : null
  const high = pct !== null && pct >= 90
  // The session's model may not be in the (possibly changed) config list;
  // keep the select value valid by prepending it as a fallback option.
  const options =
    models.some((m) => m.name === model) ? models : [{ name: model, nickname: null, base_url: '', default: false }, ...models]

  return (
    <footer className="status-bar">
      <label className="mode-switch" title="Session mode — switching changes only the write sandbox of subsequent runs; the system prompt stays as journaled at the first run">
        <select
          className={`mode-select mode-${mode}`}
          value={mode}
          onChange={(e) => onSwitchMode(e.target.value as Mode)}
          disabled={!modeEnabled}
        >
          <option value="build">Build</option>
          <option value="plan">Plan</option>
          <option value="explore">Explore</option>
        </select>
      </label>
      <label
        className="model-switch"
        title="Session model — switching only affects the next run: it is spawned with the newly selected model and receives the full journal history"
      >
        <select
          className="mode-select model-select"
          value={model}
          onChange={(e) => onSwitchModel(e.target.value)}
          disabled={!modelEnabled || models.length === 0}
        >
          {options.length === 0 ? (
            <option value={model}>{model}</option>
          ) : (
            options.map((m) => (
              <option key={m.name} value={m.name}>
                {m.nickname ? `${m.nickname} (${m.name})` : m.name}
              </option>
            ))
          )}
        </select>
      </label>
      <span className={`badge badge-${status}`}>{status}</span>
      <span className="context-usage" title="Context length reported by the LLM API (prompt tokens)">
        {tokens === null ? (
          <>
            <span className="context-label">context</span>
            <span className="muted">— tokens</span>
          </>
        ) : contextWindow === null ? (
          <>
            <span className="context-label">context</span>
            {nf.format(tokens)} tokens
          </>
        ) : (
          <>
            <span className="context-label">context</span>
            {nf.format(tokens)} / {nf.format(contextWindow)} tokens
            {pct !== null && (
              <span className={`context-pct ${high ? 'context-high' : ''}`}>
                {pct}%
              </span>
            )}
            <span className={`context-track ${high ? 'context-high' : ''}`}>
              <span
                className="context-fill"
                style={{ width: `${pct ?? 0}%` }}
              />
            </span>
          </>
        )}
      </span>
    </footer>
  )
}
