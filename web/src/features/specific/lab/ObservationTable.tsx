import { Fragment, useState, type Dispatch } from "react"
import { formatUtcClock, formatUtcDate } from "../date"
import { qualityFlags, type ExposureQuality } from "../frames"
import type { SnippetSort } from "./useObservations"
import type { SectorAction, SectorState, Snippet } from "./state"

export function ObservationTable(props: {
  state: SectorState; rows: Snippet[]; matches: number; pending: number; failed: number; discoveryState: string; loading: boolean; error?: string
  sampled: boolean; sampledFrom: number; elapsedMs: number; cancel: () => void; cancelled: boolean; pageSize: number
  search: string; onSearch: (search: string) => void; sort: SnippetSort; direction: "asc" | "desc"
  onSort: (sort: SnippetSort) => void; offset: number; onPage: (offset: number) => void; dispatch: Dispatch<SectorAction>
  onPreview: (snippet: Snippet) => void
  /** `pass` hides rows whose header quality raises any flag. */
  quality: "any" | "pass"; onQuality: (quality: "any" | "pass") => void
  /** Snippet ids picked for a side-by-side compare, in pick order. */
  picked: string[]; onTogglePick: (snippet: Snippet) => void; onCompare: () => void; onClearPicks: () => void; comparing: boolean
}) {
  const active = props.state.activeSectorId && props.state.sectors.get(props.state.activeSectorId)
  const [infoId, setInfoId] = useState<string>()
  const complete = props.discoveryState === "done" && props.pending === 0 && props.failed === 0 && !props.sampled
  const elapsed = Math.round(props.elapsedMs / 1000)
  const listing = props.loading ? `Listing ${elapsed}s · ${props.matches.toLocaleString()} found${props.pending ? ` · ${props.pending.toLocaleString()} pending` : ""}. Rows below are usable now.`
    : props.discoveryState === "done" ? `${props.sampled ? "Quick sample" : "Complete"} in ${elapsed}s · ${props.matches.toLocaleString()} ${props.sampled ? "found" : "matching"}.`
    : props.cancelled ? `Listing stopped after ${elapsed}s · ${props.matches.toLocaleString()} found. Rows below are usable now.`
    : props.error ? `Listing failed after ${elapsed}s · ${props.matches.toLocaleString()} found.`
    : `Listing ${elapsed}s · ${props.matches.toLocaleString()} found.`
  return <details className="specific-section observations" open>
    <summary>Available snippets <span>{props.matches.toLocaleString()}{complete ? " matching" : " found so far"}</span></summary>
    <p className="specific-note">Filters change this list, not existing sector members.</p>
    <label>Search snippets<input type="search" value={props.search} onChange={(event) => props.onSearch(event.target.value)} placeholder="Band, UTC, release, detector, obsid…" /></label>
    <div className="discovery-status">
      <p role="status">{listing}{props.failed ? ` ${props.failed} geometry ${props.failed === 1 ? "failure" : "failures"}; listing is incomplete.` : ""}{` ${props.state.ownerBySnippet.size} assigned in workspace.`}</p>
      {props.loading && <button type="button" onClick={props.cancel}>Stop listing</button>}
    </div>
    {props.sampled && <p className="specific-note">Quick video sample · {props.sampledFrom.toLocaleString()} files available. Apply filters for the full list.</p>}
    {props.error && <p role="alert">{props.error}</p>}
    <div className="snippet-tools">
      <label className="specific-inline" title="Hide exposures with a failed quality check, aurora, airglow, spacecraft glow, satellite streaks or a poor astrometry solution">
        <input type="checkbox" checked={props.quality === "pass"} onChange={(event) => props.onQuality(event.target.checked ? "pass" : "any")} />
        Passing quality only
      </label>
      <span className="snippet-compare">
        <button type="button" disabled={props.picked.length < 2 || props.comparing} onClick={props.onCompare} title="Blink, swipe and difference the picked exposures on one shared stretch">
          {props.comparing ? "Building compare…" : `Compare ${props.picked.length || ""} picked`.replace("  ", " ")}
        </button>
        {props.picked.length > 0 && <button type="button" onClick={props.onClearPicks}>Clear</button>}
      </span>
    </div>
    {!active && <p className="specific-note">Create or select a sector to enable +. Tick Pick on two or more rows to compare single exposures.</p>}
    <div className="snippet-scroll" tabIndex={0} role="region" aria-label="Snippet table">
      <table><thead><tr><th scope="col">Join</th>
        {(["band", "time"] as const).map((sort) => <th key={sort} scope="col" aria-sort={props.sort === sort ? props.direction === "asc" ? "ascending" : "descending" : "none"}><button type="button" onClick={() => props.onSort(sort)}>{sort === "band" ? "Band" : "UTC"}{props.sort === sort ? props.direction === "asc" ? " ↑" : " ↓" : ""}</button></th>)}
        <th scope="col">Rel</th><th scope="col">Det</th>
        <th scope="col" aria-sort={props.sort === "psf" ? props.direction === "asc" ? "ascending" : "descending" : "none"}><button type="button" title="Sort by star image width (PSF FWHM)" onClick={() => props.onSort("psf")}>Quality{props.sort === "psf" ? props.direction === "asc" ? " ↑" : " ↓" : ""}</button></th>
        <th scope="col">Actions</th></tr></thead>
        <tbody>{props.rows.map((snippet) => {
          const ownerId = props.state.ownerBySnippet.get(snippet.id)
          const owner = ownerId && props.state.sectors.get(ownerId)
          const infoOpen = infoId === snippet.id
          const pickAt = props.picked.indexOf(snippet.id)
          return <Fragment key={snippet.id}>
            <tr title={`Obs ${snippet.obsid} · version ${snippet.version}`} data-picked={pickAt >= 0 || undefined}>
              <td>{owner ? <button type="button" className="owner-badge" title={owner.name} aria-label={`Select ${owner.name}`} onClick={() => props.dispatch({ type: "select", sectorId: owner.id })}>{sectorLabel(owner.id)}</button> : <button type="button" disabled={!active} aria-label={`Join band ${snippet.band} at ${snippet.utc} to ${active ? active.name : "a sector"}`} onClick={() => props.dispatch({ type: "join", snippetId: snippet.id })}>+</button>}</td>
              <td>{snippet.band}</td><td><time dateTime={snippet.utc} title={snippet.utc}><span>{formatUtcDate(snippet.utc)}</span><span>{formatUtcClock(snippet.utc).replace(/\.\d+/, "")}</span></time></td><td>{snippet.release.toUpperCase()}</td><td title={`Detector ${snippet.detector}`}>D{snippet.detector}</td>
              <td><QualityBadge quality={snippet.quality} /></td>
              <td><div className="snippet-row-actions">
                <label className="snippet-pick" title="Pick for compare"><input type="checkbox" checked={pickAt >= 0} aria-label={`Pick band ${snippet.band} at ${snippet.utc} for compare`} onChange={() => props.onTogglePick(snippet)} />{pickAt >= 0 ? String.fromCharCode(65 + pickAt) : "Pick"}</label>
                <button type="button" aria-label={`Preview band ${snippet.band} at ${snippet.utc}`} onClick={() => props.onPreview(snippet)}>View</button>
                <button type="button" className="snippet-info-toggle" aria-expanded={infoOpen} aria-controls={`snippet-info-${snippet.id}`} aria-label={`Details for obs ${snippet.obsid} band ${snippet.band}`} onClick={() => setInfoId(infoOpen ? undefined : snippet.id)}>Info</button>
              </div></td>
            </tr>
            {infoOpen && <tr className="snippet-info-row">
              <td colSpan={7}><dl id={`snippet-info-${snippet.id}`}>
                <dt>Observation ID</dt><dd>{snippet.obsid || "Unknown"}</dd>
                <dt>Version</dt><dd>{snippet.version || "Unknown"}</dd>
                <dt>Wavelength</dt><dd>{formatWavelength(snippet.wl_min_um, snippet.wl_max_um)}</dd>
                {qualityRows(snippet.quality).map((item) => <Fragment key={item.label}><dt>{item.label}</dt><dd>{item.value}</dd></Fragment>)}
              </dl></td>
            </tr>}
          </Fragment>
        })}</tbody>
      </table>
    </div>
    {!props.loading && !props.error && props.rows.length === 0 && <p>No snippets match the applied filters and search.</p>}
    <div className="specific-actions"><button type="button" disabled={props.offset === 0} onClick={() => props.onPage(Math.max(0, props.offset - props.pageSize))}>Previous page</button><span>{props.rows.length ? `${props.offset + 1}–${props.offset + props.rows.length}` : "0"} / {props.matches}</span><button type="button" disabled={props.offset + props.pageSize >= props.matches} onClick={() => props.onPage(props.offset + props.pageSize)}>Next page</button></div>
  </details>
}

