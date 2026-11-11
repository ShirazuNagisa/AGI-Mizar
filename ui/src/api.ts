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
  /** Whether the agent is installed on this machine at all. */
  available: boolean
  /** Whether the operator picked it and its dependencies are in place. */
  configured: boolean
  /**
   * The lamp: enabled here and live now. DSH is live while its plugin is
   * attached; Claude/Codex only while a turn is running.
   */
  engaged: boolean
  capabilities: Capabilities
  detail: string | null
  sessions: number
}

/** Whether the first-run guide still has to be shown. */
export interface OnboardingState {
  done: boolean
}

/** One model on disk. */
export interface InstalledModel {
  id: string
  version: string
  path: string
  size_bytes: number
}

/** The sidecar that runs the model, state included. */
export interface ModelRuntime {
  state: 'stopped' | 'starting' | 'ready' | 'failed'
  port: number | null
  detail: string | null
}

/** A download in flight. */
export interface ModelDownload {
  version: string
  done: number
  total: number
  state: 'running' | 'verifying' | 'failed'
  error: string | null
}

/** What model this install has, and whether the thing that runs it is up. */
export interface ModelState {
  installed: InstalledModel[]
  active: InstalledModel | null
  previous: InstalledModel | null
  runtime: ModelRuntime
  download: ModelDownload | null
}

