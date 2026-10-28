/**
 * The kernel's local API, as the window uses it.
 *
 * Two transports, both loopback: HTTP for commands, a WebSocket for events. The
 * token comes from the page's own URL, which the shell put there — the window
 * never asks the user for it and never stores it.
 *
 * ### Why the types are written out rather than generated
 *
 * They mirror `agi-protocol` field for field. A generator (`specta`, `ts-rs`)
 * would be the right answer once the contract stops moving; until then a
 * hand-written mirror with a compile-time check on every field is less machinery
 * than a codegen step, and the mismatch it could hide is a field rename, which
 * the kernel would reject loudly.
 */

export type EngineId = 'dsh' | 'claude' | 'codex'

export interface Capabilities {
  attachments: boolean
  file_preview: boolean
  fork: boolean
  archive: boolean
  activity: boolean
  balance: boolean
  models: boolean
  permissions: boolean
}

export interface EngineStatus {
  engine: EngineId
  connected: boolean
  capabilities: Capabilities
  detail: string | null
  sessions: number
}

export interface ContextPressure {
  window: number
  used: number
  percent: number
}

export interface Session {
  key: string
  cwd: string | null
  title: string | null
  model: string | null
  updated_at: number
  running: boolean
  parent: string | null
  origin: string | null
  turns: number
  /** `null` means this engine cannot say, which is not the same as empty. */
  context: ContextPressure | null
}

export type RecordKind =
  | 'turn_start'
  | 'turn_end'
  | 'step'
  | 'user_message'
  | 'assistant_message'
  | 'tool_call'
  | 'tool_result'
  | 'notice'

export interface ToolInfo {
  call_id: string
  name: string
  arguments: string | null
  output: string | null
}

export interface TurnRecord {
  seq: number
  time: number
  turn: number | null
  step: number | null
  kind: RecordKind
  text: string | null
  tool: ToolInfo | null
}

export type InteractionKind = 'approval' | 'question'

export interface Question {
  id: string
  text: string
  header: string | null
  detail: string | null
  options: string[]
  multi_select: boolean
}

export interface Interaction {
  id: string
  session: string | null
  kind: InteractionKind
  title: string
  detail: string | null
  requested_at: number
  /** Empty for an approval. */
  questions: Question[]
}

/**
 * One day of usage, as the dashboard draws it.
 *
 * Every day that had any activity is present; the window does the range
 * arithmetic itself so switching 全部/30 天/7 天 never refetches.
 */
export interface UsageDay {
  /** `YYYY-MM-DD`, local time. */
  date: string
  messages: number
  inputTokens: number
  outputTokens: number
  /** Activity per local hour, 24 buckets. */
  hours: number[]
  models: ModelUsage[]
}

export interface ModelUsage {
  model: string
  messages: number
  inputTokens: number
  outputTokens: number
}

export interface UsageOverview {
  /** Top-level sessions across every engine. */
  sessions: number
  days: UsageDay[]
}

export interface AccountDevice {
  id: string
  name: string
  model: string
  /** `ios`, `macos`, … as the device reported it. */
  platform: string
  loginAt: number
  lastSeenAt: number
  /** Whether this is the device that asked. */
  current: boolean
}

export interface AccountDesktop {
  deviceId: string
  name: string
  online: boolean
}

export interface AccountUser {
  id: string
  username: string
  email: string | null
  plan: 'free' | 'paid'
  paidUntil: number | null
  devices: AccountDevice[]
}

export interface StateSnapshot {
  version: string
  engines: EngineStatus[]
  sessions: Session[]
  interactions: Interaction[]
}

export interface ActivityEntry {
  id: string
  seq: number
  at: number
  kind: 'thinking' | 'command' | 'output' | 'tool'
  title: string
  text: string
}

export interface SessionStats {
  turns: number
  steps: number
  llm_ms: number
  tool_ms: number
  input_tokens: number
  output_tokens: number
}

export interface DeliveredFile {
  path: string
  name: string
  description: string | null
}

export interface ModelInfo {
  id: string
  label: string
  provider: string | null
  reasoning: string[]
}

export interface PermissionPreset {
  id: string
  label: string
  description: string | null
}

export interface Wallet {
  currency: string
  amount: string
}

export type Balance =
  | { status: 'ready'; wallets: Wallet[]; bonus: Wallet[]; top_up_url: string | null }
  | { status: 'signed_out' }
  | { status: 'unavailable' }
  | { status: 'unsupported' }

