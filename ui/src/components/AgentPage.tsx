import { useEffect, useState } from 'react'
import type { Balance, EngineId, EngineStatus } from '../api'
import type { Kernel } from '../api'
import { ENGINE_INSTALL_HINTS } from '../engines'
import { Button } from './ui/button'
import { Toolbar } from './Toolbar'
import { DshPluginHint } from './DshPluginHint'
import { ENGINE_MARKS, Spinner } from '../icons'
import { cn, failureMessage } from '../lib/utils'

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
 *
 * Two states come before the content, because "not installed" and "installed but
 * never configured" are different problems with different fixes: one is a trip to
 * a terminal, the other is one click.
 */
export function AgentPage({ engine, name, status, kernel, onReconnect }: AgentPageProps) {
  const connected = status?.connected ?? false
  const installed = status?.available ?? true
  const configured = status?.configured ?? true
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
      setNote(failureMessage(error, '没有成功'))
    } finally {
      setBusy(false)
    }
  }

  /** Discovery again: for an agent installed after this window opened. */
  async function rescan() {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'engine_rescan' })
      await onReconnect(engine)
      setNote('已重新检测')
    } catch (error) {
      setNote(failureMessage(error, '重新检测没有成功'))
    } finally {
      setBusy(false)
    }
  }

  /** One click for an agent that is here but was never switched on. */
  async function configure() {
    setBusy(true)
    setNote(null)
    try {
      await kernel.send({ op: 'engine_configure', engine, enabled: true })
      await onReconnect(engine)
      setNote('已配置')
    } catch (error) {
      setNote(failureMessage(error, '配置没有成功'))
    } finally {
      setBusy(false)
    }
  }

  const Mark = ENGINE_MARKS[engine]

  if (!installed || !configured) {
    return (
      <div className="flex h-full min-w-0 flex-1 flex-col">
        <Toolbar
          title={name}
          badge={<StatusLamp connected={connected} installed={installed} />}
        />
        <div className="flex flex-1 items-center justify-center overflow-y-auto px-8 py-10">
          <div className="w-full max-w-[440px] text-center">
            <Mark size={26} className="mx-auto text-muted" />
            {installed ? (
              <>
                <h2 className="mt-4 text-[15px] font-semibold text-fg">尚未配置该 Agent</h2>
                <p className="mt-2 text-[12px] leading-5 text-muted">
                  {name} 已经装在这台电脑上，但还没有在这里启用。启用后它的会话才会出现在左侧。
                </p>
                <Button
                  className="mt-5"
                  variant="primary"
                  onClick={() => void configure()}
                  disabled={busy}
                >
                  {busy ? <Spinner /> : null}
                  一键配置
                </Button>
                {engine === 'dsh' ? (
                  <DshPluginHint className="mt-4 text-left" />
                ) : null}
              </>
            ) : (
              <>
                <h2 className="mt-4 text-[15px] font-semibold text-fg">本机未检测到该 Agent</h2>
                <p className="mt-2 text-[12px] leading-5 text-muted">
                  {ENGINE_INSTALL_HINTS[engine]}
                </p>
                <p className="mt-1 text-[11px] leading-5 text-muted">
                  装好后回到这里点「重新检测」，不用重启应用。
                </p>
                <Button
                  className="mt-5"
                  variant="primary"
                  onClick={() => void rescan()}
                  disabled={busy}
                >
                  {busy ? <Spinner /> : null}
                  重新检测
                </Button>
              </>
            )}
            {note === null ? null : (
              <p className="mt-3 text-[12px] leading-5 text-muted">{note}</p>
            )}
          </div>
        </div>
      </div>
    )
  }

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title={name} badge={<StatusLamp connected={connected} installed={installed} />}>
        {engine === 'dsh' ? (
          <Button size="sm" onClick={() => void run('已重新检查连接')} disabled={busy}>
            {busy ? <Spinner /> : null}
            重新连接
          </Button>
        ) : (
          <Button size="sm" onClick={() => void rescan()} disabled={busy}>
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

function StatusLamp({ connected, installed }: { connected: boolean; installed: boolean }) {
  return (
    <span className="flex items-center gap-1.5 text-[12px] text-muted">
      <span
        className={cn(
          'size-2 rounded-full',
          connected
            ? 'bg-accent shadow-[0_0_0_3px_var(--accent-soft)]'
            : installed
              ? 'bg-muted/40'
              : 'bg-muted/20',
        )}
      />
      {connected ? '已连接' : installed ? '未连接' : '未安装'}
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
