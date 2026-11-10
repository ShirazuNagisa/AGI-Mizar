import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { applyPlatformFromLocation, isWindows } from './platform'
import './styles.css'

// Before the first render: macOS and Windows reserve a strip at the top of the
// window for the title bar the page draws (Linux keeps the system one), and the
// window should not flash the other layout first.
applyPlatformFromLocation()
tameWebViewOnWindows()

/**
 * Take the browser out of Windows' web view.
 *
 * WebView2 is Chromium, and by default it behaves like the browser it is: a
 * right-click opens its menu (刷新 / 打印 / 另存为 / 检查), and F5 or Ctrl+R
 * reloads the page. WKWebView has neither, which is why macOS never had to deal
 * with them. A reload is not cosmetic here — the guide's current step and its
 * unsaved textareas are component state, so F5 mid-onboarding drops back to
 * step 1 — and the menu's "另存为"/"检查" belong to a browser, not to a desktop
 * app whose window is the product.
 *
 * Text fields keep their menu: right-click paste is a desktop affordance people
 * use, and Chromium's field menu is the native one. The development build keeps
 * the keys, because reloading and devtools are how the page is worked on.
 */
function tameWebViewOnWindows(): void {
  if (!isWindows()) return
  window.addEventListener('contextmenu', (event) => {
    const target = event.target
    const editable =
      target instanceof Element && target.closest('input, textarea, [contenteditable="true"]')
    if (editable === null) event.preventDefault()
  })
  if (import.meta.env.DEV) return
  window.addEventListener(
    'keydown',
    (event) => {
      const key = event.key.toLowerCase()
      const browserShortcut =
        event.key === 'F5' ||
        event.key === 'F7' ||
        event.key === 'F12' ||
        (event.ctrlKey && !event.altKey && !event.metaKey &&
          (['r', 'p', 'f', 'u'].includes(key) || (event.shiftKey && key === 'i')))
      if (!browserShortcut) return
      event.preventDefault()
      event.stopPropagation()
    },
    true,
  )
}

const root = document.getElementById('root')
if (root === null) throw new Error('the page has no root element')

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
