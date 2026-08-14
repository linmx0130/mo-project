// Regression tests for the timeline folding (src/timeline.ts). The
// headline case is an *interrupted run followed by a continue*: when the
// worker's LLM connection drops mid-stream (e.g. the OS slept), the journal
// ends with orphan `message_delta` events and no final assistant `message`
// event. The delta-built preview must be closed at the turn boundary (the
// user's followup / the terminal status change) so the next run's deltas
// start a fresh message instead of being appended onto the interrupted one.
import { describe, expect, it } from 'vitest'
import type { JournalEvent, JournalEventKind, SessionStatus, ToolCallInfo } from './api'
import { buildTimeline, type MessageBlock } from './timeline'

let seq = 0
function ev(kind: JournalEventKind): JournalEvent {
  return { seq: seq++, ts: '2026-01-01T00:00:00Z', kind }
}
const userMsg = (content: string) => ev({ kind: 'message', role: 'user', content })
const asstMsg = (content: string, toolCalls?: ToolCallInfo[]) =>
  ev({ kind: 'message', role: 'assistant', content, tool_calls: toolCalls })
const delta = (content: string) => ev({ kind: 'message_delta', content })
const status = (status: SessionStatus, error?: string) =>
  ev({ kind: 'status_change', status, error })
const usage = (tokens = 50) => ev({ kind: 'context_usage', tokens })
const toolStart = (id: string, name = 'bash', arguments_ = '{}') =>
  ev({ kind: 'tool_call_start', id, name, arguments: arguments_ })
const toolOut = (id: string, output: string) =>
  ev({ kind: 'tool_output_delta', id, name: 'bash', output })
const toolRes = (id: string, ok = true, output = 'done') =>
  ev({ kind: 'tool_result', id, name: 'bash', ok, output })

/** The assistant message rows in item order. */
function assistantMessages(
  items: ReturnType<typeof buildTimeline>,
): MessageBlock[] {
  return items
    .filter((i) => i.type === 'message' && i.message.role === 'assistant')
    .map((i) => (i.type === 'message' ? i.message : null))
    .filter((m): m is MessageBlock => m !== null)
}

/** The user message contents in item order. */
function userMessages(items: ReturnType<typeof buildTimeline>): string[] {
  return items
    .filter((i) => i.type === 'message' && i.message.role === 'user')
    .map((i) => (i.type === 'message' ? i.message.content : ''))
}

describe('buildTimeline — interrupted run followed by a continue', () => {
  it('starts a fresh message for the new run instead of appending to the interrupted one', () => {
    const items = buildTimeline([
      userMsg('Do X'),
      status('running'),
      delta('Let me'),
      delta(' look at it'),
      status('failed', 'LLM connection dropped'),
      userMsg('Continue'),
      status('running'),
      delta('Sure —'),
      delta(' continuing now'),
      asstMsg('Sure — continuing now'),
    ])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(2)
    // The interrupted preview keeps its partial text, is closed (not
    // streaming) and is marked truncated.
    expect(assistant[0].content).toBe('Let me look at it')
    expect(assistant[0].streaming).toBe(false)
    expect(assistant[0].truncated).toBe(true)
    // The new run's deltas assembled into their own message, finalized by
    // their own `message` event.
    expect(assistant[1].content).toBe('Sure — continuing now')
    expect(assistant[1].streaming).toBe(false)
    expect(assistant[1].truncated).toBeFalsy()
    // Order: user → assistant(interrupted) → user(continue) → assistant(new).
    expect(userMessages(items)).toEqual(['Do X', 'Continue'])
    const indexOf = (m: MessageBlock) =>
      items.findIndex((i) => i.type === 'message' && i.message === m)
    expect(indexOf(assistant[0])).toBeLessThan(indexOf(assistant[1]))
    const continueIdx = items.findIndex(
      (i) => i.type === 'message' && i.message.role === 'user' && i.message.content === 'Continue',
    )
    expect(continueIdx).toBeGreaterThan(indexOf(assistant[0]))
    expect(continueIdx).toBeLessThan(indexOf(assistant[1]))
  })

  it('splits runs even without a terminal status event between them', () => {
    // The status event may be missing or synthesized with `seq: null`; the
    // user message itself is the boundary.
    const items = buildTimeline([
      userMsg('Do X'),
      delta('partial '),
      userMsg('Continue'),
      delta('fresh'),
      asstMsg('fresh'),
    ])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(2)
    expect(assistant[0].content).toBe('partial ')
    expect(assistant[0].truncated).toBe(true)
    expect(assistant[1].content).toBe('fresh')
    expect(userMessages(items)).toEqual(['Do X', 'Continue'])
  })

  it('closes the interrupted preview when the run dies even before a followup', () => {
    const items = buildTimeline([
      userMsg('Do X'),
      delta('thinking'),
      status('failed', 'worker died'),
    ])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(1)
    expect(assistant[0].content).toBe('thinking')
    expect(assistant[0].streaming).toBe(false)
    expect(assistant[0].truncated).toBe(true)
  })

  it('treats a mode_change notice (approved request) as a boundary too', () => {
    const items = buildTimeline([
      userMsg('Do X'),
      delta('old '),
      ev({ kind: 'mode_change', mode: 'build', content: 'mode notice' }),
      delta('new run'),
      asstMsg('new run'),
    ])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(2)
    expect(assistant[0].content).toBe('old ')
    expect(assistant[0].truncated).toBe(true)
    expect(assistant[1].content).toBe('new run')
  })
})

