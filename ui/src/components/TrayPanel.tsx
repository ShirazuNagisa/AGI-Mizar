import { useEffect, useRef, useState } from 'react'
import type { DeviceStatus, EngineStatus, Kernel, StateSnapshot } from '../api'
import { ENGINE_MARKS, GearIcon, PowerIcon } from '../icons'
import { AGENT_TABS } from '../navigation'
import { applyPlatform } from '../platform'
import { cn } from '../lib/utils'

/**
 * The tray page.
 *
 * A separate window, not a native menu: the menu bar draws its own rows, and
 * these rows need an icon on the left, a lamp pinned to the right and a width
 * that hugs the text. A native menu can do none of the three.
 *
 * It is the same React bundle, served from the kernel with `?panel=tray`, so it
 * reads state the way the window does.
 */
export function TrayPanel({ kernel }: { kernel: Kernel }) {
  const [snapshot, setSnapshot] = useState<StateSnapshot | null>(null)

  useEffect(() => {
    let cancelled = false
    const read = () =>
      void kernel
        .state()
        .then((next) => {
          if (cancelled) return
          applyPlatform(next.platform)
          setSnapshot(next)
        })
        .catch(() => {})
    read()
    const timer = window.setInterval(read, 2000)
    return () => {
      cancelled = true
      window.clearInterval(timer)
    }
  }, [kernel])

  // The window is sized to what it actually drew. Measuring the DOM rather than
  // predicting row heights is the only version that cannot drift.
  const root = useRef<HTMLDivElement | null>(null)
  useEffect(() => {
    const element = root.current
    if (element === null) return
    const frame = requestAnimationFrame(() => {
      const height = Math.ceil(element.getBoundingClientRect().height)
      void import('../shell')
        .then(({ panelResize }) => panelResize(height))
        .catch(() => {})
    })
    return () => cancelAnimationFrame(frame)
  }, [snapshot])

  const engines: EngineStatus[] = snapshot?.engines ?? []
  const devices: DeviceStatus[] = snapshot?.devices ?? []

  return (
    <div ref={root} className="flex w-full flex-col gap-1.5 bg-surface p-1.5 text-fg">
      <div className="flex flex-col">
        {AGENT_TABS.map((tab) => {
          const status = engines.find((entry) => entry.engine === tab.engine)
          // Same rule as the sidebar: enabled here and live now, not merely
          // "the adapter could accept a prompt".
          const engaged = status?.engaged === true
          const Mark = ENGINE_MARKS[tab.engine]
          const label = tab.engine === 'dsh' ? tab.name : `${tab.name}（实验性）`
          return (
            <Row key={tab.id} label={label} connected={engaged}>
              <span className="relative inline-flex size-[15px] shrink-0 items-center justify-center text-muted">
                <Mark size={15} />
                {tab.surface === 'cli' ? (
                  <span className="absolute -bottom-1 -right-1 font-mono text-[5px] font-bold leading-none tracking-[-1px]">
                    &gt;_
                  </span>
                ) : null}
              </span>
            </Row>
          )
        })}
      </div>

      <div className="mz-divide-t" />

      <div className="flex flex-col">
        {devices.length === 0 ? (
          <Row label="没有同账户的设备" />
        ) : (
          devices.map((device) => (
            <Row key={device.name} label={device.name} connected={device.online}>
              <PhoneMark size={15} className="shrink-0 text-muted" />
            </Row>
          ))
        )}
      </div>

      <div className="mz-divide-t" />

      <button
        type="button"
        onClick={() => void import('../shell').then(({ panelOpenSettings }) => panelOpenSettings())}
        className="flex h-7 items-center justify-center gap-2 rounded-md text-[13px] outline-none hover:bg-surface-2"
      >
        <GearIcon size={14} />
        设置
      </button>
      <button
        type="button"
        onClick={() => void import('../shell').then(({ quitApp }) => quitApp())}
        className="flex h-7 items-center justify-center gap-2 rounded-md text-[13px] outline-none hover:bg-surface-2"
      >
        <PowerIcon size={14} />
        退出 AGI Mizar
      </button>
    </div>
  )
}

function Row({
  label,
  connected,
  children,
}: {
  label: string
  connected?: boolean
  children?: React.ReactNode
}) {
  return (
    <div className="flex h-7 items-center gap-2 px-1.5 text-[13px]">
      {children}
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {connected === undefined ? null : (
        <span
          aria-hidden
          className={cn(
            'size-2 shrink-0 rounded-full',
            connected ? 'bg-fg' : 'border border-fg bg-transparent',
          )}
        />
      )}
    </div>
  )
}

function PhoneMark({ size = 15, className }: { size?: number; className?: string }) {
  return (
    <svg width={size} height={size} viewBox="0 0 16 16" fill="currentColor" className={className} aria-hidden>
      <path d="M5 1h6a1 1 0 0 1 1 1v12a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V2a1 1 0 0 1 1-1Zm.5 2v9h5V3h-5Z" />
    </svg>
  )
}
