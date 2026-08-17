// Timeline folding: turn a journal event stream into renderable items.
// Kept out of the component files so the react-refresh lint rule (component
// files must only export components) stays satisfied — the row components
// in `components/Timeline.tsx` consume these.

import type { JournalEvent, JournalMessage, SessionStatus } from './api'

export function isTerminal(status: SessionStatus): boolean {
  return status === 'completed' || status === 'failed' || status === 'cancelled'
}

export interface ToolBlock {
  id: string
  name: string
  arguments: string
  output?: string
  ok?: boolean
  /** True while output deltas are still arriving (tool still running). */
  streaming?: boolean
  /** Epoch ms of the `tool_call_start` event, for the elapsed badge. */
  startedAt?: number
  /** The subagent session id when this `spawn_subagent` block actually
   *  spawned a child (from the matching `subagent_started` event); the
   *  block then shows a "view subagent" affordance opening a modal. */
  childId?: string
}

/** An assistant message as rendered; `streaming` marks a message whose
 *  content is still being assembled from `message_delta` events, and
 *  `truncated` marks a streaming preview that was closed by a turn
 *  boundary before its final `message` event arrived (the run died
 *  mid-stream) — it keeps its partial text but is no longer streaming. */
export type MessageBlock = JournalMessage & {
  streaming?: boolean
  truncated?: boolean
}

export type TimelineItem =
  | { type: 'message'; message: MessageBlock }
  | { type: 'tool'; block: ToolBlock }
  | { type: 'event'; event: JournalEvent }

/** True when the message deserves its own timeline row. An assistant
 *  message with no text and no reasoning that only wraps tool calls is
 *  fully represented by the tool blocks that follow it, so the empty
 *  "assistant" bubble is skipped. */
function isRenderableMessage(msg: JournalMessage): boolean {
  if (msg.role !== 'assistant') return true
  const hasText = (msg.content ?? '').trim().length > 0
  const hasReasoning = (msg.reasoning_content ?? '').trim().length > 0
  return hasText || hasReasoning || (msg.tool_calls?.length ?? 0) === 0
}

/** Fold the journal event stream into renderable items.
 *
 * - `message_delta` events append to the assistant message currently being
 *   assembled (token-by-token preview of both the visible text and the
 *   reasoning content); the following `message` event replaces its content
 *   with the canonical assembled text — which also repairs the transient
 *   state when a retried LLM call left partial deltas behind.
 * - `tool_output_delta` events append to the open tool block with the same
 *   id (bash output streaming); the following `tool_result` event replaces
 *   the preview with the complete, capped output.
 * - `tool_call_start`/`tool_result` are paired into one block as before.
 * - `subagent_started` links the matching `spawn_subagent` tool block to
 *   the child session id (the block then offers "view subagent").
 * - An assistant `message` with no text and no reasoning that only wraps
 *   tool calls is skipped — the tool blocks that follow it carry the
 *   action (the journal keeps the message; the worker needs it to rebuild
 *   the chat context on followups).
 *
 * **Turn boundaries.** A `message_delta`/`tool_output_delta` is the only
 * event that may arrive while its preview is still streaming — except
 * `context_usage`, which the worker journals *between* the deltas and the
 * final `message` event of the same turn (so it never closes the open
 * message). Every other non-delta event marks a *turn boundary*. When a
 * run dies mid-stream (e.g. the OS slept and the LLM connection was
 * dropped, so the worker's retries failed and the session was marked
 * failed/cancelled), the journal ends with orphan `message_delta`s and no
 * final `message` event. Without the boundary close below, the delta-built
 * preview would stay "open" forever and swallow the next run's deltas —
 * the new response would be appended onto the interrupted message instead
 * of appearing as a fresh message after the followup. So any non-delta
 * event closes the open previews: they keep their partial text, stop
 * "streaming" (and are marked `truncated` when no canonical `message` ever
 * finalized them), and the next run's deltas start a new message.
 */
