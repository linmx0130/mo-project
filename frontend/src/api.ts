// Typed fetch wrappers matching the gateway DTOs (hand-duplicated, as the
// template does).

export type SessionStatus =
  | 'pending'
  | 'running'
  | 'completed'
  | 'failed'
  | 'cancelled'

/** Session modes (GET /api/modes): build (full access), plan and explore
 *  (codebase read-only, writes go to the session scratch dir). */
export type Mode = 'build' | 'plan' | 'explore'

export interface Session {
  id: string
  parent_id: string | null
  workdir: string
  prompt: string
  model: string
  status: SessionStatus
  /** The session mode: frames the journaled system prompt (first run) and
   *  the write sandbox of every run. Switchable via POST /:id/mode when the
   *  session is terminal — switching changes the sandbox, never the prompt. */
  mode: Mode
  /** The tools enabled for this session (their schemas are injected into
   *  the prompt). Chosen once in the "New session" form; bash + the file
   *  operations are always included. An empty list = all tools (legacy
   *  sessions created before tool selection existed). */
  tools: string[]
  pid: number | null
  journal_path: string
  created_at: string
  updated_at: string
  heartbeat_at: string | null
  error: string | null
}

/** Static gateway metadata (GET /api/meta). */
export interface Meta {
  /** Absolute path of the gateway's startup directory. */
  cwd: string
}

/** A configured model (GET /api/models). The first one is the default. */
export interface ModelInfo {
  /** Optional human-readable label from mo.toml. */
  nickname: string | null
  /** Model name; sent as `model` when creating a session. */
  name: string
  base_url: string
  /** True for the first (default) model in the config. */
  default: boolean
}

/** One built-in session mode (GET /api/modes). */
export interface ModeInfo {
  name: Mode
  label: string
  description: string
  /** Tool names available in this mode. */
  tools: string[]
  /** Where file mutations land: 'codebase' or 'scratch only'. */
  writable: string
}

/** One session tool (GET /api/tools): rendered as a checkbox in the
 *  "New session" form. `fixed` tools (bash + file operations) are always
 *  available and cannot be disabled; the toggleable ones may be turned off
 *  per session (their schemas are not injected into the prompt). */
export interface ToolInfo {
  name: string
  label: string
  description: string
  fixed: boolean
}

export interface ToolCallInfo {
  id: string
  name: string
  arguments: string
}

/** One selectable option of a clarification question (the `ask_user` tool):
 *  `option_title` is the precise, concise label the user picks (and the
 *  answer value when chosen); `option_text` further explains the option. */
export interface AskUserOption {
  option_title: string
  option_text: string
}

/** One clarification question (the `ask_user` tool). Stage 1 supports one
 *  question per call; `question_id` is assigned by the worker (`q1`) and
 *  keys the answer in the `ask_user_answered` answers object. */
export interface AskUserQuestion {
  question_id: string
  question_title: string
  question_text: string
  options: AskUserOption[]
}

export interface JournalMessage {
  role: string
  content: string
  reasoning_content?: string | null
  tool_call_id?: string | null
  tool_calls?: ToolCallInfo[] | null
}

