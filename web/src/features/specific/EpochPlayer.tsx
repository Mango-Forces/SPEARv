import { useEffect, useMemo, useRef, useState } from "react"
import { skyAtPixel, ViewerFrame, type CatalogStatusReport } from "../overlay/ViewerFrame"
import { clicksWork } from "../mosaic/MosaicPicture"
import type { Mosaic } from "../mosaic/useMosaic"
import type { SpectrumPointTarget } from "../spectrum-panel"
import type { View } from "../../url-state/view"
import { formatBands, type Bounds } from "./bounds"
import { mjdToMs, stepReady, type EpochVisit } from "./frames"
import { useEpochFrames, type EpochFrame } from "./useEpochFrames"
import "./specific.css"

/** What the canvas shows for the current epoch. The differences and the sum
 * are taken on the displayed grey levels, so they are a visual aid; the Delta
 * image remains the calibrated subtraction. */
type Show = "epoch" | "previous" | "first" | "sum"

const SHOW_LABELS: Record<Show, string> = {
  epoch: "Each epoch",
  previous: "Minus previous epoch",
  first: "Minus first epoch",
  sum: "All epochs added",
}

const TILE = 256
/** Grey levels per level of change; mid-grey is no change. */
const DIFFERENCE_GAIN = 1.5

/** The epochs of one field as a looping picture: one frame per SPHEREx visit,
 * all on the same grid, so anything that moves or varies shows as a flicker. */
