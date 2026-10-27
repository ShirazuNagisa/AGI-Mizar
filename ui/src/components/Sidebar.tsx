import type { EngineStatus } from '../api'
import { AGENTS, type Page, samePage } from '../navigation'
import { ClaudeMark, CodexMark, DshMark, GearIcon, PersonIcon } from '../icons'
import { cn } from '../lib/utils'
import mizar from '../assets/mizar.png'

interface SidebarProps {
  engines: EngineStatus[]
  page: Page
  serverConnected: boolean
  loggedIn: boolean
  onNavigate: (page: Page) => void
}

/** One agent's mark, named in the engine's own spelling. */
const MARKS = { dsh: DshMark, claude: ClaudeMark, codex: CodexMark } as const

/**
 * The window's left column: the mark that goes home, the three agents, and the
 * two places that are not agents. No rules between sections — spacing is the
 * separation, the way the reference window does it.
 */
export function Sidebar({ engines, page, serverConnected, loggedIn, onNavigate }: SidebarProps) {
  return (
    <aside className="flex h-full w-[240px] shrink-0 flex-col border-r border-border bg-surface">
      {/* The window's title bar is gone; the first row is the traffic lights'
          room, and the system draws them over this strip. */}
      <div data-tauri-drag-region className="h-8 shrink-0" />

      <button
        type="button"
        onClick={() => onNavigate({ kind: 'home' })}
        className="mx-2 flex h-9 items-center gap-2 rounded-lg px-2 text-left hover:bg-surface-2"
        aria-label="回到主页"
      >
        <img src={mizar} alt="" className="size-5" />
        <span className="text-[14px] font-semibold tracking-tight text-fg">AGI Mizar</span>
      </button>

      <nav className="mt-3 flex flex-col gap-0.5 px-2">
        {AGENTS.map(({ engine, name }) => {
          const status = engines.find((candidate) => candidate.engine === engine)
          const connected = status?.connected ?? false
          const active = samePage(page, { kind: 'agent', engine })
          const Mark = MARKS[engine]
          return (
            <button
              key={engine}
              type="button"
              onClick={() => onNavigate({ kind: 'agent', engine })}
              className={cn(
                'flex h-9 items-center gap-2.5 rounded-lg px-2 text-left transition-colors',
                active ? 'bg-surface-2' : 'hover:bg-surface-2/70',
              )}
            >
              <Mark size={18} className={active ? 'text-accent' : 'text-muted'} />
              <span
                className={cn(
                  'flex-1 truncate text-[13px]',
                  active ? 'font-medium text-fg' : 'text-fg/90',
                )}
              >
                {name}
              </span>
              <span
                className={cn(
                  'size-2 rounded-full transition-colors',
                  connected
                    ? 'bg-accent shadow-[0_0_0_3px_var(--accent-soft)]'
                    : 'bg-muted/40',
                )}
                title={connected ? '可用' : '不可用'}
                aria-label={connected ? '可用' : '不可用'}
              />
            </button>
          )
        })}
      </nav>

      <div className="flex-1" />

      <div className="flex flex-col gap-0.5 px-2 pb-2">
        <button
          type="button"
          onClick={() => onNavigate({ kind: 'settings' })}
          className="flex h-8 items-center gap-2 rounded-lg px-2 text-left text-[12px] text-muted hover:bg-surface-2"
        >
          <span
            className={cn(
              'size-2 rounded-full',
              serverConnected ? 'bg-accent' : 'bg-muted/40',
            )}
          />
          {serverConnected ? '已连接' : '未连接'}
        </button>

        <button
          type="button"
          onClick={() => onNavigate({ kind: 'login' })}
          className={cn(
            'flex h-9 items-center gap-2.5 rounded-lg px-2 text-left text-[13px] transition-colors',
            page.kind === 'login' ? 'bg-surface-2 text-fg' : 'text-fg/90 hover:bg-surface-2/70',
          )}
        >
          <PersonIcon size={17} className={page.kind === 'login' ? 'text-accent' : 'text-muted'} />
          <span className="flex-1">{loggedIn ? '账户' : '登录'}</span>
        </button>

        <button
          type="button"
          onClick={() => onNavigate({ kind: 'settings' })}
          className={cn(
            'flex h-9 items-center gap-2.5 rounded-lg px-2 text-left text-[13px] transition-colors',
            page.kind === 'settings'
              ? 'bg-surface-2 text-fg'
              : 'text-fg/90 hover:bg-surface-2/70',
          )}
        >
          <GearIcon size={17} className={page.kind === 'settings' ? 'text-accent' : 'text-muted'} />
          <span className="flex-1">设置</span>
        </button>
      </div>
    </aside>
  )
}
