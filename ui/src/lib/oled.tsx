/**
 * Which appearance the controls are being drawn in.
 *
 * The two themes do not share a shape: `1-Bit OLED` has square, outlined,
 * always-bordered controls and no shadows, and the classic window has rounded
 * ones with soft fills. A class list cannot express both, so a control asks this
 * and picks its own.
 *
 * It is context rather than a CSS attribute because the difference is in which
 * classes are emitted, not in a variable those classes read.
 */
import { createContext, useContext, type ReactNode } from 'react'

const OledContext = createContext(false)

export function OledScope({ oled, children }: { oled: boolean; children: ReactNode }) {
  return <OledContext.Provider value={oled}>{children}</OledContext.Provider>
}

/** True when the window is wearing 1-Bit OLED. */
export function useIsOled(): boolean {
  return useContext(OledContext)
}
