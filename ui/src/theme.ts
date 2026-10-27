/**
 * The window's appearance.
 *
 * `system` is the default and the only one that reacts to macOS changing while
 * the app is open; the two explicit choices exist because a person can prefer
 * one at night and the system may be set to the other.
 */

export type ThemePreference = 'system' | 'light' | 'dark'

const STORAGE_KEY = 'agi-mizar.theme'
const DARK_QUERY = '(prefers-color-scheme: dark)'

/** What the preference resolves to on this machine right now. */
export function resolvedTheme(preference: ThemePreference): 'light' | 'dark' {
  if (preference !== 'system') return preference
  return window.matchMedia(DARK_QUERY).matches ? 'dark' : 'light'
}

/** Paint the window for this preference. */
export function applyTheme(preference: ThemePreference): void {
  document.documentElement.classList.toggle('dark', resolvedTheme(preference) === 'dark')
  document.documentElement.style.colorScheme = resolvedTheme(preference)
}

/** Read the stored preference; `system` when nothing was chosen. */
export function storedTheme(): ThemePreference {
  const value = localStorage.getItem(STORAGE_KEY)
  return value === 'light' || value === 'dark' ? value : 'system'
}

/** Remember the choice. */
export function storeTheme(preference: ThemePreference): void {
  if (preference === 'system') localStorage.removeItem(STORAGE_KEY)
  else localStorage.setItem(STORAGE_KEY, preference)
}

/** Call `listener` when the system appearance changes, if it does. */
export function onSystemThemeChange(listener: () => void): () => void {
  const query = window.matchMedia(DARK_QUERY)
  query.addEventListener('change', listener)
  return () => query.removeEventListener('change', listener)
}
