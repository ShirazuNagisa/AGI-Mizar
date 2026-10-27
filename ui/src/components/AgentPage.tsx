import { useEffect, useState } from 'react'
import type { Balance, EngineId, EngineStatus } from '../api'
import type { Kernel } from '../api'
import { Button } from './ui/button'
import { Toolbar } from './Toolbar'
import { Spinner } from '../icons'
import { cn } from '../lib/utils'

interface AgentPageProps {
  engine: EngineId
  name: string
  status: EngineStatus | undefined
  kernel: Kernel
  onReconnect: (engine: EngineId) => Promise<void>
}

/**
 * One agent's page: what this computer knows about it, and the one action that
 * is useful when it is not working. There are deliberately no technical numbers
 * here — the operator does not need a port to decide whether DSH is attached.
 */
export function AgentPage({ engine, name, status, kernel, onReconnect }: AgentPageProps) {
  const connected = status?.connected ?? false
  const [balance, setBalance] = useState<Balance | null>(null)
  const [busy, setBusy] = useState(false)
  const [note, setNote] = useState<string | null>(null)

  useEffect(() => {
    setBalance(null)
    if (engine !== 'dsh') return
    // Reading before the plugin attaches answers "this host has no account",
    // which is a different claim than "it is not connected yet". Wait for it.
    if (!connected) return
    let cancelled = false
    void kernel
      .send<Balance>({ op: 'balance', engine: 'dsh' })
      .then((value) => {
        if (!cancelled) setBalance(value)
      })
      .catch(() => {
        if (!cancelled) setBalance({ status: 'unavailable' })
      })
    return () => {
      cancelled = true
    }
  }, [kernel, engine, connected])

  async function run(label: string) {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'reconnect', engine })
      await onReconnect(engine)
      setNote(label)
    } catch (error) {
      setNote(error instanceof Error ? error.message : '没有成功')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar
        title={name}
        badge={<StatusLamp connected={connected} />}
      >
        {engine === 'dsh' ? (
          <Button size="sm" onClick={() => void run('已重新检查连接')} disabled={busy}>
            {busy ? <Spinner /> : null}
            重新连接
          </Button>
        ) : (
          <Button size="sm" onClick={() => void run('已重新检测')} disabled={busy}>
            {busy ? <Spinner /> : null}
            重新检测
          </Button>
        )}
      </Toolbar>

      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
          {note === null ? null : <p className="mb-4 text-[12px] text-muted">{note}</p>}

          {engine === 'dsh' ? (
            <section>
              <h2 className="text-[12px] font-medium text-muted">账户余额</h2>
              <div className="mt-2 rounded-xl border border-border px-4 py-3">
                <BalanceRows balance={balance} connected={connected} />
              </div>
            </section>
          ) : (
            <section>
              <p className="text-[13px] leading-6 text-muted">
                {name} 在本机以命令行工具运行，模型与权限由它自己的配置文件管理。
                这里暂时只保留重新检测。
              </p>
            </section>
          )}
        </div>
      </div>
    </div>
  )
}

function StatusLamp({ connected }: { connected: boolean }) {
  return (
    <span className="flex items-center gap-1.5 text-[12px] text-muted">
      <span
        className={cn(
          'size-2 rounded-full',
          connected ? 'bg-accent shadow-[0_0_0_3px_var(--accent-soft)]' : 'bg-muted/40',
        )}
      />
      {connected ? '已连接' : '未连接'}
    </span>
  )
}

function BalanceRows({ balance, connected }: { balance: Balance | null; connected: boolean }) {
  if (!connected) {
    return <p className="text-[13px] text-muted">连接后读取</p>
  }
  if (balance === null) {
    return <p className="text-[13px] text-muted">正在读取…</p>
  }
  switch (balance.status) {
    case 'ready':
      return (
        <div className="flex flex-col gap-2">
          {balance.wallets.length === 0 ? (
            <p className="text-[13px] text-muted">余额为空</p>
          ) : (
            balance.wallets.map((wallet) => (
              <div key={wallet.currency} className="flex items-baseline justify-between">
                <span className="text-[12px] text-muted">{wallet.currency}</span>
                <span className="text-[22px] font-semibold text-fg tabular">{wallet.amount}</span>
              </div>
            ))
          )}
        </div>
      )
    case 'signed_out':
      return <p className="text-[13px] text-muted">需要先在 DSH 中登录账户</p>
    case 'unsupported':
      return <p className="text-[13px] text-muted">DSH 没有提供余额</p>
    default:
      return <p className="text-[13px] text-muted">余额暂时读不到</p>
  }
}
