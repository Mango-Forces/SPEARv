import { useEffect, useMemo, useRef, useState } from "react"
import { skyAtPixel, ViewerFrame, type CatalogStatusReport } from "../overlay/ViewerFrame"
import { clicksWork } from "../mosaic/MosaicPicture"
import type { Mosaic } from "../mosaic/useMosaic"
import type { SpectrumPointTarget } from "../spectrum-panel"
import type { View } from "../../url-state/view"
import { formatBands, type Bounds } from "./bounds"
import {
  centreCoverage,
  paintDifference,
  paintPlane,
  paintSum,
  sharedStretch,
  type Stretch,
} from "./calibrate"
import { formatMjdDate, mjdToMs, qualityFlags, stepReady, type ExposureRow } from "./frames"
import { useCalibratedFrames, type CalibratedFrame } from "./useCalibratedFrames"
import "./specific.css"

type Show = "epoch" | "previous" | "first" | "sum"

const SHOW_LABELS: Record<Show, string> = {
  epoch: "Each epoch",
  previous: "Minus previous epoch",
  first: "Minus first epoch",
  sum: "All epochs added",
}

const TILE = 256

/** The fused Specific workspace: calibrated frames from the engine, one shared
 * stretch, blink / subtract / add in the browser, and an exposure table with
 * header quality so any hand-picked set can become a custom frame. */
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
  const [custom, setCustom] = useState<{ id: string; label: string; exposureIds: string[] }[]>([])
  const [picked, setPicked] = useState<string[]>([])
  const [customRun, setCustomRun] = useState(0)
  const epochs = useCalibratedFrames(view, bounds, armed, run + customRun * 1000, custom)
  const [index, setIndex] = useState(0)
  const [playing, setPlaying] = useState(() => !window.matchMedia("(prefers-reduced-motion: reduce)").matches)
  const [perSecond, setPerSecond] = useState(2)
  const [show, setShow] = useState<Show>("epoch")
  const [loop, setLoop] = useState(true)
  const [tableOpen, setTableOpen] = useState(false)

  const ready = epochs.frames.flatMap((frame, at) => (frame.state === "ready" && frame.plane ? [at] : []))
  const readyKey = ready.join(",")
  const readyRef = useRef(ready)
  readyRef.current = ready
  const shown = ready.includes(index) ? index : ready[0]
  const current = shown === undefined ? undefined : epochs.frames[shown]
  const first = ready.length > 0 ? epochs.frames[ready[0]!]!.plane : undefined
  const width = first?.width
  const height = first?.height
  const busy = epochs.phase === "listing" || epochs.phase === "loading"
  const stretch = useMemo<Stretch | undefined>(() => {
    const planes = ready.flatMap((at) => {
      const plane = epochs.frames[at]?.plane
      return plane ? [plane] : []
    })
    return planes.length ? sharedStretch(planes) : undefined
  }, [readyKey, epochs.frames])

  useEffect(() => {
    props.onBusy({ run, busy: armed && busy })
  }, [props.onBusy, run, armed, busy])

  useEffect(() => {
    setIndex(0)
    setCustom([])
    setPicked([])
    setCustomRun(0)
  }, [run])

  useEffect(() => {
    if (!playing || show === "sum" || ready.length < 2) return
    const timer = setInterval(() => {
      setIndex((at) => {
        const next = stepReady(readyRef.current, at, 1)
        if (!loop && next <= at && readyRef.current[0] !== undefined && at === readyRef.current[readyRef.current.length - 1]) {
          setPlaying(false)
          return at
        }
        return next
      })
    }, 1000 / perSecond)
    return () => clearInterval(timer)
  }, [playing, show, perSecond, ready.length < 2, loop])

  const mosaic = useMemo<Mosaic | undefined>(() => {
    if (!width || !height) return undefined
    const canvas = document.createElement("canvas")
    canvas.width = width
    canvas.height = height
    return {
      canvas,
      info: {
        id: `epochs-${run}-${customRun}`,
        width,
        height,
        tile: TILE,
        cols: Math.ceil(width / TILE),
        rows: Math.ceil(height / TILE),
        exposures: 0,
      },
    }
  }, [width, height, run, customRun])

  useEffect(() => {
    const context = mosaic?.canvas.getContext("2d")
    if (!mosaic || !context || !current?.plane || !stretch) return
    const out = context.createImageData(mosaic.info.width, mosaic.info.height)
    if (show === "epoch") {
      paintPlane(current.plane, stretch, out)
    } else if (show === "sum") {
      const planes = ready.flatMap((at) => {
        const plane = epochs.frames[at]?.plane
        return plane ? [plane] : []
      })
      paintSum(planes, stretch, out)
    } else {
      const referenceAt = show === "first" ? ready[0]! : stepReady(ready, shown ?? 0, -1)
      const reference = epochs.frames[referenceAt]?.plane
      if (!reference) return
      paintDifference(current.plane, reference, stretch, out)
    }
    context.putImageData(out, 0, 0)
  }, [mosaic, current?.plane, show, readyKey, stretch])

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
      ? "listing exposures"
      : epochs.phase === "loading"
        ? `rendering frame ${Math.min(settled + 1, epochs.frames.length)} of ${epochs.frames.length}`
        : epochs.phase === "failed"
          ? "failed"
          : `${ready.length} frame${ready.length === 1 ? "" : "s"}`
  const coverage = current?.plane ? centreCoverage(current.plane) : undefined
  const epochMjd = current?.mjd ?? epochs.frames[ready[0] ?? -1]?.mjd

  const composeCustom = () => {
    if (picked.length === 0) return
    const id = `custom-${Date.now()}`
    setCustom((list) => [...list, { id, label: `Custom ${list.length + 1}`, exposureIds: picked }])
    setCustomRun((value) => value + 1)
    setPicked([])
    setShow("epoch")
  }

  const filteredExposures = useMemo(() => {
    const from = bounds.fromUtc ? Date.parse(bounds.fromUtc) : -Infinity
    const to = bounds.toUtc ? Date.parse(bounds.toUtc) : Infinity
    return epochs.exposures.filter((row) => {
      const ms = mjdToMs(row.mjd)
      return ms >= from && ms <= to
    })
  }, [epochs.exposures, bounds.fromUtc, bounds.toUtc])

  return (
    <section className="stage" aria-label="Sky">
      <figure className="viewer">
        {mosaic ? (
          <ViewerFrame
            mosaic={mosaic}
            candidatesReady={false}
            view={view}
            epochMjd={epochMjd}
            onCatalogStatus={props.onCatalogStatus}
            onPick={pick}
          />
        ) : (
          <div className="empty">
            {!armed ? (
              <span className="empty-find">Press Find to load every epoch of this field and blink them.</span>
            ) : busy ? (
              <span className="rendering">Rendering calibrated frames · {status}</span>
            ) : epochs.phase === "failed" ? (
              "The epochs could not be loaded."
            ) : epochs.visits.length === 0 && epochs.exposures.length === 0 ? (
              "This release has no exposure of this field."
            ) : epochs.frames.length === 0 ? (
              "No visit falls inside the date range."
            ) : (
              "No frame has data in these bands."
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
                <li key={frame.id}>
                  <button
                    type="button"
                    className="epoch-chip"
                    data-state={frame.state}
                    data-kind={frame.kind}
                    aria-pressed={show !== "sum" && at === shown}
                    disabled={frame.state !== "ready"}
                    title={chipTitle(frame)}
                    onClick={() => {
                      setPlaying(false)
                      setIndex(at)
                    }}
                  >
                    {frame.label}
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
            <label className="epoch-option epoch-loop">
              <input type="checkbox" checked={loop} onChange={(event) => setLoop(event.target.checked)} />
              Loop
            </label>
          </div>
        ) : null}
        <figcaption>
          {current && show !== "sum" ? (
            <span className="epoch-now">
              {current.kind === "custom" ? "Custom" : `Epoch ${(ready.indexOf(shown ?? 0) + 1) || 1}`} of {ready.length}
              {" · "}
              {visitSpan(current)}
              {" · "}
              {current.exposureIds.length || "?"} exposures
            </span>
          ) : current ? (
            <span className="epoch-now">{ready.length} frames added · shared stretch</span>
          ) : null}
          {current && (show === "previous" || show === "first") ? (
            <span className="delta-legend">white: brighter in this epoch · black: brighter in the other · calibrated</span>
          ) : null}
          <span>bands {formatBands(bounds.bands)}</span>
          {stretch ? (
            <span title="Shared display stretch across every frame">
              stretch {stretch.low.toPrecision(3)}–{stretch.high.toPrecision(3)} MJy/sr
            </span>
          ) : null}
          {mosaic ? (
            <span>
              {mosaic.info.width}×{mosaic.info.height} px
            </span>
          ) : null}
          <span className={busy ? "status busy" : "status"}>{status}</span>
          {mosaic && clicksWork(mosaic) ? <span className="caption-hint">Click a pixel for its spectrum.</span> : null}
        </figcaption>
        {coverage !== undefined && coverage < 0.2 ? (
          <p className="release-hint" role="status">
            Little or no data at the field centre in this frame ({Math.round(coverage * 100)}% of the centre aperture).
          </p>
        ) : null}
        {armed && epochs.phase === "done" && ready.length === 1 ? (
          <p className="release-hint">
            Only one epoch has data here, so there is nothing to blink yet. Another release or a wider date range may hold more.
          </p>
        ) : null}
        {epochs.error ?? failed?.error ? (
          <p className="error" role="alert">
            {epochs.error ?? `${failed!.label}: ${failed!.error}`}
          </p>
        ) : null}
        {armed && epochs.exposures.length > 0 ? (
          <details className="epoch-table" open={tableOpen} onToggle={(event) => setTableOpen((event.target as HTMLDetailsElement).open)}>
            <summary>
              Exposures · {filteredExposures.length.toLocaleString("en-GB")}
              {picked.length ? ` · ${picked.length} selected` : ""}
            </summary>
            <div className="epoch-table-actions">
              <button type="button" disabled={picked.length === 0} onClick={composeCustom}>
                Make frame from selection
              </button>
              <button type="button" disabled={picked.length === 0} onClick={() => setPicked([])}>
                Clear selection
              </button>
              <span className="epoch-table-note">Select rows, then make a custom frame. It is stacked from those exposures only, on the same calibrated scale.</span>
            </div>
            <div className="epoch-table-scroll" tabIndex={0} role="region" aria-label="Exposure table">
              <table>
                <thead>
                  <tr>
                    <th scope="col">Pick</th>
                    <th scope="col">UTC</th>
                    <th scope="col">Visit</th>
                    <th scope="col">Det</th>
                    <th scope="col">Quality</th>
                    <th scope="col">Obs</th>
                  </tr>
                </thead>
                <tbody>
                  {filteredExposures.map((row) => (
                    <ExposureRowView
                      key={row.id}
                      row={row}
                      checked={picked.includes(row.id)}
                      onToggle={() =>
                        setPicked((list) => (list.includes(row.id) ? list.filter((id) => id !== row.id) : [...list, row.id]))
                      }
                    />
                  ))}
                </tbody>
              </table>
            </div>
          </details>
        ) : null}
      </figure>
    </section>
  )
}

function ExposureRowView(props: { row: ExposureRow; checked: boolean; onToggle: () => void }) {
  const flags = qualityFlags(props.row.quality)
  const pass =
    props.row.quality?.dqa_l1?.toLowerCase() === "pass" && props.row.quality?.dqa_l2?.toLowerCase() === "pass"
  return (
    <tr data-dirty={flags.length > 0 || undefined}>
      <td>
        <input type="checkbox" checked={props.checked} aria-label={`Select ${props.row.obsid} D${props.row.detector}`} onChange={props.onToggle} />
      </td>
      <td>
        <time dateTime={new Date(mjdToMs(props.row.mjd)).toISOString()}>{formatMjdDate(props.row.mjd)}</time>
      </td>
      <td>{props.row.visit ?? "—"}</td>
      <td>D{props.row.detector}</td>
      <td title={flags.join(", ") || (pass ? "Pass" : "Unknown")}>
        {flags.length ? flags.join(", ") : pass ? "Pass" : "—"}
        {props.row.quality?.psf_fwhm != null ? ` · ${props.row.quality.psf_fwhm.toFixed(1)}″` : ""}
      </td>
      <td title={props.row.version}>{props.row.obsid}</td>
    </tr>
  )
}

function Glyph({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d={d} />
    </svg>
  )
}

function visitSpan(frame: CalibratedFrame): string {
  const start = formatMjdDate(frame.mjd)
  const end = formatMjdDate(frame.mjd_end)
  return start === end ? start : `${start} to ${end}`
}

function chipTitle(frame: CalibratedFrame): string {
  const span = `${visitSpan(frame)} · ${frame.exposureIds.length || "?"} exposures`
  if (frame.state === "empty") return `${span} · no data in these bands`
  if (frame.state === "failed") return `${span} · ${frame.error ?? "failed"}`
  if (frame.state === "ready") return span
  return `${span} · rendering`
}
