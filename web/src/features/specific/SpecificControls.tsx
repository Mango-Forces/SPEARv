import { useEffect, useId, useState, type KeyboardEvent } from "react"
import { formatBands, parseBands, parseUtc, type Bounds } from "./bounds"
import { DETECTORS, detectorBands } from "./frames"
import "../mosaic/raw-display.css"
import "./specific.css"

const ALL_BANDS = "1-102"

/** Which data goes into each epoch: the bands stacked per frame and the UTC
 * window the visits are drawn from. Applying either asks for Find again. */
export function EpochBandsControl(props: { bounds: Bounds; onApply(next: Bounds): void }) {
  const id = useId()
  const applied = formatBands(props.bounds.bands)
  const [draft, setDraft] = useState(applied)
  const [error, setError] = useState("")
  useEffect(() => {
    setDraft(applied)
    setError("")
  }, [applied])
  const apply = (expression: string) => {
    try {
      props.onApply({ ...props.bounds, bands: parseBands(expression) })
      setError("")
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Enter valid bands.")
    }
  }
  return (
    <fieldset className="raw-display raw-range epoch-fieldset">
      <legend>Bands per epoch</legend>
      <div className="epoch-presets" role="group" aria-label="Band presets">
        <button type="button" aria-pressed={applied === ALL_BANDS} onClick={() => apply(ALL_BANDS)}>
          All
        </button>
        {DETECTORS.map((detector) => {
          const bands = formatBands(detectorBands(detector))
          return (
            <button
              key={detector}
              type="button"
              aria-pressed={applied === bands}
              title={`Detector ${detector}: bands ${bands}`}
              onClick={() => apply(bands)}
            >
              D{detector}
            </button>
          )
        })}
      </div>
      <label className="raw-band-expression" htmlFor={id}>
        Bands
        <input
          id={id}
          type="text"
          autoComplete="off"
          spellCheck={false}
          placeholder="1-102 or 1-8;44"
          value={draft}
          aria-invalid={Boolean(error)}
          aria-describedby={`${id}-hint${error ? ` ${id}-error` : ""}`}
          onChange={(event) => {
            setDraft(event.target.value)
            setError("")
          }}
          onKeyDown={(event) => {
            if (event.key !== "Enter") return
            event.preventDefault()
            apply(draft)
          }}
        />
      </label>
      {error ? (
        <p id={`${id}-error`} role="alert" className="raw-error">
          {error}
        </p>
      ) : null}
      <div className="raw-actions">
        <button type="button" onClick={() => apply(draft)}>
          Apply
        </button>
      </div>
      <p id={`${id}-hint`} className="raw-note">
        Every epoch stacks these bands over one visit. All bands gives the deepest frames; D1 to D6 are the six detector
        arrays, short to long wavelength. Press Find after a change.
      </p>
    </fieldset>
  )
}

export function EpochDatesControl(props: { bounds: Bounds; onApply(next: Bounds): void }) {
  const id = useId()
  const appliedFrom = props.bounds.fromUtc?.slice(0, 10) ?? ""
  const appliedTo = props.bounds.toUtc?.slice(0, 10) ?? ""
  const [from, setFrom] = useState(appliedFrom)
  const [to, setTo] = useState(appliedTo)
  const [error, setError] = useState("")
  useEffect(() => {
    setFrom(appliedFrom)
    setTo(appliedTo)
    setError("")
  }, [appliedFrom, appliedTo])
  const apply = (nextFrom: string, nextTo: string) => {
    try {
      const fromUtc = parseUtc(nextFrom)
      const toUtc = parseUtc(nextTo, "end")
      if (fromUtc && toUtc && fromUtc > toUtc) throw new Error("The start must not be after the end.")
      props.onApply({ ...props.bounds, fromUtc, toUtc })
      setError("")
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : "Enter valid UTC dates.")
    }
  }
  const onEnter = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key !== "Enter") return
    event.preventDefault()
    apply(from, to)
  }
  return (
    <fieldset className="raw-display raw-range epoch-fieldset">
      <legend>Date range</legend>
      <div className="epoch-dates">
        <label className="raw-band-expression" htmlFor={`${id}-from`}>
          From (UTC)
          <input id={`${id}-from`} type="text" autoComplete="off" spellCheck={false} placeholder="2025-05-01" value={from} aria-invalid={Boolean(error)} onChange={(event) => setFrom(event.target.value)} onKeyDown={onEnter} />
        </label>
        <label className="raw-band-expression" htmlFor={`${id}-to`}>
          To (UTC)
          <input id={`${id}-to`} type="text" autoComplete="off" spellCheck={false} placeholder="open" value={to} aria-invalid={Boolean(error)} onChange={(event) => setTo(event.target.value)} onKeyDown={onEnter} />
        </label>
      </div>
      {error ? (
        <p role="alert" className="raw-error">
          {error}
        </p>
      ) : null}
      <div className="raw-actions">
        <button type="button" onClick={() => apply(from, to)}>
          Apply
        </button>
        <button type="button" disabled={!appliedFrom && !appliedTo && !from && !to} onClick={() => apply("", "")}>
          All dates
        </button>
      </div>
      <p className="raw-note">Only visits that overlap this window become epochs. Leave both empty for the whole release.</p>
    </fieldset>
  )
}
