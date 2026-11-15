import { useEffect, useState } from "react"
import { DEFAULT_BOUNDS, boundsKey, isDefaultBounds } from "../link"
import { formatBands, parseBounds, type Bounds } from "../bounds"
import { formatUtcText } from "../date"

export const ALL_BOUNDS = DEFAULT_BOUNDS
const RELEASES = DEFAULT_BOUNDS.releases

export function BoundsCard(props: { bounds: Bounds; releases: string[]; boundsError?: string; onApply: (bounds: Bounds) => void }) {
  const appliedKey = boundsKey(props.bounds)
  const [draft, setDraft] = useState(() => draftOf(props.bounds))
  const [error, setError] = useState<string>()
  const [open, setOpen] = useState(() => !isDefaultBounds(props.bounds) || !!props.boundsError)
  // Synchronize by canonical value, not object identity, so an unrelated render
  // cannot erase what the user is typing. A genuinely different applied value
  // still replaces the draft and restores the appropriate fold default.
  useEffect(() => {
    setDraft(draftOf(props.bounds))
    setError(undefined)
    setOpen(!isDefaultBounds(props.bounds) || !!props.boundsError)
  }, [appliedKey])
  useEffect(() => {
    if (props.boundsError) setOpen(true)
  }, [props.boundsError])
  return <details className="specific-section" open={open} onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>Snippet filters</summary>
    <p className="specific-note">Time (UTC): 2026-06-01 or 1 Jun 2026, 14:30 UTC. DR1, QR2 and QR3 are NASA data releases; bands are wavelength channels. Blank = no limit.</p>
    <form onSubmit={(event) => {
      event.preventDefault()
      try {
        const bounds = parseBounds(draft)
        props.onApply(bounds)
        setDraft({ ...draft, bands: formatBands(bounds.bands) })
        setError(undefined)
      } catch (cause) {
        setError(cause instanceof Error ? cause.message : "Invalid filters.")
        setOpen(true)
      }
    }}>
      <label className="specific-time-bound"><span>Start</span><input aria-label="Start UTC" type="text" inputMode="text" placeholder="e.g. 2026-06-01" autoComplete="off" spellCheck={false} value={draft.fromUtc} onChange={(event) => setDraft({ ...draft, fromUtc: event.target.value })} /></label>
      <label className="specific-time-bound"><span>Finish</span><input aria-label="Finish UTC" type="text" inputMode="text" placeholder="e.g. 2026-06-30" autoComplete="off" spellCheck={false} value={draft.toUtc} onChange={(event) => setDraft({ ...draft, toUtc: event.target.value })} /></label>
      <fieldset className="specific-releases">
        <legend>Releases</legend>
        {RELEASES.filter((release) => props.releases.length === 0 || props.releases.includes(release)).map((release) => (
          <label key={release} className="specific-inline">
            <input
              type="checkbox"
              checked={draft.releases.includes(release)}
              onChange={(event) =>
                setDraft({
                  ...draft,
                  releases: event.target.checked
                    ? RELEASES.filter((id) => id === release || draft.releases.includes(id))
                    : draft.releases.filter((id) => id !== release),
                })
              }
            />
            {release.toUpperCase()}
          </label>
        ))}
      </fieldset>
      <label title="Enter band numbers or inclusive ranges separated by semicolons, e.g. 1-8;44.">Bands (1-102) · e.g. 1-8;44<input value={draft.bands} placeholder="1-8;44" onChange={(event) => setDraft({ ...draft, bands: event.target.value })} aria-invalid={!!error} /></label>
      <div className="specific-actions"><button type="submit">Apply to snippet list</button><button type="button" onClick={() => {
        props.onApply(ALL_BOUNDS)
        setDraft({ fromUtc: "", toUtc: "", releases: ALL_BOUNDS.releases, bands: "1-102" })
        setError(undefined)
      }}>Reset to all</button></div>
      {error && <p role="alert">{error}</p>}
    </form>
  </details>
}

function draftOf(bounds: Bounds) {
  return {
    fromUtc: bounds.fromUtc ? formatUtcText(bounds.fromUtc) : "",
    toUtc: bounds.toUtc ? formatUtcText(bounds.toUtc) : "",
    releases: bounds.releases,
    bands: formatBands(bounds.bands),
  }
}
