import { useMemo, useState } from 'react'
import type { UsageDay, UsageOverview } from '../api'
import { formatCount } from '../format'
import { hourLabel, heatmapWindow, RANGES, type Range, daysInRange, summarize } from '../usage'
import { Toolbar } from './Toolbar'
import { cn } from '../lib/utils'

interface HomePageProps {
  overview: UsageOverview | null
  loading: boolean
  error: string | null
}

type Tab = 'overview' | 'models'

const TABS: { id: Tab; label: string }[] = [
  { id: 'overview', label: '总览' },
  { id: 'models', label: '模型' },
]

/**
 * The landing page: what this computer has done, not what it is saying.
 *
 * It is the only page that is not about one agent, so its numbers are the sum
 * over every engine's own log — the phone sees the conversations, this window
 * sees the shape of them.
 */
export function HomePage({ overview, loading, error }: HomePageProps) {
  const [tab, setTab] = useState<Tab>('overview')
  const [range, setRange] = useState<Range>('all')

  const days = useMemo(() => daysInRange(overview, range), [overview, range])
  const summary = useMemo(() => summarize(days), [days])
  const heat = useMemo(() => heatmapWindow(overview?.days ?? []), [overview])

  return (
    <div className="flex h-full min-w-0 flex-1 flex-col">
      <Toolbar title="主页">
        <div className="flex items-center gap-0.5">
          {RANGES.map((entry) => (
            <button
              key={entry.id}
              type="button"
              onClick={() => setRange(entry.id)}
              aria-pressed={range === entry.id}
              className={cn(
                'h-7 rounded-md px-2.5 text-[12px] transition-colors',
                range === entry.id ? 'bg-surface-2 text-fg' : 'text-muted hover:text-fg',
              )}
            >
              {entry.label}
            </button>
          ))}
        </div>
      </Toolbar>
      <div className="flex-1 overflow-y-auto">
        <div className="mx-auto w-full max-w-[1180px] px-8 py-5">
          <div className="flex items-center gap-3">
            <div className="flex items-center gap-0.5 rounded-lg bg-surface-2/70 p-0.5">
              {TABS.map((entry) => (
                <button
                  key={entry.id}
                  type="button"
                  onClick={() => setTab(entry.id)}
                  aria-pressed={tab === entry.id}
                  className={cn(
                    'h-7 rounded-md px-3 text-[12px] transition-colors',
                    tab === entry.id
                      ? 'bg-surface text-fg shadow-sm shadow-black/10'
                      : 'text-muted hover:text-fg',
                  )}
                >
                  {entry.label}
                </button>
              ))}
            </div>
          </div>

          {error === null ? null : (
            <p className="mt-3 text-[12px] text-muted">统计暂时读不到：{error}</p>
          )}

          {tab === 'overview' ? (
            <>
              <div className="mt-4 grid grid-cols-2 gap-3 min-[1000px]:grid-cols-3">
                <Metric label="会话" value={overview === null ? '—' : formatCount(overview.sessions)} loading={loading} />
                <Metric label="消息" value={overview === null ? '—' : formatCount(summary.messages)} loading={loading} />
                <Metric label="总 token" value={overview === null ? '—' : formatCount(summary.tokens)} loading={loading} />
                <Metric
                  label="活跃天数"
                  value={overview === null ? '—' : `${summary.activeDays}`}
                  loading={loading}
                  suffix=" 天"
                />
                <Metric label="高峰时段" value={overview === null ? '—' : hourLabel(summary.peakHour)} loading={loading} />
                <Metric
                  label="常用模型"
                  value={overview === null ? '—' : (summary.topModel ?? '—')}
                  loading={loading}
                  text
                />
              </div>

              <Heatmap days={heat} />
            </>
          ) : (
            <ModelTable summary={summary} loading={loading} />
          )}
        </div>
      </div>
    </div>
  )
}

function Metric({
  label,
  value,
  suffix,
  text,
  loading,
}: {
  label: string
  value: string
  suffix?: string
  text?: boolean
  loading: boolean
}) {
  return (
    <div className="rounded-xl border border-border px-4 py-3.5">
      <div className="text-[12px] text-muted">{label}</div>
      <div
        className={cn(
          'mt-1.5 truncate font-semibold text-fg tabular',
          text ? 'text-[20px] leading-7' : 'text-[24px] leading-8',
          loading && value === '—' && 'animate-pulse text-muted/50',
        )}
      >
        {value}
        {suffix === undefined ? null : (
          <span className="ml-0.5 text-[13px] font-normal text-muted">{suffix}</span>
        )}
      </div>
    </div>
  )
}

