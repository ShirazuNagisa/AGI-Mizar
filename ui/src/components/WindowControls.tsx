import { useEffect, useState } from 'react'
import type { ReactNode } from 'react'

import { isWindows } from '../platform'
import { windowClose, windowIsMaximized, windowMinimize, windowToggleMaximize } from '../shell'

/**
 * The window buttons, drawn by the page on Windows.
 *
 * macOS gets a title-bar overlay and its own traffic lights from the system;
 * Windows gets the same edge-to-edge chrome, but the page has to paint it: the
 * system caption is gone (`decorations(false)`) and these three buttons call
 * the window plugin instead. Their order, size and hover behavior follow the
 * Windows caption (minimize, maximize/restore, close; close turns red), not the
 * macOS traffic lights — the chrome is the app's, the interactions are the
 * platform's.
 *
 * The container is fixed to the window's top-right corner. The page leaves room
 * for it by padding the toolbar's right edge (see `Toolbar`), so page actions
 * never slide underneath.
 *
 * Only Windows draws this. macOS overlays its traffic lights on the strip the
 * page reserves and Linux keeps the system title bar; rendering the caption
 * there too put Windows buttons beside a Mac's traffic lights. The gate is a
 * wrapper rather than an early return after the hooks, so a platform that
 * arrives late (the snapshot rewrites `<html data-platform>`) mounts or
 * unmounts the whole caption instead of changing the hook count mid-render.
 */
export function WindowControls() {
  if (!isWindows()) return null
  return <WindowsCaption />
}

/** The three buttons; only ever mounted on Windows. */
function WindowsCaption() {
  const [maximized, setMaximized] = useState(false)

  useEffect(() => {
    let alive = true
    const refresh = () => {
      void windowIsMaximized()
        .then((value) => {
          if (alive) setMaximized(value)
        })
        .catch(() => {
          // A page opened outside the shell has no window to ask.
        })
    }
    void refresh()
    window.addEventListener('resize', refresh)
    return () => {
      alive = false
      window.removeEventListener('resize', refresh)
    }
  }, [])

  return (
    <div className="pointer-events-none fixed right-0 top-0 z-[70] flex h-12 items-stretch">
      <WindowButton
        label="最小化"
        onClick={() => void windowMinimize().catch(() => {})}
        icon={<MinimizeGlyph />}
      />
      <WindowButton
        label={maximized ? '还原' : '最大化'}
        onClick={() => {
          setMaximized((value) => !value)
          void windowToggleMaximize().catch(() => {})
        }}
        icon={maximized ? <RestoreGlyph /> : <MaximizeGlyph />}
      />
      <WindowButton
        label="关闭"
        danger
        onClick={() => void windowClose().catch(() => {})}
        icon={<CloseGlyph />}
      />
    </div>
  )
}

/** Windows caption buttons are 46px wide and run the full height of the strip,
 * which here is the same 48px row the page's toolbar shares. */
const BUTTON = 'pointer-events-auto flex h-12 w-[46px] items-center justify-center transition-colors'

function WindowButton({
  label,
  icon,
  danger = false,
  onClick,
}: {
  label: string
  icon: ReactNode
  danger?: boolean
  onClick: () => void
}) {
  return (
    <button
      type="button"
      aria-label={label}
      title={label}
      onClick={onClick}
      className={
        danger
          ? `${BUTTON} text-fg/80 hover:bg-[#c42b1c] hover:text-white focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-fg/40`
          : `${BUTTON} text-fg/80 hover:bg-fg/10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-fg/40`
      }
    >
      {icon}
    </button>
  )
}

const GLYPH = 'size-[10px]'

function MinimizeGlyph() {
  return (
    <svg viewBox="0 0 10 10" className={GLYPH} aria-hidden="true">
      <path d="M1 5h8" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}

/** Windows draws the caption square with a thin border. */
function MaximizeGlyph() {
  return (
    <svg viewBox="0 0 10 10" className={GLYPH} aria-hidden="true">
      <rect x="1.5" y="1.5" width="7" height="7" fill="none" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}

/** Restore: two offset squares, the front one solid-outlined. */
function RestoreGlyph() {
  return (
    <svg viewBox="0 0 10 10" className={GLYPH} aria-hidden="true">
      <path d="M3.5 3.5V2a1 1 0 0 1 1-1H8a1 1 0 0 1 1 1v3.5a1 1 0 0 1-1 1H6.5" fill="none" stroke="currentColor" strokeWidth="1" />
      <rect x="1" y="3.5" width="5.5" height="5.5" fill="none" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}

function CloseGlyph() {
  return (
    <svg viewBox="0 0 10 10" className={GLYPH} aria-hidden="true">
      <path d="M1.5 1.5l7 7M8.5 1.5l-7 7" stroke="currentColor" strokeWidth="1" />
    </svg>
  )
}
