import { useEffect, useState } from 'react'
import type { DshPluginView, Kernel } from '../api'
import { Button } from './ui/button'
import { Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'
import { useIsOled } from '../lib/oled'

interface DshPluginUpdateDialogProps {
  plugin: DshPluginView
  kernel: Kernel
  /** Close without updating; the same release is not asked about again this session. */
  onDismiss: () => void
  /** The kernel's answer after a successful install. */
  onUpdated: (plugin: DshPluginView) => void
}

/**
 * The one popup the background check is allowed to open.
 *
 * It appears at launch (or when a later check finds a release) and offers the
 * same thing the DSH page does. The install runs through DSH's own command, so
 * the dialog has to be honest about the second half: the plugin is on disk when
 * the button returns, but DSH keeps the version it loaded until it restarts.
 */
export function DshPluginUpdateDialog({
  plugin,
  kernel,
  onDismiss,
  onUpdated,
}: DshPluginUpdateDialogProps) {
  const oled = useIsOled()
  const [busy, setBusy] = useState(false)
  const [done, setDone] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !busy) onDismiss()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [busy, onDismiss])

  async function update() {
    setBusy(true)
    setError(null)
    try {
      const next = await kernel.send<DshPluginView>({ op: 'dsh_plugin_update' })
      onUpdated(next)
      setDone(next.installed ?? next.latest ?? '最新版本')
    } catch (failure) {
      setError(failureMessage(failure, '更新插件没有成功'))
    } finally {
      setBusy(false)
    }
  }

  const installed = plugin.installed === null ? '未安装' : plugin.installed
  const canUpdate = plugin.cli
  const installing = plugin.installed === null

  return (
    <div
      className={cn(
        'fixed inset-0 z-50 flex items-center justify-center px-6',
        oled ? 'bg-bg' : 'bg-black/40',
      )}
      role="presentation"
      onClick={() => {
        if (!busy) onDismiss()
      }}
    >
      <div
        role="dialog"
        aria-modal="true"
        aria-labelledby="dsh-plugin-update-title"
        className="mz-border w-full max-w-[440px] bg-bg p-5"
        onClick={(event) => event.stopPropagation()}
      >
        <div className="text-[12px] tracking-[1px] text-muted">DSH 插件更新</div>
        <h2 id="dsh-plugin-update-title" className="mt-1 text-[24px] leading-8">
          {installing ? '安装 dsh-mizar 插件' : 'dsh-mizar 有新版本'}
        </h2>
        <p className="mt-2 text-[12px] leading-5 text-muted">
          本机 {installed} · npm 最新 {plugin.latest}。{installing ? '安装' : '更新'}会通过 DSH
          自己的插件命令完成；完成后需要重启 DeepSeek Harness 才会加载新版本。
        </p>
        {canUpdate ? null : (
          <p className="mt-2 bg-ink px-2 py-1.5 text-[12px] leading-5 text-bg">
            没有找到 dsh 命令行，无法一键安装。请确认 DeepSeek Harness 已安装，或按 DSH
            页面上的命令手动安装。
          </p>
        )}
        {done === null ? null : (
          <p className="mt-3 bg-ink px-2 py-1.5 text-[12px] leading-5 text-bg">
            已{installing ? '安装' : '更新'}到 {done}，重启 DeepSeek Harness 后生效。
          </p>
        )}
        {error === null ? null : (
          <p className="mt-3 bg-ink px-2 py-1.5 text-[12px] leading-5 text-bg">{error}</p>
        )}
        <div className="mt-4 flex justify-end gap-2">
          <Button size="sm" variant="ghost" onClick={onDismiss} disabled={busy}>
            {done === null ? '稍后' : '关闭'}
          </Button>
          {done === null && canUpdate ? (
            <Button size="sm" variant="primary" onClick={() => void update()} disabled={busy}>
              {busy ? <Spinner /> : null}
              {busy ? (installing ? '正在安装…' : '正在更新…') : installing ? '现在安装' : '现在更新'}
            </Button>
          ) : null}
        </div>
      </div>
    </div>
  )
}
