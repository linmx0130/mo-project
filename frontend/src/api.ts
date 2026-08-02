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

export function getSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}`)
}

export function createSession(workdir: string, prompt: string): Promise<Session> {
  return http('/api/sessions', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ workdir, prompt }),
  })
}

export function getHistory(id: string, afterSeq?: number): Promise<JournalEvent[]> {
  const q = afterSeq !== undefined ? `?after_seq=${afterSeq}` : ''
  return http(`/api/sessions/${id}/history${q}`)
}

export function cancelSession(id: string): Promise<Session> {
  return http(`/api/sessions/${id}/cancel`, { method: 'POST' })
}
