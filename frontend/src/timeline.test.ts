// Regression tests for the timeline folding (src/timeline.ts). The
// headline case is an *interrupted run followed by a continue*: when the
// worker's LLM connection drops mid-stream (e.g. the OS slept), the journal
// ends with orphan `message_delta` events and no final assistant `message`
// event. The delta-built preview must be closed at the turn boundary (the
// user's followup / the terminal status change) so the next run's deltas
// start a fresh message instead of being appended onto the interrupted one.
import { describe, expect, it } from 'vitest'
import type { JournalEvent, JournalEventKind, SessionStatus, ToolCallInfo } from './api'
import {
  TimelineFolder,
  buildTimeline,
  type MessageBlock,
  type ToolBlock,
} from './timeline'

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

  it('treats a model_change notice as a boundary too', () => {
    const items = buildTimeline([
      userMsg('Do X'),
      delta('old '),
      ev({ kind: 'model_change', from: 'model-a', to: 'model-b' }),
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

  it('does not close a streaming sibling while a permission_request event lands', () => {
    // `permission_request` is journaled mid-execution by a file tool; a
    // sibling bash block may still legitimately be streaming and must keep
    // its running badge until a true boundary.
    const items = buildTimeline([
      asstMsg('', [{ id: 'call_a', name: 'bash', arguments: '{}' }]),
      toolStart('call_a'),
      toolOut('call_a', 'streaming'),
      ev({
        kind: 'permission_request',
        request_id: 'p1',
        tool: 'read_file',
        operation: 'read',
        path: '/etc/hostname',
      }),
    ])

    const tool = items.find((i) => i.type === 'tool')
    expect(tool).toBeDefined()
    if (tool?.type === 'tool') {
      expect(tool.block.streaming).toBe(true)
    }
    // The permission request itself is rendered as a notice row.
    const notices = items.filter((i) => i.type === 'event')
    expect(notices).toHaveLength(1)
  })

  it('treats a permission_answered event as a true turn boundary', () => {
    // `permission_answered` is journaled by the gateway between runs: any
    // stale streaming tool block from an interrupted earlier run is closed.
    const items = buildTimeline([
      asstMsg('', [{ id: 'call_a', name: 'bash', arguments: '{}' }]),
      toolStart('call_a'),
      toolOut('call_a', 'partial output'),
      ev({
        kind: 'permission_answered',
        request_id: 'p1',
        tool: 'read_file',
        operation: 'read',
        path: '/etc/hostname',
        allowed: true,
      }),
    ])

    const tool = items.find((i) => i.type === 'tool')
    expect(tool).toBeDefined()
    if (tool?.type === 'tool') {
      expect(tool.block.streaming).toBe(false)
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

describe('buildTimeline — messages with images', () => {
  it('keeps the images reference on the user message block', () => {
    const items = buildTimeline([
      ev({
        kind: 'message',
        role: 'user',
        content: 'what is this?',
        images: [{ path: 'images/abc.png', mime: 'image/png' }],
      }),
    ])
    const user = items.find(
      (i) => i.type === 'message' && i.message.role === 'user',
    )
    expect(user?.type).toBe('message')
    if (user?.type !== 'message') return
    expect(user.message.images).toEqual([
      { path: 'images/abc.png', mime: 'image/png' },
    ])
    // An image-only user message (no text) is still rendered.
    const items2 = buildTimeline([
      ev({
        kind: 'message',
        role: 'user',
        content: '',
        images: [{ path: 'images/abc.png', mime: 'image/png' }],
      }),
    ])
    expect(
      items2.some((i) => i.type === 'message' && i.message.role === 'user'),
    ).toBe(true)
  })
})

describe('buildTimeline — the journal seq on message blocks', () => {
  it('exposes the seq of a user message, so a fork can cut before it', () => {
    const first = userMsg('first')
    const second = userMsg('second')
    const items = buildTimeline([first, asstMsg('answer'), second])
    const users = items.filter(
      (i) => i.type === 'message' && i.message.role === 'user',
    )
    if (users[0]?.type !== 'message' || users[1]?.type !== 'message') {
      throw new Error('expected two user message items')
    }
    expect(users[0].message.seq).toBe(first.seq)
    expect(users[1].message.seq).toBe(second.seq)
  })

  it('finalizes a delta-built preview with the canonical message seq', () => {
    const message = asstMsg('final')
    const items = buildTimeline([delta('fi'), delta('nal'), message])
    const [asst] = assistantMessages(items)
    expect(asst.content).toBe('final')
    expect(asst.seq).toBe(message.seq)
  })
})

describe('TimelineFolder — incremental folding (SSE events folded onto history)', () => {
  it('matches buildTimeline when events are pushed one by one', () => {
    const events = [
      userMsg('hi'),
      status('running'),
      delta('he'),
      delta('llo'),
      asstMsg('hello'),
      toolStart('c1'),
      toolOut('c1', 'partial'),
      toolRes('c1'),
    ]
    const folder = new TimelineFolder()
    for (const e of events) folder.push(e)
    expect(folder.items()).toEqual(buildTimeline(events))
  })

  it('patches a message block in place by seq (lazy reasoning hydration)', () => {
    const message = asstMsg('hello')
    const seq = message.seq as number
    const folder = new TimelineFolder()
    folder.push(message)
    expect(folder.messageAt(seq)?.content).toBe('hello')
    expect(folder.messageAt(seq)?.reasoning_content).toBeFalsy()
    folder.patchMessage(seq, { reasoning_content: 'deep thoughts' })
    expect(folder.messageAt(seq)?.reasoning_content).toBe('deep thoughts')
  })

  it('messageAt returns null and patchMessage is a no-op for unknown seqs', () => {
    const folder = new TimelineFolder()
    folder.push(asstMsg('hi'))
    expect(folder.messageAt(9999)).toBeNull()
    folder.patchMessage(9999, { reasoning_content: 'x' })
  })
})

describe('buildTimeline — light-history messages (reasoning omitted)', () => {
  it('renders a reasoning_omitted tool-call message as its own row', () => {
    // Without the flag this assistant message would be an unrenderable bare
    // tool-call wrapper; the omitted reasoning makes it worth a row (it can
    // be expanded lazily).
    const toolCalls: ToolCallInfo[] = [{ id: 'c1', name: 'bash', arguments: '{}' }]
    const items = buildTimeline([
      userMsg('do it'),
      ev({
        kind: 'message',
        role: 'assistant',
        content: '',
        reasoning_omitted: true,
        tool_calls: toolCalls,
      }),
      toolStart('c1'),
      toolRes('c1'),
    ])
    const assistants = assistantMessages(items)
    expect(assistants).toHaveLength(1)
    expect(assistants[0].reasoning_omitted).toBe(true)
  })

  it('keeps the flag on a plain light message block', () => {
    const items = buildTimeline([
      ev({ kind: 'message', role: 'assistant', content: 'answer', reasoning_omitted: true }),
    ])
    expect(assistantMessages(items)[0].reasoning_omitted).toBe(true)
  })
})

describe('buildTimeline — repeated tool-call ids across runs', () => {
  // Providers may number their calls per response (`call_0`, `call_1`, …), so
  // a later run can repeat an id — and a run that died mid-stream leaves its
  // id for the resumed run to repeat. The folded timeline therefore pairs
  // deltas/results with the *nearest preceding* `tool_call_start`, and the
  // light history keeps both runs' previews (see the gateway's
  // `superseded_tool_output_deltas`).
  const toolBlocks = (items: ReturnType<typeof buildTimeline>): ToolBlock[] =>
    items.flatMap((i) => (i.type === 'tool' ? [i.block] : []))

  it("keeps a dead run's preview in its own block", () => {
    const items = buildTimeline([
      toolStart('call_1'),
      toolOut('call_1', 'dead output'),
      // The run died here (no `tool_result`); the resumed run repeats the id.
      toolStart('call_1'),
      toolOut('call_1', 'live output'),
      toolRes('call_1', true, 'live output'),
    ])
    const blocks = toolBlocks(items)
    expect(blocks.length).toBe(2)
    expect(blocks[0].output).toBe('dead output')
    // The new start closed the stale block: it no longer looks running.
    expect(blocks[0].streaming).toBe(false)
    expect(blocks[0].resultSeq).toBeUndefined()
    expect(blocks[1].output).toBe('live output')
    expect(blocks[1].streaming).toBe(false)
    expect(blocks[1].resultSeq).toEqual(expect.any(Number))
  })

  it("does not append a new run's deltas to an earlier run's finished block", () => {
    const items = buildTimeline([
      toolStart('call_1'),
      toolOut('call_1', 'run1'),
      toolRes('call_1', true, 'run1'),
      toolStart('call_1'),
      toolOut('call_1', 'run2 streaming'),
    ])
    const blocks = toolBlocks(items)
    expect(blocks.length).toBe(2)
    expect(blocks[0].output).toBe('run1')
    expect(blocks[1].output).toBe('run2 streaming')
    expect(blocks[1].streaming).toBe(true)
  })
})

describe('buildTimeline — lazy tool output (light history)', () => {
  it('records the result seq and the omitted-output markers', () => {
    const result = ev({
      kind: 'tool_result',
      id: 'c1',
      name: 'bash',
      ok: true,
      output: 'preview',
      output_omitted: true,
      output_chars: 12345,
    })
    const items = buildTimeline([toolStart('c1'), result])
    const item = items.find((i) => i.type === 'tool')
    if (item?.type !== 'tool') throw new Error('expected a tool block')
    expect(item.block.output).toBe('preview')
    expect(item.block.outputOmitted).toBe(true)
    expect(item.block.outputChars).toBe(12345)
    expect(item.block.resultSeq).toBe(result.seq)
  })

  it('leaves a complete output unflagged but still records the result seq', () => {
    const result = ev({
      kind: 'tool_result',
      id: 'c1',
      name: 'bash',
      ok: true,
      output: 'done',
    })
    const items = buildTimeline([toolStart('c1'), result])
    const item = items[0]
    if (item.type !== 'tool') throw new Error('expected a tool block')
    expect(item.block.outputOmitted).toBe(false)
    expect(item.block.outputChars).toBeUndefined()
    expect(item.block.resultSeq).toBe(result.seq)
  })
})

describe('TimelineFolder — patchToolResult', () => {
  it('hydrates the block finalized by that result seq', () => {
    const folder = new TimelineFolder()
    folder.push(toolStart('c1'))
    const result = ev({
      kind: 'tool_result',
      id: 'c1',
      name: 'bash',
      ok: true,
      output: 'preview',
      output_omitted: true,
      output_chars: 9999,
    })
    folder.push(result)
    folder.patchToolResult(result.seq as number, {
      output: 'the full text',
      outputOmitted: false,
    })
    const item = folder.items()[0]
    if (item.type !== 'tool') throw new Error('expected a tool block')
    expect(item.block.output).toBe('the full text')
    expect(item.block.outputOmitted).toBe(false)
  })

  it('is a no-op for an unknown seq', () => {
    const folder = new TimelineFolder()
    folder.push(toolStart('c1'))
    folder.patchToolResult(9999, { output: 'nope' }) // must not throw
    const item = folder.items()[0]
    if (item.type !== 'tool') throw new Error('expected a tool block')
    expect(item.block.output).toBeUndefined()
  })
})
