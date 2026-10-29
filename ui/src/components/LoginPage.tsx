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
  onGithubStart: () => Promise<void>
  onGithubExchange: (handoff: string) => Promise<void>
  onAppleStart?: () => Promise<void>
  onAppleExchange?: (handoff: string) => Promise<void>
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
  onGithubStart,
  onGithubExchange,
  onAppleStart,
  onAppleExchange,
}: LoginPageProps) {
  const [identifier, setIdentifier] = useState('')
  const [password, setPassword] = useState('')
  const [codeStarted, setCodeStarted] = useState<'github' | 'apple' | null>(null)
  const [handoff, setHandoff] = useState('')

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

              <div className="my-5 flex items-center gap-3 text-[11px] text-muted">
                <span className="h-px flex-1 bg-border" />
                或
                <span className="h-px flex-1 bg-border" />
              </div>

              <div className="flex flex-col gap-2">
                <Button
                  variant="outline"
                  size="lg"
                  disabled={busy}
                  onClick={() => {
                    void onGithubStart()
                    setCodeStarted('github')
                  }}
                >
                  <span aria-hidden></span>
                  使用 GitHub 登录
                </Button>
                {onAppleStart === undefined ? null : (
                  <Button
                    variant="outline"
                    size="lg"
                    disabled={busy}
                    onClick={() => {
                      void onAppleStart()
                      setCodeStarted('apple')
                    }}
                  >
                     Apple 登录
                  </Button>
                )}
              </div>

              {codeStarted !== null ? (
                <div className="mt-4 rounded-xl border border-border p-4">
                  <p className="text-[12px] leading-5 text-muted">
                    浏览器里完成 {codeStarted === 'apple' ? 'Apple' : 'GitHub'} 授权后，页面会给出一个
                    <code>MZ-XXXX-XXXX</code> 代码，粘贴到这里完成登录。
                  </p>
                  <div className="mt-3 flex gap-2">
                    <input
                      value={handoff}
                      onChange={(event) => setHandoff(event.target.value.toUpperCase())}
                      placeholder="MZ-XXXX-XXXX"
                      spellCheck={false}
                      className="h-9 flex-1 rounded-lg border border-border bg-transparent px-3 font-mono text-[13px] tracking-wider text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                    />
                    <Button
                      variant="primary"
                      disabled={busy || handoff.trim().length < 5}
                      onClick={() =>
                        void (codeStarted === 'apple' && onAppleExchange !== undefined
                          ? onAppleExchange(handoff.trim())
                          : onGithubExchange(handoff.trim()))
                      }
                    >
                      完成
                    </Button>
                  </div>
                </div>
              ) : null}
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