/** Pass, flagged or unknown at a glance, with the star image width. */
function QualityBadge(props: { quality: unknown }) {
  const quality = typeof props.quality === "object" && props.quality !== null ? (props.quality as ExposureQuality) : null
  const flags = qualityFlags(quality)
  const passed = quality?.dqa_l1?.toLowerCase() === "pass" && quality?.dqa_l2?.toLowerCase() === "pass"
  const kind = flags.length ? "flagged" : passed ? "pass" : "unknown"
  const label = flags.length ? `${flags.length} flag${flags.length === 1 ? "" : "s"}` : passed ? "Pass" : "?"
  const width = typeof quality?.psf_fwhm === "number" ? `${quality.psf_fwhm.toFixed(1)}″` : undefined
  return <span className="quality-cell" title={flags.length ? flags.join(", ") : passed ? "Level 1 and 2 checks pass" : "Quality unknown"}>
    <span className={`quality-badge ${kind}`}>{label}</span>{width && <span className="quality-psf">{width}</span>}
  </span>
}

function formatWavelength(min: number, max: number) {
  return `${min.toFixed(3)}–${max.toFixed(3)} µm`
}

/** Stable compact badge for the internal sector id (sector-1 -> S1). */
function sectorLabel(id: string) {
  return id.replace(/^sector-/, "S")
}

