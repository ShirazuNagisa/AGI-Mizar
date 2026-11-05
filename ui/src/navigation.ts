import type { EngineId } from './api'

/** Which page the content area is showing. */
export type Page =
  | { kind: 'home' }
  | { kind: 'agent'; tab: AgentTabId }
  | { kind: 'login' }
  | { kind: 'settings' }

/** GUI and CLI are different surfaces of the same engine. */
export type AgentSurface = 'gui' | 'cli'

/**
 * One row in the sidebar.
 *
 * The backend still has three engines. The window shows five destinations
 * because Claude and Codex each have a GUI transcript surface and a CLI one,
 * and a single page that mixes both makes "can I send here?" depend on which
 * row happens to be on top.
 */
export interface AgentTab {
  id: string
  engine: EngineId
  surface: AgentSurface | null
  name: string
}

export const AGENT_TABS: AgentTab[] = [
  { id: 'dsh', engine: 'dsh', surface: null, name: 'DSH' },
  { id: 'claude-gui', engine: 'claude', surface: 'gui', name: 'CC GUI' },
  { id: 'claude-cli', engine: 'claude', surface: 'cli', name: 'CC CLI' },
  { id: 'codex-gui', engine: 'codex', surface: 'gui', name: 'Codex GUI' },
  { id: 'codex-cli', engine: 'codex', surface: 'cli', name: 'Codex CLI' },
]

export type AgentTabId = (typeof AGENT_TABS)[number]['id']

/** The tab a page id names. The id is the source of truth on the wire. */
export function agentTab(id: string): AgentTab {
  return AGENT_TABS.find((tab) => tab.id === id) ?? AGENT_TABS[0]!
}

/**
 * Whether a session belongs to this surface.
 *
 * `source` is the desktop adapter's answer: `codex_desktop` and
 * `claude_desktop` are GUI-owned transcripts. Everything else is the CLI
 * surface, including old rows that predate the field.
 */
export function sessionOnSurface(
  session: { key: string; source: string | null },
  tab: AgentTab,
): boolean {
  if (!session.key.startsWith(`${tab.engine}:`)) return false
  if (tab.surface === null) return true
  const isGui = session.source === `${tab.engine}_desktop`
  return tab.surface === 'gui' ? isGui : !isGui
}

/**
 * The three engines, for screens that configure engines rather than surfaces
 * (first-run setup, the menu bar panel).
 */
export const AGENTS: { engine: EngineId; name: string }[] = [
  { engine: 'dsh', name: 'DSH' },
  { engine: 'claude', name: 'Claude Code' },
  { engine: 'codex', name: 'Codex' },
]

/** Whether two pages point at the same place. */
export function samePage(a: Page, b: Page): boolean {
  if (a.kind !== b.kind) return false
  if (a.kind === 'agent' && b.kind === 'agent') return a.tab === b.tab
  return true
}
