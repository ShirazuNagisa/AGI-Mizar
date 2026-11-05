import { useState } from 'react'
import type { DshPluginView, Kernel } from '../api'
import { DSH_PLUGIN_COMMAND, DSH_PLUGIN_VERSION } from '../engines'
import { CheckIcon, Spinner } from '../icons'
import { Button } from './ui/button'
import { cn, failureMessage } from '../lib/utils'

/**
 * What DSH needs before this window can send it anything, and the two ways to
 * get it: one click through DSH's own command, or the command itself for a host
 * this app cannot drive. Shown wherever DSH is being configured, so the screens
 * agree.
 */
export function DshPluginHint({
  className,
  kernel,
}: {
  className?: string
  /** Without it the hint is instructions only; with it there is a button. */
  kernel?: Kernel
}) {
  const [copied, setCopied] = useState<'idle' | 'done' | 'failed'>('idle')
  const [installing, setInstalling] = useState(false)
  const [result, setResult] = useState<string | null>(null)

  async function copy() {
    try {
      await navigator.clipboard.writeText(DSH_PLUGIN_COMMAND)
      setCopied('done')
    } catch {
      setCopied('failed')
    }
  }

  async function install() {
    if (kernel === undefined) return
    setInstalling(true)
    setResult(null)
    try {
      const view = await kernel.send<DshPluginView>({ op: 'dsh_plugin_update' })
      setResult(
        `已安装 ${view.installed ?? view.latest ?? '最新版本'}，重启 DeepSeek Harness 后生效`,
      )
    } catch (failure) {
      setResult(failureMessage(failure, '安装没有成功'))
    } finally {
      setInstalling(false)
    }
  }

  return (
    <div className={cn('mz-border p-3', className)}>
      <p className="text-[12px] leading-5 text-muted">
        DSH 需要在 agent 中安装 dsh-mizar 插件（≥ {DSH_PLUGIN_VERSION}）。
      </p>
      <div className="mt-2 flex flex-wrap items-center gap-2">
        {kernel === undefined ? null : (
          <Button
            size="sm"
            variant="primary"
            onClick={() => void install()}
            disabled={installing}
          >
            {installing ? <Spinner /> : null}
            {installing ? '正在安装…' : '一键安装插件'}
          </Button>
        )}
        <Button size="sm" variant="outline" onClick={() => void copy()} aria-live="polite">
          {copied === 'done' ? <CheckIcon size={13} /> : null}
          {copied === 'done' ? '已复制' : '复制安装命令'}
        </Button>
        {copied === 'failed' ? (
          <span className="bg-ink px-1.5 text-[12px] text-bg">复制失败，请手动选中下面的命令</span>
        ) : null}
      </div>
      <code className="selectable mt-2 block break-all mz-border px-2.5 py-1.5 text-[12px] leading-5 text-fg">
        {DSH_PLUGIN_COMMAND}
      </code>
      {result === null ? null : (
        <p className="mt-2 text-[12px] leading-5 text-fg" aria-live="polite">
          {result}
        </p>
      )}
    </div>
  )
}
