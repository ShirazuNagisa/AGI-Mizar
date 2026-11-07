/**
 * Which desktop this page is running on.
 *
 * The kernel answers it in `StateSnapshot.platform`, and the shell also puts it
 * in the window's URL, because the first paint needs it before the snapshot has
 * been fetched: macOS and Windows both give the page the top strip of the
 * window (macOS overlays its traffic lights there, Windows has no system bar at
 * all and the page paints them); Linux keeps its system title bar.
 *
 * The value is painted on `<html data-platform>` so CSS can branch too, and so a
 * component that has no snapshot (the toolbar) does not have to drill a prop.
 */

export type DesktopPlatform = 'macos' | 'windows' | 'linux'

/** `macos`/`windows`/`linux`, or null for anything that is not one of them. */
function known(value: string | null | undefined): DesktopPlatform | null {
  if (value === 'macos' || value === 'windows' || value === 'linux') return value
  return null
}

/** The platform as the shell spelled it in the window's URL, if it did. */
function fromLocation(): DesktopPlatform | null {
  return known(new URLSearchParams(window.location.search).get('platform'))
}

/** The platform guessed from the browser, for a page opened outside the shell. */
function fromBrowser(): DesktopPlatform {
  const agent = navigator.userAgent
  if (agent.includes('Windows')) return 'windows'
  if (agent.includes('Macintosh') || agent.includes('Mac OS X')) return 'macos'
  return 'linux'
}

/**
 * The platform this page should lay itself out for.
 *
 * The attribute wins: it is the answer the shell already gave, either in the URL
 * or in the snapshot, and it is what CSS reads. The browser is only the fallback
 * for a page opened outside the desktop shell.
 */
export function desktopPlatform(): DesktopPlatform {
  return known(document.documentElement.dataset.platform) ?? fromLocation() ?? fromBrowser()
}

/** Whether this is a Mac, the one desktop with the traffic-light overlay. */
export function isMac(): boolean {
  return desktopPlatform() === 'macos'
}

/**
 * Whether this is Windows, where the page draws the title bar itself.
 *
 * Windows 11 removed nothing: the native caption is disabled and the page
 * paints the same strip mac gets from its overlay, including the traffic
 * lights; the buttons call the window plugin's minimize/maximize/close.
 */
export function isWindows(): boolean {
  return desktopPlatform() === 'windows'
}

/** Paint a platform on `<html>`, so CSS and components can branch without props. */
export function applyPlatform(platform: string | null | undefined): void {
  const next = known(platform)
  if (next !== null) document.documentElement.dataset.platform = next
}

/** Apply the platform the shell put in the URL; the pre-render entry calls this. */
export function applyPlatformFromLocation(): void {
  applyPlatform(fromLocation() ?? fromBrowser())
}

/** Turn a snapshot's `platform` string into the closed set this module uses. */
export function platformOf(platform: string | null | undefined): DesktopPlatform {
  return known(platform) ?? desktopPlatform()
}
