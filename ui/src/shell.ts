/**
 * The things only the desktop shell can do.
 *
 * Installing an update, writing a login item, hiding the Dock icon and quitting
 * are all process-level actions the kernel has no business owning, so they are
 * Tauri IPC commands rather than kernel ops. The window is served from a
 * loopback origin, so this module is the one place that knows the bridge exists
 * at all — and the one place that degrades when it does not: opened in a plain
 * browser (or before the shell's capabilities land) every call throws
 * `仅桌面窗口可用` and the pages grey their buttons out instead.
 */

export interface UpdateStatus {
  current: string
  latest: string | null
  available: boolean
  error: string | null
}

export interface ShellSettings {
  launch_at_login: boolean
  show_dock_icon: boolean
  tray_menu_only: boolean
}

export type ShellSettingsPatch = Partial<ShellSettings>

/** One switch, with the words that explain it. Shared by the guide and 设置. */
export interface ShellSettingRow {
  field: keyof ShellSettings
  title: string
  detail: string
}

export const SHELL_SETTING_ROWS: ShellSettingRow[] = [
  {
    field: 'launch_at_login',
    title: '开机静默启动',
    detail: '登录 macOS 后直接在后台启动，不弹出窗口。',
  },
  {
    field: 'show_dock_icon',
    title: '在程序坞显示图标',
    detail: '关闭后只在菜单栏留一个图标，程序坞里不再有它。',
  },
  {
    field: 'tray_menu_only',
    title: '点击托盘只打开快捷菜单',
    detail: '关闭时点击菜单栏图标会唤出主窗口。',
  },
]

type Invoke = (command: string, args?: Record<string, unknown>) => Promise<unknown>

/** The bridge Tauri injects into every window it serves. */
function bridge(): Invoke | null {
  const internals = (window as unknown as { __TAURI_INTERNALS__?: { invoke?: unknown } })
    .__TAURI_INTERNALS__
  const invoke = internals?.invoke
  return typeof invoke === 'function' ? (invoke as Invoke) : null
}

/** Whether this page is running inside the desktop shell. */
export function shellAvailable(): boolean {
  return bridge() !== null
}

async function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  const invoke = bridge()
  if (invoke === null) throw new Error('仅桌面窗口可用')
  return (await invoke(command, args)) as T
}

/** A shell failure as a line a person can read. */
export function shellFailure(failure: unknown): string {
  if (typeof failure === 'string') return failure
  if (failure instanceof Error) return failure.message
  return '桌面命令没有成功'
}

/** `{ current, latest, available, error }`: ask the updater right now. */
export function updateCheck(): Promise<UpdateStatus> {
  return call<UpdateStatus>('app_update_check')
}

/** The same shape, without going to the network: the last check's result. */
export function updateState(): Promise<UpdateStatus> {
  return call<UpdateStatus>('app_update_state')
}

/** Download, install and relaunch. Does not return on success. */
export function updateInstall(): Promise<void> {
  return call<void>('app_update_install')
}

export function shellSettingsGet(): Promise<ShellSettings> {
  return call<ShellSettings>('shell_settings_get')
}

/** Write the given fields and answer with the whole settings object. */
export function shellSettingsSet(settings: ShellSettingsPatch): Promise<ShellSettings> {
  return call<ShellSettings>('shell_settings_set', { settings })
}

export function quitApp(): Promise<void> {
  return call<void>('app_quit')
}
