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

export interface ToolCallInfo {
  id: string
  name: string
  arguments: string
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
   *  `build`). */
  | { kind: 'system_prompt'; content: string; mode: Mode }
  /** A notice that the session's mode changed since the last run, injected
   *  by the gateway right before a followup user message when the mode
   *  differs from the mode of the last run (at most one per followup).
   *  `mode` is the new mode; `content` is the full notice text. Rendered
   *  as a subtle notice row, not a chat bubble. */
  | { kind: 'mode_change'; mode: Mode; content: string }
  /** Streamed assistant text/reasoning chunk; the following `message` event
   *  carries the assembled content and replaces the delta-built preview. */
  | { kind: 'message_delta'; content: string; reasoning_content?: string | null }
  /** Streamed output of a running tool (bash); the following `tool_result`
   *  event carries the complete, capped output and replaces the preview. */
  | { kind: 'tool_output_delta'; id: string; name: string; output: string }
  /** Context length (API-reported prompt tokens) after an LLM call; the
   *  status bar shows the latest one. `context_window` is the model's
   *  configured window at session time, or null for unlimited. */
  | { kind: 'context_usage'; tokens: number; context_window?: number | null }

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

export function getSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}`)
}

export function createSession(
  workdir: string,
  prompt: string,
  model?: string,
  mode?: Mode,
): Promise<Session> {
  const body: Record<string, string> = { workdir, prompt }
  if (model) body.model = model
  if (mode) body.mode = mode
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
