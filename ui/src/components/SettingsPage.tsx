import { useState } from 'react'
import type { Kernel, PairingView } from '../api'
import type { ThemePreference } from '../theme'
import { ChevronIcon } from '../icons'
import { Button } from './ui/button'
import { Select, SelectContent, SelectItem, SelectTrigger, SelectValue } from './ui/select'
import { Toolbar } from './Toolbar'
import { cn } from '../lib/utils'

interface SettingsPageProps {
  version: string
  loggedIn: boolean
  serverConnected: boolean
  theme: ThemePreference
  onThemeChange: (theme: ThemePreference) => void
  kernel: Kernel
}

/**
 * Everything that is not about one agent.
 *
 * Device lists are read-only and empty until the account service exists — the
 * page says so rather than drawing devices this computer cannot know about.
 * The pairing panel is the one temporary thing here: until accounts arrive it
 * is the only way a phone can attach, so it lives under 开发者.
 */
export function SettingsPage({
  version,
  loggedIn,
  serverConnected,
  theme,
  onThemeChange,
  kernel,
}: SettingsPageProps) {
  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="设置" />
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[720px] px-8 py-6">
          <Section title="设备">
            <Group title="AGI Mizar 桌面端">
              {loggedIn ? (
                <Empty>这台电脑</Empty>
              ) : (
                <Empty>登录后可查看同一账户下的桌面端</Empty>
              )}
            </Group>
            <Group title="Mizar 手机端">
              {loggedIn ? (
                <Empty>这个账户下还没有手机</Empty>
              ) : (
                <Empty>登录后可查看同一账户下的手机</Empty>
              )}
            </Group>
          </Section>

          <Section title="连接状态">
            <div className="flex items-center justify-between rounded-xl border border-border px-4 py-3">
              <span className="flex items-center gap-2 text-[13px] text-fg">
                <span
                  className={cn(
                    'size-2 rounded-full',
                    serverConnected ? 'bg-accent' : 'bg-muted/40',
                  )}
                />
                {serverConnected ? '已连接' : '未连接'}
              </span>
              <span className="text-[12px] text-muted">
                {loggedIn ? '中继由账户决定' : '登录后自动连接'}
              </span>
            </div>
          </Section>

          <Section title="外观">
            <div className="flex items-center justify-between rounded-xl border border-border px-4 py-2.5">
              <span className="text-[13px] text-fg">主题</span>
              <Select
                value={theme}
                onValueChange={(value) => onThemeChange(value as ThemePreference)}
              >
                <SelectTrigger aria-label="主题">
                  <SelectValue />
                </SelectTrigger>
                <SelectContent>
                  <SelectItem value="system">跟随系统</SelectItem>
                  <SelectItem value="light">浅色</SelectItem>
                  <SelectItem value="dark">深色</SelectItem>
                </SelectContent>
              </Select>
            </div>
          </Section>

          <Section title="关于">
            <div className="divide-y divide-border rounded-xl border border-border">
              <AboutRow label="版本" value={version === '' ? '—' : version} />
              <AboutRow label="构建" value="开发预览版" />
              <AboutRow label="版权" value="© 2026 AGI Mizar" />
            </div>
          </Section>

          <DeveloperPairing kernel={kernel} />
        </div>
      </div>
    </div>
  )
}

function Section({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="mb-7">
      <h2 className="mb-2 text-[12px] font-medium text-muted">{title}</h2>
      <div className="flex flex-col gap-2">{children}</div>
    </section>
  )
}

function Group({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-xl border border-border">
      <div className="border-b border-border/70 px-4 py-2 text-[12px] text-muted">{title}</div>
      <div className="px-4 py-3">{children}</div>
    </div>
  )
}

function Empty({ children }: { children: React.ReactNode }) {
  return <p className="text-[13px] text-muted">{children}</p>
}

function AboutRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between px-4 py-2.5 text-[13px]">
      <span className="text-muted">{label}</span>
      <span className="selectable text-fg">{value}</span>
    </div>
  )
}

/** The temporary way a phone attaches, until accounts replace it. */
function DeveloperPairing({ kernel }: { kernel: Kernel }) {
  const [open, setOpen] = useState(false)
  const [view, setView] = useState<PairingView | null>(null)
  const [error, setError] = useState<string | null>(null)

  async function reveal() {
    setOpen(true)
    setError(null)
    try {
      setView(await kernel.send<PairingView>({ op: 'pairing', refresh: true }))
    } catch (failure) {
      setError(failure instanceof Error ? failure.message : '配对码读取失败')
    }
  }

  return (
    <section className="mb-6">
      <button
        type="button"
        onClick={() => (open ? setOpen(false) : void reveal())}
        className="flex items-center gap-1.5 text-[12px] text-muted hover:text-fg"
      >
        <ChevronIcon size={13} className={open ? '' : '-rotate-90'} />
        开发者
      </button>
      {open ? (
        <div className="mt-2 rounded-xl border border-border p-4">
          <p className="text-[12px] leading-5 text-muted">
            账户体系上线前的临时入口：手机扫码配对这台电脑。上线后删除。
          </p>
          {error === null ? null : <p className="mt-2 text-[12px] text-muted">{error}</p>}
          {view === null ? null : (
            <div className="mt-3 flex items-center gap-4">
              {view.svg === null ? null : (
                <div
                  className="size-[132px] rounded-lg bg-white p-2"
                  dangerouslySetInnerHTML={{ __html: view.svg }}
                />
              )}
              <div>
                <div className="selectable text-[15px] font-semibold tracking-wide text-fg">
                  {view.code}
                </div>
                <p className="mt-1 text-[12px] text-muted">在 Mizar 手机端扫码或输入此码</p>
                <Button className="mt-2" size="sm" variant="outline" onClick={() => void reveal()}>
                  换一个码
                </Button>
              </div>
            </div>
          )}
        </div>
      ) : null}
    </section>
  )
}
