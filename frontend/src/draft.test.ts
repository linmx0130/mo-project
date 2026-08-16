// Tests for the "New session" draft persistence (src/draft.ts), in
// particular the `bannedTools` field added for per-session tool selection:
// saved drafts round-trip it, drafts saved before the field existed default
// to an empty ban list (everything enabled), and malformed entries are
// dropped defensively.
import { beforeEach, describe, expect, it, vi } from 'vitest'
import { clearDraft, loadDraft, saveDraft } from './draft'

/** A minimal localStorage backed by a Map, stubbed on globalThis (the
 *  vitest node environment has no DOM storage). */
function stubStorage(): Map<string, string> {
  const store = new Map<string, string>()
  vi.stubGlobal('localStorage', {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, value),
    removeItem: (key: string) => void store.delete(key),
  })
  return store
}

beforeEach(() => {
  vi.unstubAllGlobals()
})

describe('loadDraft / saveDraft', () => {
  it('round-trips bannedTools (empty ban list = everything enabled)', () => {
    stubStorage()
    const draft = {
      workdir: '/work',
      model: 'm',
      mode: 'build' as const,
      bannedTools: [],
      text: 'hello',
    }
    saveDraft(draft)
    expect(loadDraft()).toEqual(draft)
  })

  it('round-trips a non-empty ban list (tools the user turned off)', () => {
    stubStorage()
    const draft = {
      workdir: '/work',
      model: 'm',
      mode: 'plan' as const,
      bannedTools: ['ask_user', 'spawn_subagent'],
      text: 'plan this',
    }
    saveDraft(draft)
    expect(loadDraft()).toEqual(draft)
  })

  it('drafts saved before tool selection existed default to an empty ban list', () => {
    const store = stubStorage()
    // The pre-tool-selection draft shape: no bannedTools key at all.
    store.set(
      'mo-new-session-draft',
      JSON.stringify({ workdir: '/w', model: 'm', mode: 'build', text: 'hi' }),
    )
    expect(loadDraft()).toEqual({
      workdir: '/w',
      model: 'm',
      mode: 'build',
      bannedTools: [],
      text: 'hi',
    })
  })

  it('drops non-string entries from a hand-edited ban list', () => {
    const store = stubStorage()
    store.set(
      'mo-new-session-draft',
      JSON.stringify({
        workdir: '/w',
        model: 'm',
        mode: 'build',
        bannedTools: ['ask_user', 42, null, 'load_skill'],
        text: 'hi',
      }),
    )
    const draft = loadDraft()
    expect(draft?.bannedTools).toEqual(['ask_user', 'load_skill'])
  })

  it('a non-array bannedTools (older malformed shape) resets to empty', () => {
    const store = stubStorage()
    store.set(
      'mo-new-session-draft',
      JSON.stringify({
        workdir: '/w',
        model: 'm',
        mode: 'build',
        bannedTools: 'ask_user',
        text: 'hi',
      }),
    )
    expect(loadDraft()?.bannedTools).toEqual([])
  })

  it('clearDraft removes the stored entry', () => {
    const store = stubStorage()
    saveDraft({
      workdir: '/w',
      model: 'm',
      mode: 'build',
      bannedTools: ['ask_user'],
      text: 'hi',
    })
    expect(store.has('mo-new-session-draft')).toBe(true)
    clearDraft()
    expect(store.has('mo-new-session-draft')).toBe(false)
    expect(loadDraft()).toBeNull()
  })
})
