// Tests for src/title.ts — the poll-for-change helper behind the edit-title
// modal's regenerate fallback (server answered 202), and the character
// counter matching the backend's 256-char cap (chars, not UTF-16 units).
import { describe, expect, it } from 'vitest'
import { charLength, pollForChange } from './title'

const opts = { timeoutMs: 300, pollMs: 5, isCancelled: () => false }

describe('pollForChange', () => {
  it('resolves with the first value the change check accepts', async () => {
    let n = 0
    const value = await pollForChange(
      async () => ++n,
      (v) => v >= 3,
      opts,
    )
    expect(value).toBe(3)
  })

  it('resolves null when nothing changes before the timeout', async () => {
    const fetchCount = { n: 0 }
    const value = await pollForChange(
      async () => {
        fetchCount.n++
        return 'same'
      },
      (v) => v !== 'same',
      opts,
    )
    expect(value).toBeNull()
    expect(fetchCount.n).toBeGreaterThan(1)
  })

  it('resolves null when cancelled mid-wait', async () => {
    let cancelled = false
    const timer = setTimeout(() => {
      cancelled = true
    }, 30)
    const value = await pollForChange(
      async () => 'same',
      (v) => v !== 'same',
      { ...opts, isCancelled: () => cancelled },
    )
    clearTimeout(timer)
    expect(value).toBeNull()
  })

  it('detects changes the prompt alone cannot (updated_at fingerprint)', async () => {
    // A temperature-0 regeneration may return the identical title; the
    // modal also passes the row's `updated_at`, which the backend bumps on
    // every write. A row with an unchanged prompt but a new `updated_at`
    // must end the wait.
    const row = { prompt: 'same title', updated_at: '2026-09-19T00:00:01Z' }
    const value = await pollForChange(
      async () => row,
      (s) => s.prompt !== 'same title' || s.updated_at !== '2026-09-19T00:00:00Z',
      opts,
    )
    expect(value).toEqual(row)
  })

  it('propagates fetch errors to the caller', async () => {
    await expect(
      pollForChange(
        async () => {
          throw new Error('boom')
        },
        () => false,
        opts,
      ),
    ).rejects.toThrow('boom')
  })
})

describe('charLength', () => {
  it('counts characters, not UTF-16 units', () => {
    expect(charLength('abc')).toBe(3)
    // Astral-plane characters are surrogate pairs: String.length is 2.
    expect(charLength('🎉')).toBe(1)
    expect(charLength('标题')).toBe(2)
  })
})
