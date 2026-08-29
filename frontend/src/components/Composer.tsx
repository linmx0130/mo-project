import { useEffect, useMemo, useRef, useState } from 'react'
import type { ChangeEvent, FormEvent, KeyboardEvent } from 'react'

interface Props {
  /** A session run is in progress: the input is frozen and Stop is shown. */
  running: boolean
  /** A send request is in flight: Send is disabled. */
  busy?: boolean
  placeholder?: string
  onStop?: () => void
  /** Controlled text: when set, the textarea shows this value and every edit
   *  is reported via `onTextChange` (used by the draft view so the typed
   *  message survives navigation). Omit for fully internal state. */
  value?: string
  onTextChange?: (text: string) => void
  /** Resolve `false` to keep the typed text and the picked images (e.g.
   *  validation failed or the send/upload failed). `files` are the locally
   *  picked image files, shown as thumbnails below the textarea; the view
   *  uploads them into the session folder when the message is sent. */
  onSubmit: (
    text: string,
    files: File[],
  ) => Promise<boolean | void> | boolean | void
}

/** Bottom-of-view chat input: textarea + Upload image + Send, or Stop while
 *  running. Picked images are previewed as thumbnails in a strip between
 *  the textarea and the actions row (the bottom part of the typing box);
 *  they are only uploaded into the session folder on send, so this
 *  component is identical in the session view and the new-session form. */
export default function Composer({
  running,
  busy = false,
  placeholder,
  onStop,
  value,
  onTextChange,
  onSubmit,
}: Props) {
  const [text, setText] = useState('')
  const [files, setFiles] = useState<File[]>([])
  const [uploadError, setUploadError] = useState<string | null>(null)
  const fileInputRef = useRef<HTMLInputElement | null>(null)
  const currentText = value !== undefined ? value : text

  // Thumbnail previews via object URLs (the files are not uploaded yet).
  // Revoked when the list changes or the composer unmounts.
  const previews = useMemo(() => files.map((f) => URL.createObjectURL(f)), [files])
  useEffect(() => {
    const urls = previews
    return () => urls.forEach((url) => URL.revokeObjectURL(url))
  }, [previews])

  const handleChange = (next: string) => {
    if (value !== undefined) onTextChange?.(next)
    else setText(next)
  }

  /** Add the picked image files to the strip. Non-image picks are skipped
   *  with a note; the input is reset so re-picking the same file works. */
  const pickFiles = (e: ChangeEvent<HTMLInputElement>) => {
    const picked = Array.from(e.target.files ?? [])
    e.target.value = ''
    if (picked.length === 0) return
    const images = picked.filter((f) => f.type.startsWith('image/'))
    if (images.length < picked.length) {
      setUploadError('Only image files can be attached; non-image files were skipped.')
    } else {
      setUploadError(null)
    }
    if (images.length > 0) setFiles((prev) => [...prev, ...images])
  }

  const removeFile = (index: number) => {
    setFiles((prev) => prev.filter((_, i) => i !== index))
  }

  const send = async () => {
    const trimmed = currentText.trim()
    if ((!trimmed && files.length === 0) || running || busy) return
    const keep = await onSubmit(trimmed, files)
    if (keep === false) return
    // No-op in controlled mode: the parent owns the value (the draft view
    // clears it by dropping the whole draft on session creation).
    setText('')
    setFiles([])
    setUploadError(null)
  }

  const submit = (e: FormEvent) => {
    e.preventDefault()
    void send()
  }

  const onKeyDown = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    // Enter sends (Shift+Enter inserts a newline); ignore IME composition.
    if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) {
      e.preventDefault()
      void send()
    }
  }

  return (
    <form className="composer" onSubmit={submit}>
      <textarea
        value={currentText}
        onChange={(e) => handleChange(e.target.value)}
        onKeyDown={onKeyDown}
        placeholder={placeholder ?? 'Type a message…'}
        rows={3}
        disabled={running}
        spellCheck={false}
      />
      {(files.length > 0 || uploadError) && (
        <div className="composer-images">
          {files.map((file, i) => (
            <span className="composer-thumb" key={`${file.name}-${i}`}>
              <img src={previews[i]} alt={file.name} title={file.name} />
              <button
                type="button"
                className="thumb-remove"
                onClick={() => removeFile(i)}
                aria-label={`Remove ${file.name}`}
                title="Remove"
              >
                ×
              </button>
            </span>
          ))}
          {uploadError && (
            <span className="composer-upload-error">{uploadError}</span>
          )}
        </div>
      )}
      <div className="composer-actions">
        {running ? (
          <button
            type="button"
            className="stop"
            onClick={() => onStop?.()}
            disabled={busy}
          >
            Stop
          </button>
        ) : (
          <>
            <input
              ref={fileInputRef}
              type="file"
              accept="image/*"
              multiple
              className="composer-file-input"
              tabIndex={-1}
              aria-hidden="true"
              onChange={pickFiles}
            />
            <button
              type="button"
              className="upload"
              onClick={() => fileInputRef.current?.click()}
              disabled={busy}
              title="Attach images to this message"
            >
              Upload image
            </button>
            <button
              type="submit"
              className="send"
              disabled={busy || (!currentText.trim() && files.length === 0)}
            >
              {busy ? 'Sending…' : 'Send'}
            </button>
          </>
        )}
      </div>
    </form>
  )
}
