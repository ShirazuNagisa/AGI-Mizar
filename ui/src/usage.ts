/**
 * The dashboard's arithmetic.
 *
 * The kernel sends every day that had activity and lets this side decide what a
 * range means, so switching 全部/30 天/7 天 is instant and cannot disagree with
 * the heatmap it sits above.
 */

import type { ModelUsage, UsageDay, UsageOverview } from './api'

export type Range = 'all' | '30d' | '7d'

export const RANGES: { id: Range; label: string }[] = [
  { id: 'all', label: '全部' },
  { id: '30d', label: '30 天' },
  { id: '7d', label: '7 天' },
]

/** Local calendar day as `YYYY-MM-DD`. */
export function today(): string {
  const now = new Date()
  const month = `${now.getMonth() + 1}`.padStart(2, '0')
  const day = `${now.getDate()}`.padStart(2, '0')
  return `${now.getFullYear()}-${month}-${day}`
}

/** The first day a range covers, or `null` for all time. */
export function rangeStart(range: Range): string | null {
  if (range === 'all') return null
  const days = range === '30d' ? 30 : 7
  const start = new Date()
  start.setDate(start.getDate() - (days - 1))
  const month = `${start.getMonth() + 1}`.padStart(2, '0')
  const day = `${start.getDate()}`.padStart(2, '0')
  return `${start.getFullYear()}-${month}-${day}`
}

/** The days a range covers, oldest first. */
export function daysInRange(overview: UsageOverview | null, range: Range): UsageDay[] {
  if (overview === null) return []
  const start = rangeStart(range)
  return start === null ? overview.days : overview.days.filter((day) => day.date >= start)
}

export interface Summary {
  messages: number
  inputTokens: number
  outputTokens: number
  tokens: number
  activeDays: number
  /** 0–23, or `null` when nothing has happened. */
  peakHour: number | null
  models: ModelUsage[]
  /** The model with the most messages, or `null`. */
  topModel: string | null
}

/** Fold a range of days into the six numbers the overview shows. */
export function summarize(days: UsageDay[]): Summary {
  const hours = new Array<number>(24).fill(0)
  const models = new Map<string, ModelUsage>()
  let messages = 0
  let inputTokens = 0
  let outputTokens = 0
  let activeDays = 0

  for (const day of days) {
    messages += day.messages
    inputTokens += day.inputTokens
    outputTokens += day.outputTokens
    if (day.messages > 0) activeDays += 1
    for (let hour = 0; hour < 24; hour += 1) {
      hours[hour] = (hours[hour] ?? 0) + (day.hours[hour] ?? 0)
    }
    for (const model of day.models) {
      const existing = models.get(model.model) ?? {
        model: model.model,
        messages: 0,
        inputTokens: 0,
        outputTokens: 0,
      }
      existing.messages += model.messages
      existing.inputTokens += model.inputTokens
      existing.outputTokens += model.outputTokens
      models.set(model.model, existing)
    }
  }

  let peakHour: number | null = null
  for (let hour = 0; hour < 24; hour += 1) {
    const count = hours[hour] ?? 0
    if (count === 0) continue
    if (peakHour === null || count > (hours[peakHour] ?? 0)) peakHour = hour
  }

  const ranked = [...models.values()].sort((a, b) => b.messages - a.messages)
  return {
    messages,
    inputTokens,
    outputTokens,
    tokens: inputTokens + outputTokens,
    activeDays,
    peakHour,
    models: ranked,
    topModel: ranked[0]?.model ?? null,
  }
}

/** The 365-day window the heatmap draws, oldest first. */
export function heatmapWindow(days: UsageDay[], end = today()): UsageDay[] {
  const byDate = new Map(days.map((day) => [day.date, day]))
  const window: UsageDay[] = []
  const cursor = new Date(`${end}T00:00:00`)
  cursor.setDate(cursor.getDate() - 364)
  for (let index = 0; index < 365; index += 1) {
    const month = `${cursor.getMonth() + 1}`.padStart(2, '0')
    const day = `${cursor.getDate()}`.padStart(2, '0')
    const date = `${cursor.getFullYear()}-${month}-${day}`
    window.push(
      byDate.get(date) ?? {
        date,
        messages: 0,
        inputTokens: 0,
        outputTokens: 0,
        hours: new Array<number>(24).fill(0),
        models: [],
      },
    )
    cursor.setDate(cursor.getDate() + 1)
  }
  return window
}

/** `12 点` for a 0–23 hour. */
export function hourLabel(hour: number | null): string {
  return hour === null ? '—' : `${hour} 点`
}
