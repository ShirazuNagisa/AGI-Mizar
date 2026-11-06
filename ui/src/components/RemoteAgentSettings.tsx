/**
 * The remote-agent settings surface.
 *
 * Two halves in one panel because the same operator is usually both sides: the
 * target policy (who may use this computer, in which folders) and the LAN
 * certificate that proves this computer to a peer. The task list is read-only
 * here; cancelling is the one action it needs.
 */
import { useEffect, useState } from 'react'
import type { Kernel } from '../api'
import { Button } from './ui/button'
import { PanelGroup } from './ui/panel'
import { Switch } from './ui/switch'

interface RemotePrefs {
  enabled: boolean
  allowLan: boolean
  allowWan: boolean
  workspaces: string[]
  blocked: string[]
}

interface CertificateBody {
  v: number
  accountId: string
  deviceId: string
  ed25519Public: string
  x25519Public: string
  issuedAt: number
  expiresAt: number
}

interface LanStatus {
  deviceId: string | null
  ed25519Public: string
  x25519Public: string
  certificate: CertificateBody | null
  caPublicKey: string | null
  path: string
}

interface AuditEvent {
  atMs: number
  delegationId: string
  peer: string
  route: string
  event: string
  state: string
  risk?: string
  note?: string
}

interface Approval {
  requestId: string
  risk: 'low' | 'high'
  reason: string
  requestedAt: number
}

interface Delegation {
  id: string
  peer: string
  peerName: string
  state: string
  route?: string
  promptPreview: string
  startedAt: number
  finalText?: string
  error?: string
  approval?: Approval | null
}

function day(ms: number): string {
  return new Date(ms).toLocaleDateString([], { year: 'numeric', month: '2-digit', day: '2-digit' })
}

