/**
 * The window's appearance.
 *
 * `system` is the default and the only one that reacts to macOS changing while
 * the app is open; the two explicit choices exist because a person can prefer
 * one at night and the system may be set to the other.
 */

export type ThemePreference = 'system' | 'light' | 'dark' | 'oled'

/** The appearances a person can pick, in the order the picker lists them. */
export const THEMES: { value: ThemePreference; label: string }[] = [
  { value: 'system', label: '跟随系统' },
  { value: 'light', label: '浅色' },
  { value: 'dark', label: '深色' },
  { value: 'oled', label: '1-Bit OLED' },
]

const STORAGE_KEY = 'agi-mizar.theme'
const DARK_QUERY = '(prefers-color-scheme: dark)'

/** What the preference resolves to on this machine right now.
 *
 * `oled` is drawn in the dark 1-bit palette but is not "dark mode": it has its
 * own colours, so it answers here only to tell the window which system colours
 * to use for native controls. */
export function resolvedTheme(preference: ThemePreference): 'light' | 'dark' {
  if (preference === 'oled') return 'dark'
  if (preference !== 'system') return preference
  return window.matchMedia(DARK_QUERY).matches ? 'dark' : 'light'
}

/** Whether the person asked the system to keep motion down. */
function motionAllowed(): boolean {
  return !window.matchMedia('(prefers-reduced-motion: reduce)').matches
}

/**
 * Paint the window for this preference.
 *
 * With `animate`, the change is a cross-fade: the old appearance is snapshotted
 * and dissolved into the new one. Two reasons for that over transitional CSS —
 * the font and the corner radius cannot be tweened at all, and a theme swap that
 * animates only the colours looks like the window is being repainted in the
 * wrong order.
 */
export function applyTheme(preference: ThemePreference, { animate = false } = {}): void {
  const paint = () => {
    const root = document.documentElement
    root.classList.toggle('dark', resolvedTheme(preference) === 'dark')
    // The 1-bit styles are an attribute rather than a class so they can sit beside
    // `.dark` instead of replacing it: the theme still needs a light/dark answer.
    if (preference === 'oled') root.dataset.theme = 'oled'
    else delete root.dataset.theme
    root.style.colorScheme = resolvedTheme(preference)
  }

  if (!animate || !motionAllowed()) {
    paint()
    return
  }

  // View Transitions dissolve the old snapshot into the new state, which is the
  // only mechanism here that can move between two different typefaces.
  const withTransition = document as Document & {
    startViewTransition?: (update: () => void) => { finished: Promise<void> }
  }
  if (typeof withTransition.startViewTransition === 'function') {
    withTransition.startViewTransition(paint)
    return
  }

  // Older WebKit: fade the colours, and accept that the font snaps. The class is
  // removed afterwards so it never slows down hover or focus transitions.
  document.documentElement.dataset.themeFade = ''
  paint()
  window.setTimeout(() => delete document.documentElement.dataset.themeFade, 220)
}

/** Read the stored preference; `system` when nothing was chosen. */
export function storedTheme(): ThemePreference {
  const known = (value: string | null): ThemePreference | null =>
    value === 'light' || value === 'dark' || value === 'oled' ? value : null
  const stored = known(localStorage.getItem(STORAGE_KEY))
  if (stored) return stored
  // The kernel appends what it remembered to the window's URL.
  return known(new URLSearchParams(window.location.search).get('theme')) ?? 'system'
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
