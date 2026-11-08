import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { App } from './App'
import { applyPlatformFromLocation } from './platform'
import './styles.css'

// Before the first render: macOS and Windows reserve a strip at the top of the
// window for the title bar the page draws (Linux keeps the system one), and the
// window should not flash the other layout first.
applyPlatformFromLocation()

const root = document.getElementById('root')
if (root === null) throw new Error('the page has no root element')

createRoot(root).render(
  <StrictMode>
    <App />
  </StrictMode>,
)