export type JournalEventKind =
  | ({ kind: 'message' } & JournalMessage)
  | { kind: 'tool_call_start'; id: string; name: string; arguments: string }
  | { kind: 'tool_result'; id: string; name: string; ok: boolean; output: string }
  | { kind: 'status_change'; status: SessionStatus; error?: string | null }
  /** The session's system prompt, journaled by the worker on the first run
   *  and reused verbatim on every later run. Rendered as session metadata
   *  (never as a chat message). `mode` is the mode the session ran under
   *  when the prompt was journaled — together with `mode_change` events it
   *  is the journal's mode marker (legacy events without it default to
   *  `build`). `model` is the model the session ran under when the prompt
   *  was journaled — together with `model_change` events it is the
   *  journal's model marker (legacy events lack it). */
  | { kind: 'system_prompt'; content: string; mode: Mode; model?: string }
  /** A notice that the session's mode changed since the last run, injected
   *  by the gateway right before a followup user message when the mode
   *  differs from the mode of the last run (at most one per followup).
   *  `mode` is the new mode; `content` is the full notice text. Rendered
   *  as a subtle notice row, not a chat bubble. Also journaled by
   *  `POST /:id/mode/approve` — approving a `mode_change_request` switches
   *  the mode and injects this single notice to continue the run. */
  | { kind: 'mode_change'; mode: Mode; content: string }
  /** A notice that the session's model changed since the last run, injected
   *  by the gateway right before a followup user message (or the
   *  mode-approve continuation) when the model differs from the model of
   *  the last run — at most one per followup; several switches before a
   *  single followup collapse into one event describing the final model.
   *  `from` is the model of the last run; `to` is the session's current
   *  model. Flow metadata only (the next run is spawned with `to`
   *  regardless), rendered as a subtle notice row. */
  | { kind: 'model_change'; from: string; to: string }
  /** The agent's request, via the `request_mode_change` tool, to switch the
   *  session's mode. `message` is the model's request text in the user's
   *  language. While the latest such request is pending (no `mode_change` /
   *  `mode_change_request_declined` after it), the frontend freezes the
   *  composer and offers Agree / Reject. */
  | { kind: 'mode_change_request'; mode: Mode; message: string }
  /** The user rejected a pending `mode_change_request` (POST /:id/mode/
   *  reject); it resolves the request — no mode switch happens. Rendered
   *  as a passive notice row. */
  | { kind: 'mode_change_request_declined'; mode: Mode }
  /** The agent asked the user a clarification question (the `ask_user`
   *  tool). While the latest such request is pending (no
   *  `ask_user_answered` after it), the frontend freezes the composer and
   *  shows a question card: every option plus a free-text input box. */
  | { kind: 'ask_user_request'; question: AskUserQuestion }
  /** The user answered a pending `ask_user_request` (POST /:id/ask/answer):
   *  `answers` is a JSON object keyed by question_id whose values are the
   *  chosen option's title or the user's typed text. It resolves the
   *  request; the worker respawns and the model receives the answers as a
   *  user message. Rendered as a passive notice row. */
  | { kind: 'ask_user_answered'; answers: Record<string, string> }
  /** Streamed assistant text/reasoning chunk; the following `message` event
   *  carries the assembled content and replaces the delta-built preview. */
  | { kind: 'message_delta'; content: string; reasoning_content?: string | null }
  /** Streamed output of a running tool (bash); the following `tool_result`
   *  event carries the complete, capped output and replaces the preview. */
  | { kind: 'tool_output_delta'; id: string; name: string; output: string }
  /** The parent worker spawned a subagent session: `child_id` is the
   *  subagent's session id and `tool_call_id` links it to the parent's
   *  `spawn_subagent` tool block, which the UI renders with a "view
   *  subagent" affordance opening a read-only modal of the child's
   *  messages. `mode` is the mode the subagent runs under. */
  | { kind: 'subagent_started'; child_id: string; tool_call_id: string; mode: Mode }
  /** Context length (API-reported prompt tokens) after an LLM call; the
   *  status bar shows the latest one. `context_window` is the model's
   *  configured window at session time, or null for unlimited. */
  | { kind: 'context_usage'; tokens: number; context_window?: number | null }
  /** Context compression: the model's handoff prompt, journaled when the
   *  session's context length crossed the configured fraction of the
   *  context window. Everything before this event is no longer sent to the
   *  model (the handoff text becomes the compressed context's first user
   *  message), but stays in the journal — and in this timeline — for the
   *  user to inspect. `mode` is the mode the session ran under when the
   *  handoff was generated. Rendered as a notice row. */
  | { kind: 'handoff'; content: string; mode: Mode }

export interface JournalEvent {
  /** null for synthesized SSE status events (not part of the journal) */
  seq: number | null
  ts: string
  synthetic?: boolean
  kind: JournalEventKind
}

async function http<T>(url: string, init?: RequestInit): Promise<T> {
  const res = await fetch(url, init)
  if (!res.ok) {
    let message = `HTTP ${res.status}`
    try {
      const body = (await res.json()) as { error?: string }
      if (body.error) message = body.error
    } catch {
      // keep the status-based message
    }
    throw new Error(message)
  }
  // 204 No Content (e.g. session deletion) has no JSON body.
  if (res.status === 204) return undefined as T
  return res.json() as Promise<T>
}