/** Header quality with public labels. Null or empty is unknown, never implied clean. */
function qualityRows(quality: unknown): { label: string; value: string }[] {
  if (quality == null) return [{ label: "Quality", value: "Unknown" }]
  let value = quality
  if (typeof quality === "string") {
    if (!quality.trim()) return [{ label: "Quality", value: "Unknown" }]
    try {
      value = JSON.parse(quality)
    } catch {
      return [{ label: "Quality", value: quality }]
    }
  }
  if (value == null) return [{ label: "Quality", value: "Unknown" }]
  if (typeof value !== "object" || Array.isArray(value)) return [{ label: "Quality", value: String(value) }]
  const labels: Record<string, string> = {
    dqa_l1: "Level 1 quality",
    dqa_l2: "Level 2 quality",
    psf_fwhm: "Star image width",
    finast: "Astrometry solution",
    aurora: "Aurora contamination",
    airglow: "Airglow contamination",
    shuttle_glow: "Spacecraft glow",
    n_streaks: "Satellite streaks",
    src_frac: "Source-masked pixels",
  }
  const units: Record<string, string> = { psf_fwhm: " arcsec", src_frac: "%" }
  const decimals: Record<string, number> = { psf_fwhm: 2, src_frac: 2 }
  const rows = Object.entries(value).map(([key, item]) => {
    const shown = item == null ? "Unknown" : typeof item === "boolean" ? item ? "Yes" : "No" : key === "finast" ? (item === 0 ? "Good" : `Flagged (${String(item)})`) : `${typeof item === "number" && decimals[key] !== undefined ? item.toFixed(decimals[key]) : String(item)}${units[key] ?? ""}`
    return { label: labels[key] ?? key.replaceAll("_", " "), value: shown }
  })
  return rows.length > 0 ? rows : [{ label: "Quality", value: "Unknown" }]
}
