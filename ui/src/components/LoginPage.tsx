import { useState } from 'react'
import type { AccountUser } from '../api'
import { Button } from './ui/button'
import { Toolbar } from './Toolbar'
import { Spinner } from '../icons'

interface LoginPageProps {
  account: AccountUser | null
  serverConnected: boolean
  busy: boolean
  error: string | null
  onLogin: (identifier: string, password: string) => Promise<void>
  onLogout: () => Promise<void>
}

/**
 * The account this window talks to its servers with.
 *
 * The relay's account service is the authority; this page is the form that calls
 * it. A session the relay stops accepting arrives here as an error, and the page
 * falls back to the signed-out shape rather than pretending it is still signed
 * in.
 */
export function LoginPage({
  account,
  serverConnected,
  busy,
  error,
  onLogin,
  onLogout,
}: LoginPageProps) {
  const [identifier, setIdentifier] = useState('')
  const [password, setPassword] = useState('')

  async function submit(event: React.FormEvent) {
    event.preventDefault()
    if (identifier.trim() === '' || password === '') return
    await onLogin(identifier.trim(), password)
    setPassword('')
  }

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="登录" />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[560px] px-8 py-10">
          {account === null ? (
            <form onSubmit={submit}>
              <h2 className="text-[16px] font-semibold text-fg">登录后使用远程连接</h2>
              <p className="mt-2 text-[13px] leading-6 text-muted">
                手机端需要与这台电脑登录同一个账户，才能看到它的设备并连上它。
                未登录时，本地功能照常可用，只是不能远程访问。
              </p>

              <div className="mt-6 flex flex-col gap-2">
                <label className="text-[12px] text-muted" htmlFor="account-identifier">
                  邮箱或用户名
                </label>
                <input
                  id="account-identifier"
                  value={identifier}
                  onChange={(event) => setIdentifier(event.target.value)}
                  autoComplete="username"
                  autoCapitalize="off"
                  spellCheck={false}
                  className="h-9 rounded-lg border border-border bg-transparent px-3 text-[13px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                />
                <label className="mt-2 text-[12px] text-muted" htmlFor="account-password">
                  密码
                </label>
                <input
                  id="account-password"
                  type="password"
                  value={password}
                  onChange={(event) => setPassword(event.target.value)}
                  autoComplete="current-password"
                  className="h-9 rounded-lg border border-border bg-transparent px-3 text-[13px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                />
              </div>

              {error === null ? null : (
                <p className="mt-3 text-[12px] leading-5 text-red-400">{error}</p>
              )}

              <p className="mt-3 text-[12px] text-muted">
                服务器连接：{serverConnected ? '已连接' : '未连接'}
              </p>

              <Button
                className="mt-5"
                type="submit"
                variant="primary"
                size="lg"
                disabled={busy || identifier.trim() === '' || password === ''}
              >
                {busy ? <Spinner /> : null}
                登录
              </Button>
            </form>
          ) : (
            <section>
              <p className="text-[13px] text-muted">已登录</p>
              <div className="mt-3 flex flex-col divide-y divide-border rounded-xl border border-border">
                <Row label="账户" value={account.username} />
                <Row label="邮箱" value={account.email ?? '—'} />
                <Row
                  label="订阅"
                  value={
                    account.plan === 'paid'
                      ? `付费${account.paidUntil === null ? '' : ` · 至 ${date(account.paidUntil)}`}`
                      : '免费'
                  }
                />
              </div>
              <p className="mt-3 text-[12px] leading-5 text-muted">
                这台电脑已作为设备登录；同一账户下的设备见「设置 → 设备」。
              </p>
              <Button
                className="mt-5"
                variant="outline"
                disabled={busy}
                onClick={() => void onLogout()}
              >
                {busy ? <Spinner /> : null}
                退出登录
              </Button>
            </section>
          )}
        </div>
      </div>
    </div>
  )
}

function date(at: number): string {
  return new Date(at).toLocaleDateString('zh-CN')
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-6 px-4 py-3 text-[13px]">
      <span className="shrink-0 text-muted">{label}</span>
      <span className="selectable truncate text-fg">{value}</span>
    </div>
  )
}
