import { useEffect, useState } from 'react'
import type { FilePreview } from '../api'

/**
 * One delivered file, as text.
 *
 * The fence lives in the kernel — a path outside the session's directory is
 * refused there, not here — so this only has to draw what it is given, and say
 * plainly when a file is binary or too long to show whole.
 */
export function FileViewer(props: {
  name: string
  path: string
  load: (path: string) => Promise<FilePreview>
  onClose: () => void
}) {
  const [preview, setPreview] = useState<FilePreview | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    let cancelled = false
    props
      .load(props.path)
      .then((value) => {
        if (!cancelled) setPreview(value)
      })
      .catch((cause: Error) => {
        if (!cancelled) setError(cause.message)
      })
    return () => {
      cancelled = true
    }
  }, [props])

  return (
    <div className="scrim" onClick={props.onClose}>
      <div className="file-card" onClick={(event) => event.stopPropagation()}>
        <div className="file-head">
          <span className="name">{props.name}</span>
          <span className="hint">{props.path}</span>
          <span style={{ flex: 1 }} />
          <button className="btn ghost" onClick={props.onClose}>
            Close
          </button>
        </div>

        {error !== null ? <div className="notice">{error}</div> : null}

        {preview !== null ? (
          preview.text !== null ? (
            <>
              <pre className="file-text">{preview.text}</pre>
              <div className="hint">
                {formatSize(preview.size)}
                {preview.truncated ? ' · showing the first 256 KB' : ''}
              </div>
            </>
          ) : (
            <div className="empty" style={{ padding: 32 }}>
              <strong>Not a text file</strong>
              <span>{formatSize(preview.size)} — nothing to show here.</span>
            </div>
          )
        ) : error === null ? (
          <div className="empty" style={{ padding: 32 }}>
            <span>Reading…</span>
          </div>
        ) : null}
      </div>
    </div>
  )
}

function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`
}
