import { SHELL_SETTING_ROWS } from '../shell'
import type { ShellSettingsState } from '../lib/use-shell-settings'
import { Switch } from './ui/switch'
import { cn } from '../lib/utils'

/**
 * The three switches the shell owns.
 *
 * The guide and the settings page show the same three, in the same words, so
 * they cannot drift apart. When the window is not inside the shell there is
 * nothing to read or write, and every switch says so rather than pretending.
 */
export function ShellSettingsList({
  shell,
  className,
}: {
  shell: ShellSettingsState
  className?: string
}) {
  return (
    <div className={cn('mz-border', className)}>
      <div className="divide-y divide-ink">
        {SHELL_SETTING_ROWS.map((row) => (
          <div key={row.field} className="flex items-start justify-between gap-6 px-4 py-3">
            <div className="min-w-0">
              <div className="text-[12px] text-fg">{row.title}</div>
              <p className="mt-0.5 text-[12px] leading-5 text-muted">{row.detail}</p>
            </div>
            <Switch
              className="mt-0.5"
              label={row.title}
              checked={shell.settings?.[row.field] ?? false}
              disabled={!shell.available || shell.settings === null || shell.busy === row.field}
              onCheckedChange={(next) => shell.set(row.field, next)}
            />
          </div>
        ))}
      </div>
      {shell.available ? null : (
        <p className="mz-divide-t px-4 py-2 text-[12px] leading-5 text-muted">
          这三个开关由桌面窗口负责（仅桌面窗口可用）。
        </p>
      )}
      {shell.loading ? (
        <p className="mz-divide-t px-4 py-2 text-[12px] text-muted">正在读取…</p>
      ) : null}
      {shell.error === null ? null : (
        <p className="mz-divide-t bg-ink px-4 py-1.5 text-[12px] leading-5 text-bg">
          {shell.error}
        </p>
      )}
    </div>
  )
}
