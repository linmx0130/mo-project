// Render smoke tests for the timeline rows' message actions: every message
// kind gets a copy button, and a user message additionally offers the
// "New session from here" action (only where forking makes sense, i.e. when
// the row is given an `onFork` and the message carries its journal seq).
//
// `renderToStaticMarkup` keeps this dependency-free (no jsdom / testing
// library): the assertions are about the markup the affordances live in.
import { describe, expect, it } from 'vitest'
import { renderToStaticMarkup } from 'react-dom/server'
import type { MessageBlock, ToolBlock } from '../timeline'
import { MessageRow, ToolBlockRow } from './Timeline'

const userMessage: MessageBlock = {
  role: 'user',
  content: 'hello there',
  seq: 4,
}
const assistantMessage: MessageBlock = {
  role: 'assistant',
  content: '**hi**',
  seq: 5,
}
const toolMessage: MessageBlock = { role: 'tool', content: 'exit code: 0' }

describe('MessageRow — copy and fork affordances', () => {
  it('renders a copy button on a user message and the fork action when wired', () => {
    const html = renderToStaticMarkup(
      <MessageRow
        message={userMessage}
        sessionId="s1"
        onFork={() => {}}
        forkingSeq={null}
      />,
    )
    expect(html).toContain('copy-btn')
    expect(html).toContain('New session from here')
    // The forked message's own row is not the one in flight.
    expect(html).not.toContain('Creating…')
  })

  it('hides the fork action without a handler or a journal seq', () => {
    const noHandler = renderToStaticMarkup(<MessageRow message={userMessage} />)
    expect(noHandler).toContain('copy-btn')
    expect(noHandler).not.toContain('New session from here')

    const synthetic = renderToStaticMarkup(
      <MessageRow
        message={{ role: 'user', content: 'no seq' }}
        onFork={() => {}}
      />,
    )
    expect(synthetic).not.toContain('New session from here')
  })

  it('marks the message being forked and disables the other fork buttons', () => {
    const html = renderToStaticMarkup(
      <MessageRow
        message={userMessage}
        onFork={() => {}}
        forkingSeq={4}
      />,
    )
    expect(html).toContain('Creating…')
    expect(html).toContain('disabled')
  })

  it('renders copy buttons on assistant and tool messages', () => {
    const assistant = renderToStaticMarkup(
      <MessageRow message={assistantMessage} />,
    )
    expect(assistant).toContain('copy-btn')
    const tool = renderToStaticMarkup(<MessageRow message={toolMessage} />)
    expect(tool).toContain('copy-btn')
    expect(tool).toContain('exit code: 0')
    // An image-only user message has nothing to copy.
    const imageOnly = renderToStaticMarkup(
      <MessageRow
        message={{
          role: 'user',
          content: '',
          images: [{ path: 'images/a.png', mime: 'image/png' }],
          seq: 1,
        }}
      />,
    )
    expect(imageOnly).not.toContain('copy-btn')
  })
})

describe('ToolBlockRow — copy affordance', () => {
  const block: ToolBlock = {
    id: 'call_1',
    name: 'bash',
    arguments: '{"command":"ls"}',
    output: 'a.txt\nb.txt\n',
    ok: true,
  }

  it('copies the tool output when there is one', () => {
    const html = renderToStaticMarkup(
      <ToolBlockRow block={block} now={Date.now()} />,
    )
    expect(html).toContain('copy-btn')
    expect(html).toContain('a.txt')
  })

  it('hides the button while there is no output yet', () => {
    const html = renderToStaticMarkup(
      <ToolBlockRow
        block={{ id: 'call_2', name: 'bash', arguments: '{}', streaming: true }}
        now={Date.now()}
      />,
    )
    expect(html).not.toContain('copy-btn')
  })
})