/** What the model repository says the newest release is. */
export interface ModelCheck {
  installed: string | null
  latest: {
    id: string
    version: string
    url: string
    sha256: string
    size_bytes: number
    /** Whether `latest` is newer than what is installed. */
    newer: boolean
  } | null
  error: string | null
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
  /** `claude_desktop` / `codex_desktop`; null for the CLI surface. */
  source: string | null
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

/** A phone paired to this computer, as the kernel sees it. */
export interface DeviceStatus {
  name: string
  /** The hardware the phone reported (`iPhone17,1`), when it reported one. */
  model?: string
  /** Whether the phone has a socket open right now. */
  online: boolean
  /** Whether the phone registered live-activity push tokens. */
  liveActivity?: boolean
  /** Epoch milliseconds of its last hello. */
  lastSeenMs: number
}

/** What this install knows about the DSH helper plugin. */
export interface DshPluginView {
  /** The version in the DSH profile, when installed. */
  installed: string | null
  /** The newest version npm published, once a check succeeded. */
  latest: string | null
  /** The oldest plugin version this app speaks to. */
  minimum: string
  /** Whether npm has a newer, compatible release. */
  updateAvailable: boolean
  /** The DSH profile the plugin lives (or would live) in. */
  profile: string
  /** Whether a usable `dsh` command was found. */
  cli: boolean
  /** Why the last npm check failed. */
  error: string | null
  /** Epoch milliseconds of the last successful check. */
  lastChecked: number | null
}

export interface DependencyItem {
  id: string
  label: string
  /** `ok`, `missing`, `outdated`, or `unknown`. */
  status: 'ok' | 'missing' | 'outdated' | 'unknown'
  detail: string
  /** What the one-click repair would do, when it can. */
  fix: string | null
}

export interface DependencyReport {
  engine: EngineId
  ok: boolean
  summary: string
  items: DependencyItem[]
  /** What a repair changed; empty for a read-only check. */
  changed: string[]
  /** Why a repair could not finish. */
  error: string | null
}

/** Whether the remote-agent feature has what it needs on this computer. */
export interface RemoteHealthReport {
  ok: boolean
  summary: string
  items: DependencyItem[]
  /** What a repair changed; empty for a read-only check. */
  changed: string[]
  /** Why a repair could not finish. */
  error: string | null
}

/** This computer's policy for accepting same-account peers. */
export interface RemotePrefs {
  enabled: boolean
  allowLan: boolean
  allowWan: boolean
  allowAllWorkspaces: boolean
  workspaces: string[]
  blocked: string[]
}

export interface StateSnapshot {
  version: string
  /** `macos`, `windows` or `linux`: which machine this window draws for. */
  platform: string
  engines: EngineStatus[]
  devices: DeviceStatus[]
  sessions: Session[]
  interactions: Interaction[]
  /** Whether the first-run guide still has to be shown. */
  onboarding: OnboardingState
  /** What model this install judges turns with, and whether it is running. */
  model: ModelState
  /** Whether this install shares turns to improve Mizarllm. */
  dataset: DatasetState
  /** The DSH helper plugin's version, and whether npm has a newer one. */
  dshPlugin: DshPluginView
}

/** The operator's answer about anonymous session sharing. */
export interface DatasetState {
  enabled: boolean
  /** The consent copy version; `0` means this machine was never asked. */
  policyVersion: number
  /** When the answer last changed, epoch milliseconds. */
  since: number | null
  /** Project names whose turns are never shared. */
  excludedProjects: string[]
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
  | { op: 'steer'; session: string; text: string }
  | { op: 'fork'; session: string; at_seq: number }
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
  | { op: 'set_theme'; theme: 'system' | 'light' | 'dark' | 'oled' }
  | { op: 'relay_status' }
  | { op: 'account' }
  | { op: 'account_login'; identifier: string; password: string }
  | { op: 'account_logout' }
  | { op: 'account_desktops' }
  | { op: 'account_github_start' }
  | { op: 'account_github_exchange'; handoff: string }
  | { op: 'account_apple_start' }
  | { op: 'account_apple_await'; state: string }
  | { op: 'account_github_await'; state: string }
  | { op: 'account_apple_exchange'; handoff: string }
  | { op: 'lan_identity' }
  | { op: 'lan_certificate' }
  | { op: 'remote_target_settings' }
  | { op: 'set_remote_target_settings'; enabled: boolean; allow_lan: boolean; allow_wan: boolean; allow_all_workspaces: boolean; workspaces: string[]; blocked: string[] }
  | { op: 'remote_audit' }
  | { op: 'remote_health'; repair?: boolean }
  | { op: 'remote_logs' }
  | { op: 'remote_install'; profile?: string; dry_run?: boolean }
  | { op: 'remote_uninstall'; profile?: string; dry_run?: boolean }
  | { op: 'remote_delegations' }
  | { op: 'remote_result'; id: string }
  | { op: 'remote_answer'; id: string; approval: string; outcome: string }
  | { op: 'remote_send'; id: string; message: string }
  | { op: 'remote_cancel'; id: string }
  | { op: 'open_external'; url: string }
  | { op: 'model_state' }
  | { op: 'model_check' }
  | { op: 'model_download'; version: string }
  | { op: 'model_cancel' }
  | { op: 'model_activate'; id: string }
  | { op: 'model_remove'; version: string }
  | { op: 'model_rollback' }
  | { op: 'engine_rescan' }
  | { op: 'engine_configure'; engine: EngineId; enabled: boolean }
  | { op: 'dependencies'; engine: EngineId }
  | { op: 'dependencies_repair'; engine: EngineId }
  | { op: 'dsh_plugin_check' }
  | { op: 'dsh_plugin_update' }
  | { op: 'onboarding_complete' }
  | { op: 'onboarding_reset' }
  | { op: 'dataset_share'; enabled: boolean }
  | { op: 'dataset_exclude'; projects: string[] }
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
  | { type: 'model_progress'; download: ModelDownload }
  | { type: 'model_changed'; model: ModelState }
  | { type: 'onboarding_changed'; done: boolean }
  | { type: 'dsh_plugin_changed'; plugin: DshPluginView }
  | { type: 'notice'; level: string; message: string }

/** The token the shell put in this page's URL, when it used one. */
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
    // A browser launched through /launch has no URL token: the HttpOnly cookie
    // rides the same-origin WebSocket handshake. A packaged window keeps the
    // query token it has always used.
    const query = this.#token === '' ? '' : `?token=${encodeURIComponent(this.#token)}`
    const socket = new WebSocket(`${scheme}://${window.location.host}/api/v1/events${query}`)
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
    return this.#token === '' ? {} : { authorization: `Bearer ${this.#token}` }
  }
}
