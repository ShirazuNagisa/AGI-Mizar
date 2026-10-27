import { useState } from 'react'
import { Button } from './ui/button'
import { Toolbar } from './Toolbar'

interface LoginPageProps {
  loggedIn: boolean
  serverConnected: boolean
  onLogin: () => void
  onLogout: () => void
}

/**
 * The account this window talks to its servers with.
 *
 * There is no account service behind the button yet, so the page shows the two
 * shapes it will have rather than inventing a signed-in device: signed out, one
 * sentence and one action; signed in, the three facts the service would send.
 */
export function LoginPage({ loggedIn, serverConnected, onLogin, onLogout }: LoginPageProps) {
  const [note, setNote] = useState<string | null>(null)

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="登录" />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[560px] px-8 py-10">
          {loggedIn ? (
            <section>
              <p className="text-[13px] text-muted">已登录</p>
              <div className="mt-3 flex flex-col divide-y divide-border rounded-xl border border-border">
                <Row label="账户" value="—" />
                <Row label="订阅" value="—" />
                <Row label="配额" value="—" />
              </div>
              <p className="mt-3 text-[12px] leading-5 text-muted">
                登录后手机才能连上这台电脑。账户服务尚未接入，以上信息由服务器返回。
              </p>
              <Button className="mt-5" variant="outline" onClick={onLogout}>
                退出登录
              </Button>
            </section>
          ) : (
            <section>
              <h2 className="text-[16px] font-semibold text-fg">登录后使用远程连接</h2>
              <p className="mt-2 text-[13px] leading-6 text-muted">
                手机端需要与这台电脑登录同一个账户，才能看到它的设备并连上它。
                未登录时，本地功能照常可用，只是不能远程访问。
              </p>
              <Button
                className="mt-6"
                variant="primary"
                size="lg"
                onClick={() => {
                  setNote('登录服务尚未接入（开发预览版）')
                  onLogin()
                }}
              >
                登录
              </Button>
              <p className="mt-3 text-[12px] text-muted">
                服务器连接：{serverConnected ? '已连接' : '未连接'}
              </p>
              {note === null ? null : <p className="mt-2 text-[12px] text-muted">{note}</p>}
            </section>
          )}
        </div>
      </div>
    </div>
  )
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between px-4 py-3 text-[13px]">
      <span className="text-muted">{label}</span>
      <span className="text-fg">{value}</span>
    </div>
  )
}
