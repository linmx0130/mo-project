// Typed fetch wrappers matching the gateway DTOs (hand-duplicated, as the
// template does).

export type SessionStatus =
  | 'pending'
  | 'running'
  | 'completed'
  | 'failed'
  | 'cancelled'

export interface Session {
  id: string
  parent_id: string | null
  workdir: string
  prompt: string
  model: string
  status: SessionStatus
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
  /** Streamed assistant text/reasoning chunk; the following `message` event
   *  carries the assembled content and replaces the delta-built preview. */
  | { kind: 'message_delta'; content: string; reasoning_content?: string | null }
  /** Streamed output of a running tool (bash); the following `tool_result`
   *  event carries the complete, capped output and replaces the preview. */
  | { kind: 'tool_output_delta'; id: string; name: string; output: string }

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

export function getSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}`)
}

export function createSession(
  workdir: string,
  prompt: string,
  model?: string,
): Promise<Session> {
  return http('/api/sessions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(model ? { workdir, prompt, model } : { workdir, prompt }),
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
