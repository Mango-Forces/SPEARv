import { useEffect, useId, useState } from "react"
import { formatBands, parseBands } from "../specific/bounds"
import { rangeCommit, type CompositeRangeCommit } from "./range"
import "./raw-display.css"

export type CompositeRangeApplied = { kind: "all" } | { kind: "range"; bands: number[] }

export function CompositeRangeControl(props: {
  applied: CompositeRangeApplied
  disabled?: boolean
  onApply(next: CompositeRangeCommit): void
}) {
  const id = useId()
  const applied = props.applied.kind === "all" ? "1-102" : formatBands(props.applied.bands)
  const [draft, setDraft] = useState(applied)
  const [error, setError] = useState("")
  const reset = () => {
    setDraft(applied)
    setError("")
  }
  useEffect(reset, [applied])
  const applyDraft = () => {
    try {
      const next = rangeCommit(draft)
      setDraft(next.mode === "composite" ? "1-102" : formatBands(parseBands(draft)))
      setError("")
      props.onApply(next)
    } catch (err) {
      setError(err instanceof Error ? err.message : "Enter valid bands.")
    }
  }
  return (
    <fieldset className="raw-display raw-range" disabled={props.disabled}>
      <legend>Bands · server calculation</legend>
      <label className="raw-band-expression" htmlFor={id}>
        Bands
        <input
          id={id}
          type="text"
          inputMode="text"
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
            applyDraft()
          }}
        />
      </label>
      {error ? (
        <p id={`${id}-error`} role="alert" className="raw-error">
          {error}
        </p>
      ) : null}
      <div className="raw-actions">
        <button type="button" onClick={applyDraft}>
          Apply
        </button>
        <button type="button" onClick={reset}>
          Cancel
        </button>
      </div>
      <p id={`${id}-hint`} className="raw-note">
        {props.applied.kind === "all"
          ? "All bands (1-102). Clear or enter all / 1-102 for no filter."
          : `Bands ${formatBands(props.applied.bands)}.`}{" "}
        Apply starts a new mosaic.
      </p>
    </fieldset>
  )
}

export default CompositeRangeControl