export interface Workspace {
  id: string
  path: string
  name: string | null
  /** `null` when the host did not say; `[]` when it said nothing is filed. */
  session_ids: string[] | null
}

export interface FilePreview {
  path: string
  name: string
  size: number
  text: string | null
  truncated: boolean
}

export interface PairingView {
  code: string
  uri: string
  addresses: string[]
  svg: string | null
  expires_at: number
}

export type Command =
  | { op: 'state' }
  | { op: 'messages'; session: string; limit?: number; before?: number }
  | { op: 'prompt'; session: string; text: string }
  | { op: 'cancel'; session: string }
  | { op: 'answer'; id: string; outcome?: string; answers?: unknown[] }
  | { op: 'set_model'; session: string; model: string }
  | { op: 'set_permission'; session: string; preset: string }
  | { op: 'activity'; session: string; limit?: number }
  | { op: 'stats'; session: string }
  | { op: 'deliverables'; session: string }
  | { op: 'models'; engine: EngineId }
  | { op: 'permissions'; engine: EngineId }
  | { op: 'create'; engine: EngineId; cwd?: string }
  | { op: 'rename'; session: string; title: string }
  | { op: 'archive'; session: string; stop_activity?: boolean }
  | { op: 'workspaces'; engine: EngineId }
  | { op: 'balance'; engine: EngineId }
  | { op: 'read_file'; session: string; path: string }
  | { op: 'reconnect'; engine: EngineId }
  | { op: 'pairing'; refresh: boolean }
  | { op: 'usage_overview' }
  | { op: 'set_theme'; theme: 'system' | 'light' | 'dark' }
  | { op: 'relay_status' }
  | { op: 'account' }
  | { op: 'account_login'; identifier: string; password: string }
  | { op: 'account_logout' }
  | { op: 'account_desktops' }
  | { op: 'ping' }

export interface ApiError {
  code: string
  message: string
}

export interface CommandResponse<T = unknown> {
  ok: boolean
  value?: T
  error?: ApiError
}

export type ApiEvent =
  | { type: 'session_updated'; session: Session }
  | { type: 'session_removed'; session: string }
  | { type: 'record_appended'; session: string; record: TurnRecord }
  | { type: 'interaction_requested'; interaction: Interaction }
  | { type: 'interaction_resolved'; id: string }
  | { type: 'engine_status'; status: EngineStatus }
  | { type: 'notice'; level: string; message: string }

/** The token the shell put in this page's URL. */
function tokenFromLocation(): string {
  return new URLSearchParams(window.location.search).get('token') ?? ''
}

export class Kernel {
  readonly #token: string

  constructor(token = tokenFromLocation()) {
    this.#token = token
  }

  get hasToken(): boolean {
    return this.#token !== ''
  }

  async state(): Promise<StateSnapshot> {
    const response = await fetch('/api/v1/state', { headers: this.#headers() })
    if (!response.ok) throw new Error(`the kernel answered ${response.status}`)
    return (await response.json()) as StateSnapshot
  }

  async send<T = unknown>(command: Command): Promise<T> {
    const response = await fetch('/api/v1/command', {
      method: 'POST',
      headers: { ...this.#headers(), 'content-type': 'application/json' },
      body: JSON.stringify(command),
    })
    const body = (await response.json()) as CommandResponse<T>
    if (!body.ok) {
      throw new Error(body.error ? `${body.error.code}: ${body.error.message}` : 'the kernel refused')
    }
    return body.value as T
  }

  /** Subscribe to the event stream. Returns the close function. */
  subscribe(onEvent: (event: ApiEvent) => void, onDown: () => void): () => void {
    const scheme = window.location.protocol === 'https:' ? 'wss' : 'ws'
    const socket = new WebSocket(
      `${scheme}://${window.location.host}/api/v1/events?token=${encodeURIComponent(this.#token)}`,
    )
    socket.onmessage = (message) => {
      try {
        onEvent(JSON.parse(message.data as string) as ApiEvent)
      } catch {
        // A frame this build cannot read is not a reason to drop the stream.
      }
    }
    socket.onclose = onDown
    return () => socket.close()
  }

  #headers(): Record<string, string> {
    return { authorization: `Bearer ${this.#token}` }
  }
}
