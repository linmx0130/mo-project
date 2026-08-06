import type { Mode } from './api'

/** The "New session" form state, kept in a single place (App) so it
 *  survives navigating between sessions; mirrored to localStorage so it also
 *  survives reloads. It is only cleared when a session is actually created
 *  by sending the first message. */
export interface Draft {
  workdir: string
  model: string
  mode: Mode
  text: string
}

const DRAFT_KEY = 'mo-new-session-draft'

/** Restore the saved draft, or null when nothing is stored yet or the entry
 *  is malformed (older shape, hand-edited storage, …). */
export function loadDraft(): Draft | null {
  try {
    const raw = localStorage.getItem(DRAFT_KEY)
    if (!raw) return null
    const parsed = JSON.parse(raw) as Partial<Draft>
    if (typeof parsed.text !== 'string') return null
    return {
      workdir: typeof parsed.workdir === 'string' ? parsed.workdir : '',
      model: typeof parsed.model === 'string' ? parsed.model : '',
      mode:
        parsed.mode === 'build' || parsed.mode === 'plan' || parsed.mode === 'explore'
          ? parsed.mode
          : 'build',
      text: parsed.text,
    }
  } catch {
    return null
  }
}

export function saveDraft(draft: Draft): void {
  try {
    localStorage.setItem(DRAFT_KEY, JSON.stringify(draft))
  } catch {
    // Storage unavailable (private mode, quota, …); the in-memory state in
    // App still preserves the draft while the app is open.
  }
}

export function clearDraft(): void {
  try {
    localStorage.removeItem(DRAFT_KEY)
  } catch {
    // ignore
  }
}