export function buildTimeline(events: JournalEvent[]): TimelineItem[] {
  const items: TimelineItem[] = []
  const pending = new Map<string, ToolBlock>()
  let openMessage: MessageBlock | null = null
  // Index in `items` of the block `openMessage` references, so it can be
  // removed when the final message turns out to be a bare tool-call wrapper
  // (no text, no reasoning). `items.pop()` would be unsafe here because
  // `context_usage` events land between the deltas and the final message.
  let openIdx = -1

  // Close the delta-assembled assistant message preview. The preview can
  // never be open while tool events / tool-journaled requests arrive (the
  // assistant `message` is always finalized before tools run), so closing
  // it at any non-delta event is safe: it keeps its partial text but stops
  // "streaming" and is marked `truncated` when no canonical `message` ever
  // finalized it (see the doc comment above).
  const closeOpenMessage = () => {
    if (openMessage) {
      openMessage.streaming = false
      openMessage.truncated = true
      openMessage = null
      openIdx = -1
    }
  }
  // Close every streaming tool block. Unlike the open message, this is only
  // safe at *true* turn boundaries: events that can never arrive while a
  // sibling tool is legitimately running. (`subagent_started`,
  // `mode_change_request` and `ask_user_request` are journaled by tools
  // mid-execution, so they must not close a streaming bash block.)
  const closeStreamingTools = () => {
    for (const block of pending.values()) block.streaming = false
  }
  // A true turn boundary closes both.
  const closeTurn = () => {
    closeOpenMessage()
    closeStreamingTools()
  }

  for (const ev of events) {
    const kind = ev.kind
    switch (kind.kind) {
      case 'message_delta': {
        // A delta is a stream continuation — never a boundary.
        const reasoning = kind.reasoning_content ?? ''
        if (openMessage) {
          if (kind.content) openMessage.content += kind.content
          if (reasoning) {
            openMessage.reasoning_content =
              (openMessage.reasoning_content ?? '') + reasoning
          }
        } else {
          const block: MessageBlock = {
            role: 'assistant',
            content: kind.content,
            reasoning_content: reasoning || null,
            streaming: true,
          }
          openMessage = block
          openIdx = items.length
          items.push({ type: 'message', message: block })
        }
        break
      }
      case 'message': {
        if (kind.role === 'assistant' && openMessage) {
          // Finalize the delta-built preview with the canonical message.
          openMessage.content = kind.content
          openMessage.reasoning_content = kind.reasoning_content ?? null
          openMessage.tool_call_id = kind.tool_call_id ?? null
          openMessage.tool_calls = kind.tool_calls ?? null
          openMessage.streaming = false
          openMessage.truncated = false
          if (!isRenderableMessage(openMessage)) {
            // A bare tool-call turn (no text, no reasoning): the streamed
            // preview is dropped and the tool blocks carry the action.
            items.splice(openIdx, 1)
          }
          openMessage = null
          // The finalized message is a boundary: stale streaming tool
          // blocks from an interrupted earlier run are closed too.
          closeStreamingTools()
        } else if (isRenderableMessage(kind)) {
          // A message that does not finalize the in-flight preview — e.g.
          // the user's followup after an interrupted run — is a turn
          // boundary. Closing the stale open preview here is what keeps the
          // next run's `message_delta`s from being appended onto it.
          closeTurn()
          items.push({
            type: 'message',
            message: {
              role: kind.role,
              content: kind.content,
              reasoning_content: kind.reasoning_content ?? null,
              tool_call_id: kind.tool_call_id ?? null,
              tool_calls: kind.tool_calls ?? null,
            },
          })
        }
        break
      }
      case 'tool_call_start': {
        // A new tool block starts a fresh activity: any preview left open
        // from a dead run is stale. (Sibling `tool_call_start`s of the same
        // message are journaled back-to-back before any output streams, so
        // closing streaming tools here is a no-op in the normal flow.)
        closeTurn()
        const block: ToolBlock = {
          id: kind.id,
          name: kind.name,
          arguments: kind.arguments,
          startedAt: new Date(ev.ts).getTime(),
        }
        pending.set(kind.id, block)
        items.push({ type: 'tool', block })
        break
      }
      case 'tool_output_delta': {
        // A delta is a stream continuation — never a boundary.
        const block = pending.get(kind.id)
        if (block) {
          block.output = (block.output ?? '') + kind.output
          block.streaming = true
        }
        break
      }
      case 'tool_result': {
        // Finalizes only its own block — sibling blocks may legitimately
        // still be streaming (parallel tool calls), so no boundary close.
        const block = pending.get(kind.id)
        if (block) {
          block.ok = kind.ok
          block.streaming = false
          // The canonical result is capped at ~1 MB while the delta stream
          // is not; keep whichever is longer so the tail is never lost when
          // the result lands (small commands still get the canonical text,
          // including the exit-code line).
          if (!block.output || kind.output.length >= block.output.length) {
            block.output = kind.output
          } else {
            block.output = `${block.output}\n\n[tool result was truncated by the harness — showing the full streamed output]`
          }
          pending.delete(kind.id)
        }
        break
      }
      case 'subagent_started': {
        // Link the spawn_subagent tool block to the child session so the
        // user can open a read-only modal of the subagent's messages. This
        // event is journaled by the `spawn_subagent` tool *mid-execution*:
        // sibling tools may still be legitimately streaming, so only the
        // open message preview is closed (a no-op in the normal flow).
        closeOpenMessage()
        const block = pending.get(kind.tool_call_id)
        if (block) block.childId = kind.child_id
        break
      }
      case 'mode_change_request':
      case 'ask_user_request':
      case 'permission_request':
        // Journaled by their tools mid-execution; sibling tools may still
        // be streaming, so only the open message preview is closed.
        closeOpenMessage()
        items.push({ type: 'event', event: ev })
        break
      case 'system_prompt':
        // Session metadata (the system prompt journaled on the first run);
        // never rendered as a chat message. Never coexists with a streaming
        // preview, but closing keeps the invariant uniform.
        closeTurn()
        break
      case 'context_usage':
        // The worker journals this *between* the deltas and the final
        // `message` event of the same turn, so it must NOT close the open
        // message — that would leave the preview truncated and let the
        // final `message` land as a second row. It does end any stale
        // streaming tool block (all of the previous turn's tools have
        // finished by the time the next generate reports usage).
        closeStreamingTools()
        items.push({ type: 'event', event: ev })
        break
      default:
        // status_change, mode_change, ask_user_answered,
        // mode_change_request_declined, handoff — all true turn boundaries.
        closeTurn()
        items.push({ type: 'event', event: ev })
    }
  }
  return items
}
