import { useEffect, useRef, useState } from 'react'

import type { ModelDownload } from '../api'

export interface DownloadSpeed {
  /** Bytes per second over the last window; 0 until a second sample lands. */
  speed: number
  /** Seconds left at that speed, or null when the size is unknown. */
  eta: number | null
}

/**
 * How fast a model download is moving, from the progress frames it already has.
 *
 * The kernel publishes `done` four times a second and nothing else: speed and
 * the time left are differences between two frames, so they belong here rather
 * than on the wire. A field that jumps backwards (a retry from a fresh source)
 * restarts the window instead of producing a negative speed.
 */
export function useDownloadSpeed(download: ModelDownload | null): DownloadSpeed {
  const [speed, setSpeed] = useState(0)
  const last = useRef<{ at: number; done: number } | null>(null)

  const done = download?.done ?? 0
  const total = download?.total ?? 0
  const active = download !== null && download.state !== 'failed'

  useEffect(() => {
    if (!active) {
      last.current = null
      setSpeed(0)
      return
    }
    const now = Date.now()
    const previous = last.current
    if (previous === null || done < previous.done) {
      last.current = { at: now, done }
      return
    }
    const seconds = (now - previous.at) / 1000
    // One sample per window: `done` only moves every 250 ms, and dividing by a
    // 40 ms render gap turns one chunk into a speed spike.
    if (seconds < 0.75) return
    const moved = done - previous.done
    last.current = { at: now, done }
    if (moved <= 0) return
    const sample = moved / seconds
    // Half old, half new: a single slow chunk should read as slower, not as a
    // download that stopped.
    setSpeed((current) => (current === 0 ? sample : current * 0.5 + sample * 0.5))
  }, [active, done])

  const eta = speed > 0 && total > 0 && total > done ? (total - done) / speed : null
  return { speed, eta }
}