describe('buildTimeline — normal streaming (unchanged behavior)', () => {
  it('assembles deltas into one message and finalizes it with the canonical text', () => {
    // Real journal order: deltas, then the worker journals `context_usage`
    // *before* the final `message` event — the open preview must survive
    // the `context_usage` event so the final message still finalizes it in
    // place (not as a duplicate row).
    const items = buildTimeline([delta('He'), delta('llo'), usage(), asstMsg('Hello')])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(1)
    expect(assistant[0].content).toBe('Hello')
    expect(assistant[0].streaming).toBe(false)
    expect(assistant[0].truncated).toBeFalsy()
  })

  it('repairs a retried call: the final message replaces the partial preview', () => {
    // A failed attempt leaves orphan deltas; the successful retry's final
    // `message` event must replace the preview, not be appended.
    const items = buildTimeline([delta('half'), usage(), asstMsg('complete answer')])

    const assistant = assistantMessages(items)
    expect(assistant).toHaveLength(1)
    expect(assistant[0].content).toBe('complete answer')
  })

  it('drops a bare tool-call wrapper and keeps the tool blocks', () => {
    const items = buildTimeline([
      asstMsg('', [
        { id: 'call_1', name: 'bash', arguments: '{}' },
        { id: 'call_2', name: 'bash', arguments: '{}' },
      ]),
      toolStart('call_1'),
      toolStart('call_2'),
      toolOut('call_1', 'out'),
      toolRes('call_2'),
      toolRes('call_1', true, 'final'),
    ])

    expect(assistantMessages(items)).toHaveLength(0)
    const tools = items.filter((i) => i.type === 'tool')
    expect(tools).toHaveLength(2)
  })

  it('keeps a sibling tool block streaming when another tool result lands (parallel calls)', () => {
    const items = buildTimeline([
      asstMsg('', [
        { id: 'call_1', name: 'bash', arguments: '{}' },
        { id: 'call_2', name: 'bash', arguments: '{}' },
      ]),
      toolStart('call_1'),
      toolStart('call_2'),
      toolOut('call_1', 'a-out'),
      toolOut('call_2', 'b-out'),
      toolRes('call_1'),
    ])

    const tools = items.filter((i) => i.type === 'tool')
    expect(tools).toHaveLength(2)
    const a = tools.find((t) => t.type === 'tool' && t.block.id === 'call_1')
    const b = tools.find((t) => t.type === 'tool' && t.block.id === 'call_2')
    expect(a?.type === 'tool' ? a.block.streaming : undefined).toBe(false)
    expect(a?.type === 'tool' ? a.block.ok : undefined).toBe(true)
    // The second block is still running — its result never landed.
    expect(b?.type === 'tool' ? b.block.streaming : undefined).toBe(true)
  })
})

describe('buildTimeline — tool blocks at turn boundaries', () => {
  it('closes a streaming tool block when the run dies before its result', () => {
    const items = buildTimeline([
      asstMsg('', [{ id: 'call_1', name: 'bash', arguments: '{}' }]),
      toolStart('call_1'),
      toolOut('call_1', 'partial output'),
      status('failed', 'worker died'),
    ])

    const tool = items.find((i) => i.type === 'tool')
    expect(tool).toBeDefined()
    if (tool?.type === 'tool') {
      expect(tool.block.streaming).toBe(false)
      expect(tool.block.ok).toBeUndefined()
    }
  })

  it('does not close a streaming sibling while a subagent_started event lands', () => {
    // `subagent_started` is journaled mid-execution by the spawn_subagent
    // tool; a sibling bash block may still legitimately be streaming and
    // must keep its running badge until a true boundary.
    const items = buildTimeline([
      asstMsg('', [{ id: 'call_a', name: 'bash', arguments: '{}' }]),
      toolStart('call_a'),
      toolOut('call_a', 'streaming'),
      ev({ kind: 'subagent_started', child_id: 'child-1', tool_call_id: 'call_a', mode: 'build' }),
    ])

    const tool = items.find((i) => i.type === 'tool')
    expect(tool).toBeDefined()
    if (tool?.type === 'tool') {
      expect(tool.block.streaming).toBe(true)
      expect(tool.block.childId).toBe('child-1')
    }
  })

  it('closes a streaming tool block at the user-followup boundary', () => {
    const items = buildTimeline([
      asstMsg('', [{ id: 'call_1', name: 'bash', arguments: '{}' }]),
      toolStart('call_1'),
      toolOut('call_1', 'partial output'),
      userMsg('Continue'),
      delta('new run'),
      asstMsg('new run'),
    ])

    const tool = items.find((i) => i.type === 'tool')
    expect(tool).toBeDefined()
    if (tool?.type === 'tool') {
      expect(tool.block.streaming).toBe(false)
    }
  })
})