export function EpochPlayer(props: {
  view: View
  bounds: Bounds
  armed: boolean
  run: number
  onBusy: (report: { run: number; busy: boolean }) => void
  onPointChange: (point: SpectrumPointTarget | null) => void
  onOpenSpectrum: () => void
  onCatalogStatus: (report: CatalogStatusReport) => void
}) {
  const { view, bounds, armed, run } = props
  const epochs = useEpochFrames(view, bounds, armed, run)
  const [index, setIndex] = useState(0)
  const [playing, setPlaying] = useState(() => !window.matchMedia("(prefers-reduced-motion: reduce)").matches)
  const [perSecond, setPerSecond] = useState(2)
  const [show, setShow] = useState<Show>("epoch")
  const pixels = useRef(new WeakMap<ImageBitmap, Uint8ClampedArray>())

  const ready = epochs.frames.flatMap((frame, at) => (frame.state === "ready" ? [at] : []))
  const readyKey = ready.join(",")
  const readyRef = useRef(ready)
  readyRef.current = ready
  const shown = ready.includes(index) ? index : ready[0]
  const current = shown === undefined ? undefined : epochs.frames[shown]
  const first = ready.length > 0 ? epochs.frames[ready[0]].bitmap : undefined
  const width = first?.width
  const height = first?.height
  const busy = epochs.phase === "listing" || epochs.phase === "loading"

  useEffect(() => {
    props.onBusy({ run, busy: armed && busy })
  }, [props.onBusy, run, armed, busy])

  useEffect(() => {
    setIndex(0)
  }, [run])

  useEffect(() => {
    if (!playing || show === "sum" || ready.length < 2) return
    const timer = setInterval(() => setIndex((at) => stepReady(readyRef.current, at, 1)), 1000 / perSecond)
    return () => clearInterval(timer)
  }, [playing, show, perSecond, ready.length < 2])

  const mosaic = useMemo<Mosaic | undefined>(() => {
    if (!width || !height) return undefined
    const canvas = document.createElement("canvas")
    canvas.width = width
    canvas.height = height
    return {
      canvas,
      info: { id: `epochs-${run}`, width, height, tile: TILE, cols: Math.ceil(width / TILE), rows: Math.ceil(height / TILE), exposures: 0 },
    }
  }, [width, height, run])

  useEffect(() => {
    const context = mosaic?.canvas.getContext("2d")
    if (!mosaic || !context || !current?.bitmap) return
    if (show === "epoch") {
      context.drawImage(current.bitmap, 0, 0)
      return
    }
    const read = (bitmap: ImageBitmap) => {
      const cached = pixels.current.get(bitmap)
      if (cached) return cached
      const scratch = document.createElement("canvas")
      scratch.width = bitmap.width
      scratch.height = bitmap.height
      const scratchContext = scratch.getContext("2d", { willReadFrequently: true })
      if (!scratchContext) return undefined
      scratchContext.drawImage(bitmap, 0, 0)
      const data = scratchContext.getImageData(0, 0, bitmap.width, bitmap.height).data
      pixels.current.set(bitmap, data)
      return data
    }
    const out = context.createImageData(mosaic.info.width, mosaic.info.height)
    const paint = (grey: (at: number) => number) => {
      for (let at = 0; at < out.data.length; at += 4) {
        const level = grey(at)
        out.data[at] = level
        out.data[at + 1] = level
        out.data[at + 2] = level
        out.data[at + 3] = 255
      }
      context.putImageData(out, 0, 0)
    }
    if (show === "sum") {
      const all = ready.flatMap((at) => {
        const bitmap = epochs.frames[at].bitmap
        const data = bitmap && read(bitmap)
        return data ? [data] : []
      })
      paint((at) => all.reduce((total, data) => total + data[at], 0) / Math.max(1, all.length))
      return
    }
    const referenceAt = show === "first" ? ready[0] : stepReady(ready, shown ?? 0, -1)
    const reference = epochs.frames[referenceAt]?.bitmap
    const now = read(current.bitmap)
    const before = reference && read(reference)
    if (!now || !before) return
    paint((at) => 128 + (now[at] - before[at]) * DIFFERENCE_GAIN)
  }, [mosaic, current?.bitmap, show, readyKey])

  const pick = (pixel: { x: number; y: number }) => {
    if (!mosaic || !clicksWork(mosaic)) return
    const sky = skyAtPixel(pixel.x, pixel.y, mosaic.info.width, mosaic.info.height, view.ra, view.dec)
    props.onPointChange({ x: pixel.x, y: pixel.y, ra: sky.ra, dec: sky.dec })
    props.onOpenSpectrum()
  }

  const step = (direction: 1 | -1) => {
    setPlaying(false)
    setIndex(stepReady(ready, shown ?? 0, direction))
  }
  const settled = epochs.frames.filter((frame) => frame.state !== "queued" && frame.state !== "loading").length
  const failed = epochs.frames.find((frame) => frame.state === "failed")
  const status = !armed
    ? "idle"
    : epochs.phase === "listing"
      ? "listing visits"
      : epochs.phase === "loading"
        ? `rendering epoch ${Math.min(settled + 1, epochs.frames.length)} of ${epochs.frames.length}`
        : epochs.phase === "failed"
          ? "failed"
          : `${ready.length} epoch${ready.length === 1 ? "" : "s"}`

  return (
    <section className="stage" aria-label="Sky">
      <figure className="viewer">
        {mosaic ? (
          <ViewerFrame
            mosaic={mosaic}
            candidatesReady={false}
            view={view}
            epochMjd={epochs.frames[ready[0]]?.visit.mjd}
            onCatalogStatus={props.onCatalogStatus}
            onPick={pick}
          />
        ) : (
          <div className="empty">
            {!armed ? (
              <span className="empty-find">Press Find to load every epoch of this field and blink them.</span>
            ) : busy ? (
              <span className="rendering">Rendering epochs · {status}</span>
            ) : epochs.phase === "failed" ? (
              "The epochs could not be loaded."
            ) : epochs.visitsTotal === 0 ? (
              "This release has no visit of this field."
            ) : epochs.frames.length === 0 ? (
              "No visit falls inside the date range."
            ) : (
              "No epoch has data in these bands."
            )}
          </div>
        )}
        {epochs.frames.length > 0 ? (
          <div
            className="epoch-bar"
            role="group"
            aria-label="Epoch playback"
            onKeyDown={(event) => {
              if (event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return
              if (event.key === "ArrowLeft") step(-1)
              if (event.key === "ArrowRight") step(1)
            }}
          >
            <div className="epoch-transport">
              <button type="button" aria-label="Previous epoch" title="Previous epoch" disabled={ready.length < 2 || show === "sum"} onClick={() => step(-1)}>
                <Glyph d="M15 5l-7 7 7 7" />
              </button>
              <button
                type="button"
                className="epoch-play"
                aria-label={playing ? "Pause" : "Play"}
                title={playing ? "Pause" : "Play"}
                aria-pressed={playing}
                disabled={ready.length < 2 || show === "sum"}
                onClick={() => setPlaying((value) => !value)}
              >
                {playing ? <Glyph d="M8 5v14M16 5v14" /> : <Glyph d="M7 5l12 7-12 7z" />}
              </button>
              <button type="button" aria-label="Next epoch" title="Next epoch" disabled={ready.length < 2 || show === "sum"} onClick={() => step(1)}>
                <Glyph d="M9 5l7 7-7 7" />
              </button>
            </div>
            <ol className="epoch-strip" aria-label="Epochs">
              {epochs.frames.map((frame, at) => (
                <li key={frame.visit.index}>
                  <button
                    type="button"
                    className="epoch-chip"
                    data-state={frame.state}
                    aria-pressed={show !== "sum" && at === shown}
                    disabled={frame.state !== "ready"}
                    title={chipTitle(frame)}
                    onClick={() => {
                      setPlaying(false)
                      setIndex(at)
                    }}
                  >
                    {visitDay(frame.visit.mjd)}
                  </button>
                </li>
              ))}
            </ol>
            <label className="epoch-option">
              Speed
              <input
                type="range"
                min={1}
                max={10}
                step={1}
                value={perSecond}
                aria-valuetext={`${perSecond} frames per second`}
                onChange={(event) => setPerSecond(Number(event.target.value))}
              />
            </label>
            <label className="epoch-option">
              Show
              <select value={show} onChange={(event) => setShow(event.target.value as Show)}>
                {(Object.keys(SHOW_LABELS) as Show[]).map((id) => (
                  <option key={id} value={id}>
                    {SHOW_LABELS[id]}
                  </option>
                ))}
              </select>
            </label>
          </div>
        ) : null}
        <figcaption>
          {current && show !== "sum" ? (
            <span className="epoch-now">
              Epoch {ready.indexOf(shown ?? 0) + 1} of {ready.length} · {visitSpan(current.visit)} · {current.visit.exposures} exposures
            </span>
          ) : current ? (
            <span className="epoch-now">{ready.length} epochs added</span>
          ) : null}
          {current && (show === "previous" || show === "first") ? (
            <span className="delta-legend">white: brighter in this epoch · black: brighter in the other</span>
          ) : null}
          <span>bands {formatBands(bounds.bands)}</span>
          {mosaic ? <span>{mosaic.info.width}×{mosaic.info.height} px</span> : null}
          <span className={busy ? "status busy" : "status"}>{status}</span>
          {mosaic && clicksWork(mosaic) ? <span className="caption-hint">Click a pixel for its spectrum.</span> : null}
        </figcaption>
        {armed && epochs.phase === "done" && ready.length === 1 ? (
          <p className="release-hint">
            Only one epoch has data here, so there is nothing to blink yet. Another release or a wider date range may hold more.
          </p>
        ) : null}
        {epochs.error ?? failed?.error ? (
          <p className="error" role="alert">
            {epochs.error ?? `Epoch ${visitDay(failed!.visit.mjd)}: ${failed!.error}`}
          </p>
        ) : null}
      </figure>
    </section>
  )
}

function Glyph({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d={d} />
    </svg>
  )
}

function visitDay(mjd: number): string {
  return new Date(mjdToMs(mjd)).toLocaleDateString("en-GB", { day: "numeric", month: "short", year: "numeric", timeZone: "UTC" })
}

function visitSpan(visit: EpochVisit): string {
  const start = visitDay(visit.mjd)
  const end = visitDay(visit.mjd_end)
  return start === end ? start : `${start} to ${end}`
}

function chipTitle(frame: EpochFrame): string {
  const span = `${visitSpan(frame.visit)} · ${frame.visit.exposures} exposures`
  if (frame.state === "empty") return `${span} · no data in these bands`
  if (frame.state === "failed") return `${span} · ${frame.error ?? "failed"}`
  if (frame.state === "ready") return span
  return `${span} · rendering`
}
