import type { EngineStatus } from '../api'
import { AGENT_TABS, type AgentTab, type Page, samePage } from '../navigation'
import { ENGINE_MARKS, GearIcon, PersonIcon } from '../icons'
import { cn } from '../lib/utils'
import { isMac } from '../platform'
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
    // One colour for both states: filled means connected, an outline means
    // not. In the dark window `--fg` is white; in the light one it is ink, so
    // the dot stays visible instead of becoming a white spot on white.
    const tone =
      state === 'on'
        ? 'bg-fg'
        : state === 'off'
          ? 'border border-fg bg-transparent'
          : 'border border-fg/40 bg-transparent'
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

/** A CLI surface keeps its engine mark and adds a `>_` stamp at the corner. */
function AgentMark({ tab }: { tab: AgentTab }) {
  const Mark = ENGINE_MARKS[tab.engine]
  if (tab.surface !== 'cli') return <Mark size={18} />
  return (
    <span className="relative inline-flex size-[18px] shrink-0 items-center justify-center" aria-hidden>
      <Mark size={18} />
      <span className="absolute -bottom-1 -right-1 font-mono text-[6px] font-bold leading-none tracking-[-1px]">
        &gt;_
      </span>
    </span>
  )
}

/** The label box every row shares, so the sweep and the rest state line up. */
const ROW_LABEL = 'flex w-full items-center gap-2.5 px-2'

/**
 * The window's left column: the mark that goes home, the five agent surfaces,
 * and the two places that are not agents. No rules between sections — spacing
 * is the separation.
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
      {/* The window's title bar is gone on macOS; the first row is the traffic
          lights' room, and the system draws them over this strip. Windows keeps
          the system title bar, so the strip would be empty space there — a small
          pad is all the Windows rows need. */}
      {isMac() ? (
        <div data-tauri-drag-region className="h-8 shrink-0" />
      ) : (
        <div className="h-2 shrink-0" />
      )}

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
        {AGENT_TABS.map((tab) => {
          const status = engines.find((candidate) => candidate.engine === tab.engine)
          // The lamp is not "the adapter can work": it is "this agent is
          // enabled here and is live now". Claude and Codex have no standing
          // socket, so their status is true all the time; `engaged` is what
          // goes out when nobody has a turn running.
          const engaged = status?.engaged === true
          // Only an explicit "not installed" changes the lamp; a snapshot that
          // does not mention the engine at all keeps the plain hollow look.
          const installed = status?.available ?? true
          const active = samePage(page, { kind: 'agent', tab: tab.id })
          const lamp = engaged ? '已连接' : installed ? '未连接' : '本机未安装'
          return (
            <WipeButton
              key={tab.id}
              onClick={() => onNavigate({ kind: 'agent', tab: tab.id })}
              className={cn('flex h-9 items-center text-left', active && 'bg-ink text-bg')}
              contentClassName={ROW_LABEL}
            >
              <AgentMark tab={tab} />
              <span className="min-w-0 truncate text-[12px]">{tab.name}</span>
              {tab.engine === 'dsh' ? null : (
                <span
                  className={cn(
                    'shrink-0 text-[9px] leading-none',
                    active ? 'text-bg/70' : 'text-muted',
                  )}
                >
                  （实验性）
                </span>
              )}
              <Lamp state={engaged ? 'on' : installed ? 'off' : 'absent'} label={lamp} />
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
