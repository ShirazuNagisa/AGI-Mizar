import { useEffect, useState } from 'react'

import { cn } from '../lib/utils'
import { isWindows } from '../platform'
import { windowIsMaximized, windowStartResize, type ResizeEdge } from '../shell'

/**
 * The resize borders a borderless Windows window loses.
 *
 * tao keeps the DWM shadow and its top-edge hit test, but a window with no
 * system frame no longer has the other seven resize targets. These thin strips
 * put them back: pointer-down on one starts the native resize loop through the
 * window plugin, so the drag behaves exactly like the system border did.
 */
const EDGES: readonly { edge: ResizeEdge; className: string }[] = [
  { edge: 'North', className: 'left-2 right-2 top-0 h-[3px] cursor-ns-resize' },
  { edge: 'South', className: 'left-2 right-2 bottom-0 h-[3px] cursor-ns-resize' },
  { edge: 'West', className: 'top-2 bottom-2 left-0 w-[3px] cursor-ew-resize' },
  { edge: 'East', className: 'top-2 bottom-2 right-0 w-[3px] cursor-ew-resize' },
  { edge: 'NorthWest', className: 'left-0 top-0 size-2.5 cursor-nwse-resize' },
  { edge: 'NorthEast', className: 'right-0 top-0 size-2.5 cursor-nesw-resize' },
  { edge: 'SouthWest', className: 'left-0 bottom-0 size-2.5 cursor-nesw-resize' },
  { edge: 'SouthEast', className: 'right-0 bottom-0 size-2.5 cursor-nwse-resize' },
]

export function WindowResizeHandles() {
  // Same gate as the caption: these edges exist because the Windows frame is
  // gone. A resizable Mac window already has eight native resize targets, and
  // a Mac user who found Windows 3px strips on its edges would be right to
  // call it a bug.
  if (!isWindows()) return null
  return <WindowsResizeEdges />
}

/** The eight strips, only ever mounted on Windows. */
function WindowsResizeEdges() {
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    let alive = true
    const refresh = () => {
      void windowIsMaximized()
        .then((value) => {
          if (alive) setMaximized(value)
        })
        .catch(() => {})
    }
    void refresh()
    window.addEventListener('resize', refresh)
    return () => {
      alive = false
      window.removeEventListener('resize', refresh)
    }
  }, [])

  // A maximized window has no size to drag; its edges are the screen's.
  if (maximized) return null

  return (
    <div className="pointer-events-none fixed inset-0 z-50">
      {EDGES.map(({ edge, className }) => (
        <div
          key={edge}
          className={cn('pointer-events-auto absolute', className)}
          onMouseDown={(event) => {
            // Plain mousedown, not pointerdown: the resize loop the window
            // plugin starts is a mouse loop, and mousedown is what the caption
            // would have received.
            event.preventDefault()
            void windowStartResize(edge).catch(() => {})
          }}
        />
      ))}
    </div>
  )
}
