import type { Dispatch } from "react"
import { formatBands, type Bounds } from "../bounds"
import { formatUtcClock, formatUtcDate } from "../date"
import { sectorMembers, type SectorAction, type SectorState, type Snippet } from "./state"

export function outsideBounds(snippet: Snippet, bounds: Bounds): boolean {
  const time = new Date(snippet.utc).getTime()
  return !bounds.bands.includes(snippet.band) || !bounds.releases.includes(snippet.release.replace(/_deep$/, "")) ||
    (!!bounds.fromUtc && time < new Date(bounds.fromUtc).getTime()) || (!!bounds.toUtc && time > new Date(bounds.toUtc).getTime())
}

export function outsideCurrentSearch(snippet: Snippet, bounds: Bounds, search: string): boolean {
  if (outsideBounds(snippet, bounds)) return true
  const text = asciiLower(`band ${snippet.band} ${snippet.utc} ${snippet.release} d${snippet.detector} ${snippet.obsid} ${snippet.version}`)
  return search.trim().split(/\s+/).filter(Boolean).some((term) => {
    const normalized = asciiLower(term)
    if (/^\d+$/.test(normalized)) return Number(normalized) !== snippet.band
    return !text.includes(normalized)
  })
}

export function sectorSummary(members: Snippet[]): string {
  if (members.length === 0) return "Empty"
  const times = members.map((snippet) => snippet.utc).sort()
  const parts = [
    `${members.length} snippet${members.length === 1 ? "" : "s"}`,
    `bands ${formatBands(members.map((snippet) => snippet.band))}`,
    [...new Set(members.map((snippet) => snippet.release.toUpperCase()))].join(" / "),
    formatUtcDate(times[0]),
  ]
  if (times.at(-1) !== times[0]) parts.push(`→ ${formatUtcDate(times.at(-1)!)}`)
  return parts.join(" · ")
}

export function SectorsEditor(props: { state: SectorState; bounds: Bounds; search: string; dispatch: Dispatch<SectorAction>; buildStates: Map<string, string>; onPreview: (snippet: Snippet) => void; onPreviewSector: (sectorId: string) => void }) {
  const active = props.state.activeSectorId && props.state.sectors.get(props.state.activeSectorId)
  const members = active ? sectorMembers(props.state, active.id) : []
  return <details className="specific-section" open>
    <summary>Sectors <span>Merge snippets, then compare sectors</span></summary>
    <button type="button" onClick={() => props.dispatch({ type: "create" })}>New sector</button>
    <div className="sector-nodes" role="group" aria-label="Assignment destination">
      {props.state.order.map((id) => {
        const sector = props.state.sectors.get(id)!
        const selected = id === props.state.activeSectorId
        const items = sectorMembers(props.state, id)
        return <button key={id} type="button" className="sector-node" aria-pressed={selected} onClick={() => props.dispatch({ type: "select", sectorId: id })}>
          <strong>{sector.name}{selected ? " · Active" : ""}</strong><span className="sector-meta">{sectorSummary(items).split(" · ").map((part) => <span key={part}>{part}</span>)}</span>{items.length > 0 && <span>{props.buildStates.get(id) ?? "Unbuilt"}</span>}
        </button>
      })}
    </div>
    {!active && <p>Merge snippets into a sector, then compare sectors.</p>}
    {active && <div className="sector-members" aria-label={`${active.name} members`}>
      <h2>{active.name} members</h2>
      {members.length === 0 && <p>No snippets assigned.</p>}
      <ul>{members.map((snippet) => <li key={snippet.id}>
        <span className="member-meta">
          <span>Band {snippet.band} · {snippet.release.toUpperCase()} / D{snippet.detector}</span>
          {outsideCurrentSearch(snippet, props.bounds, props.search) && <span className="member-outside">outside current search</span>}
          <time dateTime={snippet.utc} title={snippet.utc}><span>{formatUtcDate(snippet.utc)}</span><span>{formatUtcClock(snippet.utc)}</span></time>
        </span>
        <button type="button" aria-label={`Preview member band ${snippet.band} at ${snippet.utc}`} onClick={() => props.onPreview(snippet)}>Preview</button>
        <button type="button" aria-label={`Remove band ${snippet.band} at ${snippet.utc} from ${active.name}`} onClick={() => props.dispatch({ type: "remove", snippetId: snippet.id })}>Remove</button>
      </li>)}</ul>
      <div className="specific-actions"><button type="button" disabled={members.length === 0} onClick={() => props.onPreviewSector(active.id)}>Preview sector</button><button type="button" onClick={() => props.dispatch({ type: "delete", sectorId: active.id })}>Delete {active.name}</button></div>
      <p className="specific-note">Preview builds this sector alone on its own brightness scale. Playback uses one shared scale. Different band sets are different spectral mixtures; merging does not guarantee a cleaner measurement of the same quantity.</p>
    </div>}
  </details>
}

function asciiLower(value: string): string {
  return value.replace(/[A-Z]/g, (letter) => String.fromCharCode(letter.charCodeAt(0) + 32))
}
