import { useState } from 'react'
import { DSH_PLUGIN_COMMAND, DSH_PLUGIN_VERSION } from '../engines'
import { CheckIcon } from '../icons'
import { Button } from './ui/button'
import { cn } from '../lib/utils'

/**
 * What DSH needs before this window can send it anything, and the command that
 * does it. Shown wherever DSH is being configured, so the two screens agree.
 */
export function DshPluginHint({ className }: { className?: string }) {
  const [copied, setCopied] = useState<'idle' | 'done' | 'failed'>('idle')

  async function copy() {
    try {
      await navigator.clipboard.writeText(DSH_PLUGIN_COMMAND)
      setCopied('done')
    } catch {
      setCopied('failed')
    }
  }

  return (
    <div className={cn('border border-ink p-3', className)}>
      <p className="text-[12px] leading-5 text-muted">
        DSH 需要在 agent 中安装 dsh-mizar 插件（≥ {DSH_PLUGIN_VERSION}）。
      </p>
      <code className="selectable mt-2 block break-all border border-ink px-2.5 py-1.5 text-[12px] leading-5 text-fg">
        {DSH_PLUGIN_COMMAND}
      </code>
      <div className="mt-2 flex items-center gap-2">
        <Button
          size="sm"
          variant="outline"
          onClick={() => void copy()}
          aria-live="polite"
        >
          {copied === 'done' ? <CheckIcon size={13} /> : null}
          {copied === 'done' ? '已复制' : '复制安装命令'}
        </Button>
        {copied === 'failed' ? (
          <span className="bg-ink px-1.5 text-[12px] text-bg">复制失败，请手动选中上面的命令</span>
        ) : null}
      </div>
    </div>
  )
}