/** The year at a glance: one square per day, shaded by how much happened. */
function Heatmap({ days }: { days: UsageDay[] }) {
  const max = Math.max(1, ...days.map((day) => day.messages))
  const columns = useMemo(() => {
    const firstDay = days[0]
    if (firstDay === undefined) return []
    const first = new Date(`${firstDay.date}T00:00:00`)
    // Monday-first weeks, so the column reads the way a calendar does here.
    const lead = (first.getDay() + 6) % 7
    const cells: (UsageDay | null)[] = [...new Array<null>(lead).fill(null), ...days]
    while (cells.length % 7 !== 0) cells.push(null)
    const weeks: (UsageDay | null)[][] = []
    for (let index = 0; index < cells.length; index += 7) {
      weeks.push(cells.slice(index, index + 7))
    }
    return weeks
  }, [days])

  return (
    <section className="mt-5">
      <div className="flex items-end justify-between">
        <h2 className="text-[12px] font-medium text-muted">过去一年</h2>
        <div className="flex items-center gap-1.5 text-[11px] text-muted">
          <span>少</span>
          {[0, 1, 2, 3, 4].map((level) => (
            <span
              key={level}
              className="size-[10px] rounded-[2px]"
              style={{ background: heatColor(level) }}
            />
          ))}
          <span>多</span>
        </div>
      </div>
      <div
        className="mt-2 flex gap-[3px] overflow-hidden pb-1"
        role="img"
        aria-label={`过去一年共 ${days.reduce((total, day) => total + day.messages, 0)} 条消息`}
      >
        {columns.map((week, index) => (
          <div key={index} className="flex flex-col gap-[3px]">
            {week.map((day, row) => (
              <span
                key={row}
                className="size-[10px] rounded-[2px]"
                style={{ background: heatColor(day === null ? 0 : level(day.messages, max)) }}
                title={
                  day === null ? undefined : `${day.date} · ${formatCount(day.messages)} 条消息`
                }
              />
            ))}
          </div>
        ))}
      </div>
    </section>
  )
}

/** 0 is "nothing happened"; 1–4 scale with the busiest day of the year. */
function level(messages: number, max: number): number {
  if (messages === 0) return 0
  return Math.max(1, Math.min(4, Math.ceil((Math.log1p(messages) / Math.log1p(max)) * 4)))
}

function heatColor(level: number): string {
  if (level === 0) return 'var(--heat-0)'
  return `color-mix(in oklab, var(--accent) ${level * 25}%, var(--heat-0))`
}

/** Models, ranked by how much of the conversation they carried. */
function ModelTable({
  summary,
  loading,
}: {
  summary: ReturnType<typeof summarize>
  loading: boolean
}) {
  const top = Math.max(1, ...summary.models.map((model) => model.messages))
  return (
    <section className="mt-4">
      <div className="grid grid-cols-[1fr_80px_100px_120px] items-center gap-3 border-b border-border px-3 pb-2 text-[11px] text-muted">
        <span>模型</span>
        <span className="text-right">消息</span>
        <span className="text-right">token</span>
        <span />
      </div>
      {summary.models.length === 0 ? (
        <p className="px-3 py-6 text-[12px] text-muted">
          {loading ? '正在读取…' : '这个时间范围内还没有可统计的会话。'}
        </p>
      ) : (
        summary.models.map((model) => (
          <div
            key={model.model}
            className="grid grid-cols-[1fr_80px_100px_120px] items-center gap-3 border-b border-border/60 px-3 py-2.5 text-[12px]"
          >
            <span className="truncate text-fg">{model.model}</span>
            <span className="text-right text-fg tabular">{formatCount(model.messages)}</span>
            <span className="text-right text-muted tabular">
              {formatCount(model.inputTokens + model.outputTokens)}
            </span>
            <span className="h-1.5 overflow-hidden rounded-full bg-surface-2">
              <span
                className="block h-full rounded-full bg-accent"
                style={{ width: `${Math.max(2, (model.messages / top) * 100)}%` }}
              />
            </span>
          </div>
        ))
      )}
    </section>
  )
}
