import { useEffect, useRef, useState } from 'react'

interface Props {
  /** Raw message content to copy. */
  content: string
  /** Disable the button, e.g. while the message is still streaming. */
  disabled?: boolean
}

/** How long the "Copied ✓" feedback stays visible. */
const COPIED_MS = 2000

/** "Copy content" button for a message. Uses the async Clipboard API when
 *  the browser supports it; otherwise (non-secure context, permission
 *  denied, …) falls back to a dialog that shows the raw content so the
 *  user can select and copy it manually. */
export default function CopyButton({ content, disabled = false }: Props) {
  const [copied, setCopied] = useState(false)
  const [dialogOpen, setDialogOpen] = useState(false)
  const copiedTimerRef = useRef<number | null>(null)
  const textareaRef = useRef<HTMLTextAreaElement | null>(null)

  // Clear a pending "Copied!" timer on unmount.
  useEffect(() => {
    return () => {
      if (copiedTimerRef.current !== null) {
        window.clearTimeout(copiedTimerRef.current)
      }
    }
  }, [])

  // While the fallback dialog is open: focus + select the text so the user
  // can copy straight away, and close on Escape.
  useEffect(() => {
    if (!dialogOpen) return
    textareaRef.current?.focus()
    textareaRef.current?.select()
    const onKeyDown = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setDialogOpen(false)
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [dialogOpen])

  const copy = async () => {
    // `navigator.clipboard` only exists in secure contexts (https or
    // localhost); even then the write can be rejected (permission denied,
    // …). Both cases fall back to the manual dialog.
    if (typeof navigator.clipboard?.writeText === 'function') {
      try {
        await navigator.clipboard.writeText(content)
        setCopied(true)
        if (copiedTimerRef.current !== null) {
          window.clearTimeout(copiedTimerRef.current)
        }
        copiedTimerRef.current = window.setTimeout(
          () => setCopied(false),
          COPIED_MS,
        )
        return
      } catch {
        // fall through to the manual dialog
      }
    }
    setDialogOpen(true)
  }

  const selectAll = () => {
    textareaRef.current?.focus()
    textareaRef.current?.select()
  }

  return (
    <>
      <button
        type="button"
        className={`copy-btn${copied ? ' copied' : ''}`}
        onClick={() => void copy()}
        disabled={disabled}
        title={copied ? 'Copied to clipboard' : 'Copy raw content'}
      >
        {copied ? 'Copied ✓' : 'Copy content'}
      </button>
      {dialogOpen && (
        <div className="dialog-overlay" onClick={() => setDialogOpen(false)}>
          <div
            className="dialog"
            role="dialog"
            aria-modal="true"
            aria-label="Raw content"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="dialog-head">
              <span className="dialog-title">Raw content</span>
              <button
                type="button"
                className="dialog-close"
                onClick={() => setDialogOpen(false)}
                aria-label="Close"
                title="Close"
              >
                ✕
              </button>
            </div>
            <textarea
              ref={textareaRef}
              className="dialog-textarea"
              readOnly
              value={content}
              spellCheck={false}
            />
            <p className="dialog-hint">
              Your browser doesn't support programmatic copying — select the
              text above and press ⌘C / Ctrl+C.
            </p>
            <div className="dialog-actions">
              <button type="button" className="dialog-btn" onClick={selectAll}>
                Select all
              </button>
              <button
                type="button"
                className="dialog-btn primary"
                onClick={() => setDialogOpen(false)}
              >
                Close
              </button>
            </div>
          </div>
        </div>
      )}
    </>
  )
}
