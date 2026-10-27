import type { EngineId } from './api'

/** Which page the content area is showing. */
export type Page =
  | { kind: 'home' }
  | { kind: 'agent'; engine: EngineId }
  | { kind: 'login' }
  | { kind: 'settings' }

/** The three agents, in the order the sidebar fixes them. */
export const AGENTS: { engine: EngineId; name: string }[] = [
  { engine: 'dsh', name: 'DSH' },
  { engine: 'claude', name: 'Claude Code' },
  { engine: 'codex', name: 'Codex' },
]

/** Whether two pages point at the same place. */
export function samePage(a: Page, b: Page): boolean {
  if (a.kind !== b.kind) return false
  if (a.kind === 'agent' && b.kind === 'agent') return a.engine === b.engine
  return true
}