export function listSessions(): Promise<Session[]> {
  return http('/api/sessions')
}

export function getMeta(): Promise<Meta> {
  return http('/api/meta')
}

/** The configured models (from mo.toml); the first one is the default. */
export function getModels(): Promise<ModelInfo[]> {
  return http('/api/models')
}

/** The built-in session modes (build / plan / explore). */
export function getModes(): Promise<ModeInfo[]> {
  return http('/api/modes')
}

/** The session tool registry (GET /api/tools) for the "New session"
 *  checkbox list: name, label, description, and whether the tool is fixed
 *  (always available) or toggleable (may be disabled per session). */
export function getTools(): Promise<ToolInfo[]> {
  return http('/api/tools')
}

export function getSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}`)
}

/** Create a session. `bannedTools` lists the *toggleable* tools the user
 *  turned off in the "New session" form; disabled tools' schemas are not
 *  injected into the prompt and the worker refuses to execute them. Fixed
 *  tools (bash + file operations) are always available and cannot be
 *  banned; absent/empty bans nothing (all tools enabled). */
export function createSession(
  workdir: string,
  prompt: string,
  model?: string,
  mode?: Mode,
  bannedTools?: string[],
): Promise<Session> {
  const body: Record<string, string | string[]> = { workdir, prompt }
  if (model) body.model = model
  if (mode) body.mode = mode
  if (bannedTools && bannedTools.length > 0) body.banned_tools = bannedTools
  return http('/api/sessions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
}

/** Switch a terminal session's mode. The journaled system prompt never
 *  changes; only the write-sandbox policy of subsequent runs does. */
export function switchMode(id: string, mode: Mode): Promise<Session> {
  return http(`/api/sessions/${id}/mode`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ mode }),
  })
}

/** Switch a terminal session's model. Only the next run is affected: the
 *  worker respawned for the next followup is spawned with the new model,
 *  and the backend journals a `model_change` notice right before that run
 *  when the model differs from the model of the last run. */
export function switchModel(id: string, model: string): Promise<Session> {
  return http(`/api/sessions/${id}/model`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ model }),
  })
}

/** Approve a pending `mode_change_request` (the agent asked, via the
 *  request_mode_change tool, to switch the session's mode): switches the
 *  session's mode to the requested one and continues the run with a single
 *  mode-change message. */
export function approveModeChange(id: string): Promise<Session> {
  return http(`/api/sessions/${id}/mode/approve`, { method: 'POST' })
}

/** Reject a pending `mode_change_request`: resolves the request without
 *  switching the mode (nothing is sent to the agent). */
export function rejectModeChange(id: string): Promise<Session> {
  return http(`/api/sessions/${id}/mode/reject`, { method: 'POST' })
}

/** Answer a pending `ask_user_request` (the agent asked a clarification
 *  question via the ask_user tool): `answers` maps question_id to the
 *  chosen option's title or the user's typed text. The backend journals the
 *  answers and resumes the run. */
export function answerAskUser(
  id: string,
  answers: Record<string, string>,
): Promise<Session> {
  return http(`/api/sessions/${id}/ask/answer`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ answers }),
  })
}

/** Send a followup message to a terminal session; the worker respawns and
 *  continues the conversation from the journal history. */
export function postMessage(id: string, content: string): Promise<Session> {
  return http(`/api/sessions/${id}/messages`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ content }),
  })
}

export function getHistory(id: string, afterSeq?: number): Promise<JournalEvent[]> {
  const q = afterSeq !== undefined ? `?after_seq=${afterSeq}` : ''
  return http(`/api/sessions/${id}/history${q}`)
}

export function cancelSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}/cancel`, { method: 'POST' })
}

/** Permanently delete a session (worker killed, files + DB row removed). */
export function deleteSession(id: string): Promise<void> {
  return http(`/api/sessions/${id}`, { method: 'DELETE' })
}
