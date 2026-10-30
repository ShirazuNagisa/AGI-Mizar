import { useCallback, useEffect, useState } from 'react'
import {
  shellAvailable,
  shellFailure,
  shellSettingsGet,
  shellSettingsSet,
  type ShellSettings,
} from '../shell'

export interface ShellSettingsState {
  /** False when this page is not running inside the desktop shell. */
  available: boolean
  settings: ShellSettings | null
  loading: boolean
  /** The switch currently being written, so it can show it is working. */
  busy: keyof ShellSettings | null
  error: string | null
  set: (field: keyof ShellSettings, value: boolean) => void
}

/**
 * The three switches the shell owns: read once, written field by field.
 *
 * A write is optimistic — the switch moves at once and moves back if the shell
 * refuses — because a checkbox that waits on a round-trip feels broken even
 * when it is only 20ms.
 */
export function useShellSettings(): ShellSettingsState {
  const available = shellAvailable()
  const [settings, setSettings] = useState<ShellSettings | null>(null)
  const [loading, setLoading] = useState(available)
  const [busy, setBusy] = useState<keyof ShellSettings | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    if (!available) return
    let cancelled = false
    setLoading(true)
    shellSettingsGet()
      .then((value) => {
        if (!cancelled) setSettings(value)
      })
      .catch((failure: unknown) => {
        if (!cancelled) setError(shellFailure(failure))
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [available])

  const set = useCallback(
    (field: keyof ShellSettings, value: boolean) => {
      if (settings === null) return
      const previous = settings
      setSettings({ ...settings, [field]: value })
      setBusy(field)
      setError(null)
      void shellSettingsSet({ [field]: value })
        .then((next) => {
          setSettings(next)
        })
        .catch((failure: unknown) => {
          setSettings(previous)
          setError(shellFailure(failure))
        })
        .finally(() => {
          setBusy(null)
        })
    },
    [settings],
  )

  return { available, settings, loading, busy, error, set }
}