export function RemoteAgentSettings({ kernel }: { kernel: Kernel }) {
  const [prefs, setPrefs] = useState<RemotePrefs | null>(null)
  const [lan, setLan] = useState<LanStatus | null>(null)
  const [delegations, setDelegations] = useState<Delegation[]>([])
  const [audit, setAudit] = useState<AuditEvent[]>([])
  const [workspaces, setWorkspaces] = useState('')
  const [blocked, setBlocked] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [installOutput, setInstallOutput] = useState<string | null>(null)
  const [followUpFor, setFollowUpFor] = useState<string | null>(null)
  const [followUpText, setFollowUpText] = useState('')
  const [saved, setSaved] = useState(false)

  const load = async () => {
    setError(null)
    try {
      const [nextPrefs, nextLan, tasks, history] = await Promise.all([
        kernel.send<RemotePrefs>({ op: 'remote_target_settings' }),
        kernel.send<LanStatus>({ op: 'lan_identity' }),
        kernel.send<{ delegations: Delegation[] }>({ op: 'remote_delegations' }),
        kernel.send<{ events: AuditEvent[] }>({ op: 'remote_audit' }),
      ])
      setPrefs(nextPrefs)
      setWorkspaces(nextPrefs.workspaces.join('\n'))
      setBlocked(nextPrefs.blocked.join('\n'))
      setLan(nextLan)
      setDelegations(tasks.delegations)
      setAudit(history.events.slice().reverse())
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }

  useEffect(() => {
    void load()
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  const save = async (next: RemotePrefs) => {
    setPrefs(next)
    setSaved(false)
    try {
      await kernel.send<RemotePrefs>({
        op: 'set_remote_target_settings',
        enabled: next.enabled,
        allow_lan: next.allowLan,
        allow_wan: next.allowWan,
        workspaces: next.workspaces,
        blocked: next.blocked,
      })
      setSaved(true)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }

  const install = async (uninstall: boolean) => {
    setBusy(true)
    setError(null)
    setInstallOutput(null)
    try {
      const result = await kernel.send<{ stdout: string; stderr: string }>(
        uninstall ? { op: 'remote_uninstall' } : { op: 'remote_install' },
      )
      setInstallOutput([result.stdout, result.stderr].filter(Boolean).join('\n'))
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusy(false)
    }
  }

  const refreshCertificate = async () => {
    setBusy(true)
    setError(null)
    try {
      const next = await kernel.send<LanStatus>({ op: 'lan_certificate' })
      setLan(next)
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusy(false)
    }
  }

  const answer = async (task: Delegation, outcome: 'allowed-once' | 'rejected') => {
    if (task.approval === undefined || task.approval === null) return
    setBusy(true)
    try {
      await kernel.send({
        op: 'remote_answer',
        id: task.id,
        approval: task.approval.requestId,
        outcome,
      })
      await load()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusy(false)
    }
  }

  const sendFollowUp = async (task: Delegation) => {
    if (followUpText.trim() === '') return
    setBusy(true)
    setError(null)
    try {
      await kernel.send({ op: 'remote_send', id: task.id, message: followUpText.trim() })
      setFollowUpFor(null)
      setFollowUpText('')
      await load()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    } finally {
      setBusy(false)
    }
  }

  const exportAudit = () => {
    const blob = new Blob([JSON.stringify(audit, null, 2)], { type: 'application/json' })
    const url = URL.createObjectURL(blob)
    const link = document.createElement('a')
    link.href = url
    link.download = 'agi-mizar-remote-audit.json'
    link.click()
    URL.revokeObjectURL(url)
  }

  const cancel = async (id: string) => {
    try {
      await kernel.send({ op: 'remote_cancel', id })
      await load()
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause))
    }
  }

  if (prefs === null) {
    return (
      <PanelGroup title="远程 Agent">
        <p className="text-[12px] text-muted">{error ?? '读取中…'}</p>
      </PanelGroup>
    )
  }

  return (
    <div className="flex flex-col gap-2">
      <PanelGroup title="被远程使用（这台电脑）">
        <div className="flex flex-col gap-3">
          <Row
            label="允许远程 Agent"
            hint="关闭时 19393 不监听，任何 peer token 都不会被签发"
            checked={prefs.enabled}
            onChange={(enabled) => void save({ ...prefs, enabled })}
          />
          <Row
            label="允许局域网"
            hint="同账户、同一局域网；不经过 relay，免费"
            checked={prefs.allowLan}
            onChange={(allowLan) => void save({ ...prefs, allowLan })}
          />
          <Row
            label="允许跨网（付费）"
            hint="通过账户 relay；需要付费账户"
            checked={prefs.allowWan}
            onChange={(allowWan) => void save({ ...prefs, allowWan })}
          />
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] text-fg">可远程使用的工作区</span>
            <textarea
              className="min-h-24 w-full resize-y border border-border bg-transparent px-2 py-1.5 font-mono text-[12px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
              value={workspaces}
              spellCheck={false}
              onChange={(event) => setWorkspaces(event.target.value)}
              placeholder="/Users/you/project"
            />
            <span className="text-[11px] text-muted">
              一行一个绝对路径。远程 peer 只能在这些目录里创建会话。
            </span>
          </label>
          <label className="flex flex-col gap-1.5">
            <span className="text-[12px] text-fg">拒绝的设备</span>
            <textarea
              className="min-h-16 w-full resize-y border border-border bg-transparent px-2 py-1.5 font-mono text-[12px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
              value={blocked}
              spellCheck={false}
              onChange={(event) => setBlocked(event.target.value)}
              placeholder="device-id"
            />
            <span className="text-[11px] text-muted">
              一行一个 deviceId。即使 token 有效，LAN/WAN 连接也会被拒绝。
            </span>
          </label>
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              onClick={() =>
                void save({
                  ...prefs,
                  workspaces: workspaces
                    .split('\n')
                    .map((line) => line.trim())
                    .filter(Boolean),
                  blocked: blocked
                    .split('\n')
                    .map((line) => line.trim())
                    .filter(Boolean),
                })
              }
            >
              保存工作区与拒绝列表
            </Button>
            {saved ? <span className="text-[11px] text-muted">已保存</span> : null}
          </div>
        </div>
      </PanelGroup>

      <PanelGroup title="局域网证书">
        <div className="flex flex-col gap-2 text-[12px]">
          <div className="flex justify-between gap-4">
            <span className="text-muted">设备</span>
            <span className="truncate text-fg">{lan?.deviceId ?? '未登录'}</span>
          </div>
          <div className="flex justify-between gap-4">
            <span className="text-muted">证书</span>
            <span className="text-fg">
              {lan?.certificate
                ? `有效至 ${day(lan.certificate.expiresAt)}`
                : '未签发'}
            </span>
          </div>
          <div className="flex justify-between gap-4">
            <span className="text-muted">身份文件</span>
            <span className="truncate text-fg">{lan?.path ?? '—'}</span>
          </div>
          <div className="flex items-center gap-2">
            <Button size="sm" disabled={busy} onClick={() => void refreshCertificate()}>
              {busy ? '刷新中…' : '刷新证书'}
            </Button>
            <span className="text-[11px] text-muted">
              证书绑定本机 Ed25519/X25519 公钥；换机不会复用。
            </span>
          </div>
        </div>
      </PanelGroup>

      <PanelGroup title="远程任务">
        {delegations.length === 0 ? (
          <p className="text-[12px] text-muted">还没有委派记录。</p>
        ) : (
          <div className="flex flex-col divide-y divide-border">
            {delegations.map((task) => (
              <div key={task.id} className="flex flex-col gap-1 py-2 text-[12px]">
                <div className="flex items-center gap-2">
                  <span className="w-16 shrink-0 text-muted">{task.state}</span>
                  <span className="w-20 shrink-0 truncate text-fg">{task.peerName}</span>
                  <span className="min-w-0 flex-1 truncate text-muted">
                    {task.promptPreview}
                  </span>
                  {task.state === 'running' && !task.approval ? (
                    <Button size="sm" variant="subtle" onClick={() => void cancel(task.id)}>
                      取消
                    </Button>
                  ) : null}
                  {task.state === 'done' ? (
                    <Button
                      size="sm"
                      variant="subtle"
                      onClick={() => {
                        setFollowUpFor(followUpFor === task.id ? null : task.id)
                        setFollowUpText('')
                      }}
                    >
                      续接
                    </Button>
                  ) : null}
                </div>
                {task.approval ? (
                  <div className="ml-16 flex flex-wrap items-center gap-2 border-l border-border pl-2">
                    <span className="text-[11px] text-muted">
                      {task.approval.risk === 'low'
                        ? `审批：${task.approval.reason}`
                        : '高风险审批：需要在远端电脑处理'}
                    </span>
                    {task.approval.risk === 'low' ? (
                      <>
                        <Button
                          size="sm"
                          disabled={busy}
                          onClick={() => void answer(task, 'allowed-once')}
                        >
                          允许一次
                        </Button>
                        <Button
                          size="sm"
                          variant="subtle"
                          disabled={busy}
                          onClick={() => void answer(task, 'rejected')}
                        >
                          拒绝
                        </Button>
                      </>
                    ) : null}
                  </div>
                ) : null}
                {followUpFor === task.id ? (
                  <div className="ml-16 flex items-center gap-2 border-l border-border pl-2">
                    <input
                      className="min-w-0 flex-1 border border-border bg-transparent px-2 py-1 text-[12px] text-fg outline-none focus-visible:ring-2 focus-visible:ring-accent/60"
                      value={followUpText}
                      placeholder="继续这个远端会话…"
                      onChange={(event) => setFollowUpText(event.target.value)}
                      onKeyDown={(event) => {
                        if (event.key === 'Enter') void sendFollowUp(task)
                      }}
                    />
                    <Button size="sm" disabled={busy} onClick={() => void sendFollowUp(task)}>
                      发送
                    </Button>
                  </div>
                ) : null}
              </div>
            ))}
          </div>
        )}
        <div className="mt-2">
          <Button size="sm" variant="subtle" onClick={() => void load()}>
            刷新
          </Button>
        </div>
      </PanelGroup>

      <PanelGroup title="DSH 集成">
        <div className="flex flex-col gap-2 text-[12px]">
          <p className="text-[11px] text-muted">
            写入 DSH profile 的 MCP 条目与 agi-mizar-remote skill；需要重启 DSH 才加载工具。
          </p>
          <div className="flex items-center gap-2">
            <Button size="sm" disabled={busy} onClick={() => void install(false)}>
              {busy ? '处理中…' : '安装 / 修复'}
            </Button>
            <Button size="sm" variant="subtle" disabled={busy} onClick={() => void install(true)}>
              卸载
            </Button>
          </div>
          {installOutput === null ? null : (
            <pre className="max-h-40 overflow-auto whitespace-pre-wrap border border-border p-2 text-[11px] text-muted">
              {installOutput}
            </pre>
          )}
        </div>
      </PanelGroup>

      <PanelGroup title="审计">
        {audit.length === 0 ? (
          <p className="text-[12px] text-muted">还没有审计记录。</p>
        ) : (
          <div className="flex max-h-56 flex-col gap-1 overflow-y-auto text-[11px]">
            {audit.slice(0, 50).map((event, index) => (
              <div key={`${event.atMs}-${index}`} className="flex gap-2">
                <span className="w-28 shrink-0 text-muted">
                  {new Date(event.atMs).toLocaleTimeString([], {
                    hour: '2-digit',
                    minute: '2-digit',
                    second: '2-digit',
                  })}
                </span>
                <span className="w-16 shrink-0 text-fg">{event.event}</span>
                <span className="w-20 shrink-0 truncate text-muted">{event.peer}</span>
                <span className="min-w-0 flex-1 truncate text-muted">
                  {event.risk
                    ? `risk ${event.risk}${event.note ? ` · ${event.note}` : ''}`
                    : event.note ?? event.route}
                </span>
              </div>
            ))}
          </div>
        )}
        <div className="mt-2">
          <Button size="sm" variant="subtle" disabled={audit.length === 0} onClick={exportAudit}>
            导出 JSON
          </Button>
        </div>
      </PanelGroup>

      {error === null ? null : (
        <p className="border border-border px-3 py-2 text-[11px] text-fg">{error}</p>
      )}
    </div>
  )
}

function Row({
  label,
  hint,
  checked,
  onChange,
}: {
  label: string
  hint: string
  checked: boolean
  onChange: (checked: boolean) => void
}) {
  return (
    <div className="flex items-start justify-between gap-4">
      <div className="min-w-0">
        <div className="text-[12px] text-fg">{label}</div>
        <div className="text-[11px] text-muted">{hint}</div>
      </div>
      <Switch checked={checked} onCheckedChange={onChange} label={label} />
    </div>
  )
}
