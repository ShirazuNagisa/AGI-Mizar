import type { EngineStatus } from '../api'
import { AGENTS, type Page, samePage } from '../navigation'
import { ENGINE_MARKS, GearIcon, PersonIcon } from '../icons'
import { cn } from '../lib/utils'
import { WipeButton } from './ui/wipe'
import { useIsOled } from '../lib/oled'

interface SidebarProps {
  engines: EngineStatus[]
  page: Page
  serverConnected: boolean
  loggedIn: boolean
  onNavigate: (page: Page) => void
}

/**
 * A three-state lamp in one bit: filled, hollow, or dithered. There is no
 * colour to spend on "not installed", so the third state is a pattern.
 */
function Lamp({ state, label }: { state: 'on' | 'off' | 'absent'; label?: string }) {
  const oled = useIsOled()
  if (!oled) {
    // The classic window has colour and opacity, so presence is a dot.
    const tone =
      state === 'on'
        ? 'bg-emerald-500'
        : state === 'off'
          ? 'bg-transparent ring-1 ring-inset ring-muted/60'
          : 'bg-muted/35'
    return <span role="img" aria-label={label} title={label} className={cn('size-2 shrink-0 rounded-full', tone)} />
  }
  if (state === 'on') {
    return <span role="img" aria-label={label} title={label} className="size-2 shrink-0 bg-current" />
  }
  if (state === 'off') {
    return (
      <span role="img" aria-label={label} title={label} className="size-2 shrink-0 border border-current" />
    )
  }
  return (
    <span
      role="img"
      aria-label={label}
      title={label}
      className="size-2 shrink-0 border border-current"
      style={{
        backgroundImage: 'repeating-conic-gradient(currentColor 0 25%, transparent 0 50%)',
        backgroundSize: '2px 2px',
      }}
    />
  )
}

/** The mark, drawn in the current colour so it inverts with its row. */
function Star() {
  return (
    <svg width="16" height="16" viewBox="0 0 16 16" fill="currentColor" aria-hidden>
      <path d="M8 0 L9.6 5.23 L14.93 4 L11.2 8 L14.93 12 L9.6 10.77 L8 16 L6.4 10.77 L1.07 12 L4.8 8 L1.07 4 L6.4 5.23 Z" />
    </svg>
  )
}

/** The label box every row shares, so the sweep and the rest state line up. */
const ROW_LABEL = 'flex w-full items-center gap-2.5 px-2'

/**
 * The window's left column: the mark that goes home, the three agents, and the
 * two places that are not agents. No rules between sections — spacing is the
 * separation.
 *
 * Only the current page is filled. Inverting every row would turn eight
 * navigation items into eight boxes.
 */
export function Sidebar({ engines, page, serverConnected, loggedIn, onNavigate }: SidebarProps) {
  const oled = useIsOled()
  return (
    <aside
      className={cn(
        'flex h-full w-[240px] shrink-0 flex-col border-r bg-bg',
        oled ? 'border-ink' : 'border-border',
      )}
    >
      {/* The window's title bar is gone; the first row is the traffic lights'
          room, and the system draws them over this strip. */}
      <div data-tauri-drag-region className="h-8 shrink-0" />

      <WipeButton
        onClick={() => onNavigate({ kind: 'home' })}
        className="mx-2 flex h-9 items-center text-left"
        contentClassName={cn(ROW_LABEL, 'gap-2')}
        aria-label="回到主页"
      >
        <Star />
        <span className="text-[12px] tracking-[1px]">AGI Mizar</span>
      </WipeButton>

      <nav className="mt-3 flex flex-col px-2">
        {AGENTS.map(({ engine, name }) => {
          const status = engines.find((candidate) => candidate.engine === engine)
          const connected = status?.connected ?? false
          // Only an explicit "not installed" changes the lamp; a snapshot that
          // does not mention the engine at all keeps the plain hollow look.
          const installed = status?.available ?? true
          const active = samePage(page, { kind: 'agent', engine })
          const Mark = ENGINE_MARKS[engine]
          const lamp = connected ? '已连接' : installed ? '未连接' : '本机未安装'
          return (
            <WipeButton
              key={engine}
              onClick={() => onNavigate({ kind: 'agent', engine })}
              className={cn('flex h-9 items-center text-left', active && 'bg-ink text-bg')}
              contentClassName={ROW_LABEL}
            >
              <Mark size={18} />
              <span className="flex-1 truncate text-[12px]">{name}</span>
              <Lamp state={connected ? 'on' : installed ? 'off' : 'absent'} label={lamp} />
            </WipeButton>
          )
        })}
      </nav>

      <div className="flex-1" />

      <div className="flex flex-col px-2 pb-2">
        <WipeButton
          onClick={() => onNavigate({ kind: 'settings' })}
          className="flex h-8 items-center text-left text-[12px]"
          contentClassName={cn(ROW_LABEL, 'gap-2')}
        >
          <Lamp state={serverConnected ? 'on' : 'off'} label={serverConnected ? '已连接' : '未连接'} />
          {serverConnected ? '已连接' : '未连接'}
        </WipeButton>

        <WipeButton
          onClick={() => onNavigate({ kind: 'login' })}
          className={cn(
            'flex h-9 items-center text-left text-[12px]',
            page.kind === 'login' && 'bg-ink text-bg',
          )}
          contentClassName={ROW_LABEL}
        >
          <PersonIcon size={17} />
          <span className="flex-1">{loggedIn ? '账户' : '登录'}</span>
        </WipeButton>

        <WipeButton
          onClick={() => onNavigate({ kind: 'settings' })}
          className={cn(
            'flex h-9 items-center text-left text-[12px]',
            page.kind === 'settings' && 'bg-ink text-bg',
          )}
          contentClassName={ROW_LABEL}
        >
          <GearIcon size={17} />
          <span className="flex-1">设置</span>
        </WipeButton>
      </div>
    </aside>
  )
}
