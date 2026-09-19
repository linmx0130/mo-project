// Pure helpers behind the edit-title modal's "regenerate title" flow:
// the poll-for-change fallback (server answered 202) and the character
// counter matching the backend's 256-char cap.

export interface PollForChangeOptions {
  timeoutMs: number
  pollMs: number
  /** Returning true aborts the wait (e.g. the dialog unmounted). */
  isCancelled: () => boolean
}

/** Poll `fetchValue` until `hasChanged` reports a change, the timeout
 *  elapses, or `isCancelled` returns true. Resolves with the changed value,
 *  or null when the wait ended without one. Fetch errors propagate to the
 *  caller (the modal shows them in its error line). */
export async function pollForChange<T>(
  fetchValue: () => Promise<T>,
  hasChanged: (value: T) => boolean,
  { timeoutMs, pollMs, isCancelled }: PollForChangeOptions,
): Promise<T | null> {
  const deadline = Date.now() + timeoutMs
  for (;;) {
    await new Promise((r) => setTimeout(r, pollMs))
    if (isCancelled()) return null
    if (Date.now() >= deadline) return null
    const value = await fetchValue()
    if (isCancelled()) return null
    if (hasChanged(value)) return value
  }
}

/** Character count matching the backend's cap (`title::MAX_TITLE_CHARS`):
 *  `String.length` counts astral characters (emoji) as 2 UTF-16 units,
 *  the backend counts characters. */
export function charLength(s: string): number {
  return [...s].length
}
