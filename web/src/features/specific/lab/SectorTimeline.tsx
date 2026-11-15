import type { Dispatch } from "react"
import type { SectorAction, SectorState } from "./state"

export type TransportState = "idle" | "building" | "decoding" | "ready" | "stale" | "failed"

export type Transport = {
  prepareBlocked?: string
  /** Explicit server-building, prepared-decoding, ready, stale or failed state. */
  state: TransportState
  progress?: string
  error?: string
  failed: string[]
  /** The requested frame whose tiles failed, when the state is failed. */
  failedFrame?: number
  ready: boolean
  stepEnabled: boolean
  playing: boolean
  /** The prepared sequence is the surface currently on screen. */
  showing: boolean
  current: number
  rate: number
  loop: boolean
  buffering?: number
  onPrepare: () => void
  onPlay: () => void
  onStep: (delta: -1 | 1) => void
  onSeek: (index: number) => void
  onRate: (rate: number) => void
  onLoop: (loop: boolean) => void
  /** The frame console on the canvas plays the sequence; hide this row's duplicate controls. */
  consoleOwnsPlayback?: boolean
}

export function SectorTimeline(props: { state: SectorState; dispatch: Dispatch<SectorAction>; frameStates: Map<string, string>; transport: Transport; open: boolean; onOpenChange: (open: boolean) => void }) {
  const transport = props.transport
  const count = props.state.order.length
  const prepareLabel = transport.state === "building" ? "Preparing…"
    : transport.state === "decoding" ? "Buffering first frame…"
    : transport.state === "ready" || transport.state === "failed" ? "Rebuild playback"
    : "Prepare playback"
  const prepareDisabled = !!transport.prepareBlocked || transport.state === "building" || transport.state === "decoding"
  const oneFrame = transport.state === "ready" && count < 2 ? "One frame: step through it; Play needs at least two sectors." : undefined
  const failure = transport.failedFrame !== undefined
    ? `Frame ${transport.failedFrame + 1} failed. Playback stopped before advancing.`
    : transport.state === "failed" ? "Playback failed. Rebuild playback to retry." : undefined
  const status = transport.progress
    ?? (transport.buffering !== undefined ? `Buffering frame ${transport.buffering + 1}.` : undefined)
    ?? failure
    ?? (transport.playing ? "Playing." : undefined)
    ?? (transport.state === "ready" ? oneFrame ?? "Paused." : undefined)
    ?? (transport.state === "stale" ? "Composition changed since playback was prepared. Prepare again." : undefined)
    ?? (transport.state === "idle" && !transport.prepareBlocked ? "Prepare builds every sector and freezes one brightness scale for all frames." : "")
  return <details className="specific-section" open={props.open} onToggle={(event) => props.onOpenChange(event.currentTarget.open)}>
    <summary>Compare sectors <span>{count} sectors</span></summary>
    <div className="specific-actions">
      <button type="button" disabled={count < 2} onClick={() => props.dispatch({ type: "chronological" })}>Chronological order</button>
      <button type="button" disabled={prepareDisabled} onClick={transport.onPrepare}>{prepareLabel}</button>
    </div>
    <ol className="sector-timeline" aria-label="Sector playback order">{props.state.order.map((id, index) => {
      const sector = props.state.sectors.get(id)!
      const playhead = transport.ready && transport.showing && index === transport.current
      return <li key={id} data-playhead={playhead}>
        <button type="button" className="timeline-select" aria-pressed={id === props.state.activeSectorId} aria-current={playhead ? "step" : undefined} onClick={() => { props.dispatch({ type: "select", sectorId: id }); transport.onSeek(index) }}>
          <strong>{sector.name}</strong><span className="frame-state">{props.frameStates.get(id)}{playhead ? " · Showing" : ""}</span>
        </button>
        <div className="specific-actions"><button type="button" disabled={index === 0} aria-label={`Move ${sector.name} up`} onClick={() => props.dispatch({ type: "reorder", sectorId: id, direction: -1 })}>↑ Up</button><button type="button" disabled={index === count - 1} aria-label={`Move ${sector.name} down`} onClick={() => props.dispatch({ type: "reorder", sectorId: id, direction: 1 })}>↓ Down</button></div>
      </li>
    })}</ol>
    {!transport.consoleOwnsPlayback && <div className="specific-transport" role="group" aria-label="Playback" data-state={transport.state}>
      <button type="button" disabled={!transport.stepEnabled} onClick={() => transport.onStep(-1)}>Previous</button>
      <button type="button" disabled={!transport.ready || count < 2} onClick={transport.onPlay}>{transport.playing ? "Pause" : "Play"}</button>
      <button type="button" disabled={!transport.stepEnabled} onClick={() => transport.onStep(1)}>Next</button>
      <button type="button" aria-pressed={transport.loop} onClick={() => transport.onLoop(!transport.loop)}>Loop {transport.loop ? "on" : "off"}</button>
      <label className="specific-inline">Rate<select aria-label="Frames per second" value={transport.rate} onChange={(event) => transport.onRate(Number(event.target.value))}>{[1, 2, 4, 8].map((rate) => <option key={rate} value={rate}>{rate} fps</option>)}</select></label>
    </div>}
    {transport.consoleOwnsPlayback && transport.ready && <p className="specific-note">Playing on the canvas. Use its controls to blink, swipe, subtract and track movers; click a sector here to jump to it.</p>}
    <p role="status" className="specific-note">{status}</p>
    {transport.ready && transport.showing && !transport.consoleOwnsPlayback && <p className="specific-note">Frame {transport.current + 1} of {count}. Equal screen time per sector; time spans are labels, not cadence.</p>}
    {transport.prepareBlocked && <p className="specific-note">{transport.prepareBlocked}</p>}
    {transport.error && <p role="alert">{transport.error}</p>}
    {transport.failed.length > 0 && <ul aria-label="Failed sectors">{transport.failed.map((text) => <li key={text}>{text}</li>)}</ul>}
  </details>
}
