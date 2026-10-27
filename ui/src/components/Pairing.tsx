import { useCallback, useEffect, useState } from 'react'
import type { PairingView } from '../api'

/**
 * The one screen a phone needs.
 *
 * The QR is drawn by the kernel and handed over as SVG, so the window needs no
 * QR library and no canvas: the thing on screen is the same string the code is
 * printed from, produced once by the side that knows the address.
 *
 * The address list is shown under it because a QR is not always how this
 * happens — somebody reading a code to a person in another room needs to know
 * which address to type.
 */
export function Pairing(props: {
  onClose: () => void
  request: (refresh: boolean) => Promise<PairingView>
}) {
  const [view, setView] = useState<PairingView | null>(null)
  const [error, setError] = useState<string | null>(null)

  const load = useCallback(
    (refresh: boolean) => {
      props
        .request(refresh)
        .then((next) => {
          setView(next)
          setError(null)
        })
        .catch((cause: Error) => setError(cause.message))
    },
    [props],
  )

  useEffect(() => {
    load(false)
  }, [load])

  return (
    <div className="scrim" onClick={props.onClose}>
      <div className="pair-card" onClick={(event) => event.stopPropagation()}>
        <h2>Pair a phone</h2>
        {error !== null ? <span className="notice">{error}</span> : null}
        {view !== null ? (
          <>
            {view.svg !== null ? (
              // Our own SVG, built by the kernel from the URI below.
              <div className="qr" dangerouslySetInnerHTML={{ __html: view.svg }} />
            ) : null}
            <span className="code">{view.code}</span>
            <div className="addresses">
              {view.addresses.map((address) => (
                <span key={address}>{address}</span>
              ))}
            </div>
            <span className="hint">Valid for five minutes, once.</span>
            <div style={{ display: 'flex', gap: 8 }}>
              <button className="btn" onClick={() => load(true)}>
                New code
              </button>
              <button className="btn primary" onClick={props.onClose}>
                Done
              </button>
            </div>
          </>
        ) : (
          <span className="hint">{error === null ? 'Preparing…' : ''}</span>
        )}
      </div>
    </div>
  )
}
