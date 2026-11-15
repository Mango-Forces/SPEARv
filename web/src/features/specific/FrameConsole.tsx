import { useEffect, useMemo, useRef, useState, type ReactNode } from "react"
import { ViewerFrame, type CatalogStatusReport } from "../overlay/ViewerFrame"
import { clicksWork } from "../mosaic/MosaicPicture"
import type { Mosaic } from "../mosaic/useMosaic"
import type { View } from "../../url-state/view"
import {
  centreCoverage,
  paintDifference,
  paintMotionGlow,
  paintOnion,
  paintPlane,
  paintSum,
  paintSwipe,
  resolveStretch,
  sharedStretch,
  TONE_SOFT,
  type DisplayTone,
  type FramePlane,
  type Stretch,
} from "./calibrate"
import { formatMjdDate, stepReady } from "./frames"
import { heading, MIN_EPOCHS, searchMotion, type MotionSearch } from "./motion"
import { ARROW_YEARS, describe, MotionLayer } from "./MotionLayer"
import "./specific.css"

/** One calibrated frame on the shared grid: an epoch in Blink, a sector or a
 * single exposure in Lab. Values are MJy/sr with NaN for no data. */
export type ConsoleFrame = {
  id: string
  /** Chip text. */
  label: string
  mjd: number
  mjd_end: number
  state: "queued" | "loading" | "ready" | "empty" | "failed"
  plane?: FramePlane
  error?: string
  /** Caption text after the frame's dates, e.g. "31 exposures". */
  detail: string
  /** Fits a motion track. Frames that reuse other frames' exposures do not. */
  epoch: boolean
}

export type Noun = "epoch" | "sector" | "exposure"

type Show = "epoch" | "onion" | "swipe" | "previous" | "first" | "glow" | "sum"

const TILE = 256

/** Blink, compare and motion tools over any set of calibrated frames. The host
 * owns which frame shows and whether it plays, so its own lists stay in step. */
export function FrameConsole(props: {
  frames: ConsoleFrame[]
  index: number
  onIndex: (index: number) => void
  playing: boolean
  onPlaying: (playing: boolean) => void
  noun: Noun
  view: View
  /** Changes when a new set of frames replaces the old one. */
  canvasKey: string
  /** Caption band summary, e.g. "bands 69-102". */
  bandsLabel?: string
  /** Bands in the frames, for the cold-brown-dwarf hint. */
  bands?: readonly number[]
  busy: boolean
  status: string
  /** Shown instead of the image until a frame is ready. */
  empty: ReactNode
  onPick: (pixel: { x: number; y: number }, grid: { width: number; height: number }) => void
  onCatalogStatus?: (report: CatalogStatusReport) => void
  children?: ReactNode
}) {
  const { frames, noun } = props
  const Noun = noun[0]!.toUpperCase() + noun.slice(1)
  const [show, setShow] = useState<Show>("epoch")
  const [loop, setLoop] = useState(true)
  // WiseView-style display: linear + invert is the classic motion blink.
  const [invert, setInvert] = useState(false)
  const [linear, setLinear] = useState(false)
  const [manualLow, setManualLow] = useState("")
  const [manualHigh, setManualHigh] = useState("")
  // Blink interval in ms, the unit WiseView uses (speed=500 → 500 ms).
  const [intervalMs, setIntervalMs] = useState(500)
  /** Diff / glow gain. 1 = mild, 3 = loud residuals. */
  const [diffGain, setDiffGain] = useState(2)
  /** Divider position for swipe, as a fraction of the width. */
  const [split, setSplit] = useState(0.5)
  /** Optional locked second frame; defaults to the first or previous one. */
  const [lockB, setLockB] = useState<number | null>(null)
  const [trackMovers, setTrackMovers] = useState(true)

  const ready = frames.flatMap((frame, at) => (frame.state === "ready" && frame.plane ? [at] : []))
  const readyKey = ready.join(",")
  const shown = ready.includes(props.index) ? props.index : ready[0]
  const current = shown === undefined ? undefined : frames[shown]
  const first = ready.length > 0 ? frames[ready[0]!]!.plane : undefined
  const width = first?.width
  const height = first?.height
  const planesKey = `${props.canvasKey}:${readyKey}`

  // The timer reads the latest list and callbacks without restarting.
  const live = useRef({ ready, shown, onIndex: props.onIndex, onPlaying: props.onPlaying })
  live.current = { ready, shown, onIndex: props.onIndex, onPlaying: props.onPlaying }

  useEffect(() => setLockB(null), [props.canvasKey])

  useEffect(() => {
    if (!props.playing || show === "sum" || ready.length < 2) return
    const timer = setInterval(() => {
      const { ready: list, shown: at, onIndex, onPlaying } = live.current
      if (at === undefined) return
      if (!loop && at === list[list.length - 1]) {
        onPlaying(false)
        return
      }
      onIndex(stepReady(list, at, 1))
    }, intervalMs)
    return () => clearInterval(timer)
  }, [props.playing, show, intervalMs, ready.length < 2, loop])

  const autoStretch = useMemo<Stretch | undefined>(() => {
    const planes = ready.flatMap((at) => (frames[at]?.plane ? [frames[at]!.plane!] : []))
    return planes.length ? sharedStretch(planes) : undefined
  }, [planesKey])
  const stretch = useMemo<Stretch | undefined>(() => {
    if (!autoStretch) return undefined
    const low = manualLow.trim() === "" ? null : Number(manualLow)
    const high = manualHigh.trim() === "" ? null : Number(manualHigh)
    return resolveStretch(autoStretch, low, high)
  }, [autoStretch, manualLow, manualHigh])
  const display: DisplayTone = useMemo(() => ({ gamma: linear ? 1 : TONE_SOFT.gamma, invert }), [linear, invert])

  const mosaic = useMemo<Mosaic | undefined>(() => {
    if (!width || !height) return undefined
    const canvas = document.createElement("canvas")
    canvas.width = width
    canvas.height = height
    return {
      canvas,
      info: {
        id: `console-${props.canvasKey}`,
        width,
        height,
        tile: TILE,
        cols: Math.ceil(width / TILE),
        rows: Math.ceil(height / TILE),
        exposures: 0,
      },
    }
  }, [width, height, props.canvasKey])

  const referenceAt = useMemo(() => {
    if (shown === undefined || ready.length === 0) return undefined
    if (lockB !== null && ready.includes(lockB) && lockB !== shown) return lockB
    if (show === "first" || show === "glow" || show === "onion") return ready[0]
    if (show === "previous" || show === "swipe") return stepReady(ready, shown, -1)
    return undefined
  }, [show, shown, readyKey, lockB])
  const reference = referenceAt === undefined ? undefined : frames[referenceAt]

  useEffect(() => {
    const context = mosaic?.canvas.getContext("2d")
    if (!mosaic || !context || !current?.plane || !stretch) return
    const out = context.createImageData(mosaic.info.width, mosaic.info.height)
    const other = reference?.plane
    if (show === "sum") {
      const planes = ready.flatMap((at) => (frames[at]?.plane ? [frames[at]!.plane!] : []))
      paintSum(planes, stretch, out, display)
    } else if (show === "onion" && other) {
      paintOnion(current.plane, other, stretch, out, display, 0.5)
    } else if (show === "swipe" && other) {
      paintSwipe(current.plane, other, stretch, out, display, split)
    } else if (show === "glow" && other) {
      paintMotionGlow(current.plane, other, stretch, out, display, 0.06, diffGain)
    } else if ((show === "previous" || show === "first") && other) {
      paintDifference(current.plane, other, stretch, out, display, diffGain)
    } else {
      paintPlane(current.plane, stretch, out, display)
    }
    context.putImageData(out, 0, 0)
  }, [mosaic, current?.plane, reference?.plane, show, planesKey, stretch, display, diffGain, split])

  // Survey-style motion search: detect per frame, match, fit position against time.
  const motion = useMemo<MotionSearch | undefined>(() => {
    if (!trackMovers) return undefined
    const epochs = ready
      .map((at) => frames[at]!)
      .filter((frame) => frame.epoch && frame.plane)
      .sort((a, b) => a.mjd + a.mjd_end - (b.mjd + b.mjd_end))
    if (epochs.length === 0) return undefined
    const start = epochs[0]!.mjd
    return searchMotion(epochs.map((frame) => ({ years: ((frame.mjd + frame.mjd_end) / 2 - start) / 365.25, plane: frame.plane! })))
  }, [trackMovers, planesKey])

  const pick = (pixel: { x: number; y: number }) => {
    if (!mosaic || !clicksWork(mosaic)) return
    props.onPick(pixel, { width: mosaic.info.width, height: mosaic.info.height })
  }

  const step = (direction: 1 | -1) => {
    props.onPlaying(false)
    props.onIndex(stepReady(ready, shown ?? 0, direction))
  }

  const coverage = current?.plane ? centreCoverage(current.plane) : undefined
  const epochMjd = current?.mjd ?? frames[ready[0] ?? -1]?.mjd
  const labels: Record<Show, string> = {
    epoch: `Blink each ${noun}`,
    onion: "Half-and-half overlay",
    swipe: "Swipe compare",
    previous: `Minus previous ${noun}`,
    first: `Minus first ${noun}`,
    glow: "Motion glow (|A−B|)",
    sum: `All ${noun}s added`,
  }
  const failed = frames.find((frame) => frame.state === "failed")
  const stepDisabled = ready.length < 2 || show === "sum"

  return (
    <figure className="viewer frame-console">
      {mosaic ? (
        <ViewerFrame
          mosaic={mosaic}
          candidatesReady={false}
          view={props.view}
          epochMjd={epochMjd}
          onCatalogStatus={props.onCatalogStatus}
          onPick={pick}
          overlay={(frame) =>
            motion && !props.busy ? (
              <MotionLayer movers={motion.movers} native={frame.native} scale={frame.scale} onPick={pick} />
            ) : null
          }
        />
      ) : (
        <div className="empty">{props.empty}</div>
      )}
      {frames.length > 0 ? (
        <div
          className="epoch-bar"
          role="group"
          aria-label={`${Noun} playback`}
          onKeyDown={(event) => {
            if (event.target instanceof HTMLInputElement || event.target instanceof HTMLSelectElement) return
            if (event.key === "ArrowLeft") step(-1)
            if (event.key === "ArrowRight") step(1)
          }}
        >
          <div className="epoch-transport">
            <button type="button" aria-label={`Previous ${noun}`} title={`Previous ${noun}`} disabled={stepDisabled} onClick={() => step(-1)}>
              <Glyph d="M15 5l-7 7 7 7" />
            </button>
            <button
              type="button"
              className="epoch-play"
              aria-label={props.playing ? "Pause" : "Play"}
              title={props.playing ? "Pause" : "Play"}
              aria-pressed={props.playing}
              disabled={stepDisabled}
              onClick={() => props.onPlaying(!props.playing)}
            >
              {props.playing ? <Glyph d="M8 5v14M16 5v14" /> : <Glyph d="M7 5l12 7-12 7z" />}
            </button>
            <button type="button" aria-label={`Next ${noun}`} title={`Next ${noun}`} disabled={stepDisabled} onClick={() => step(1)}>
              <Glyph d="M9 5l7 7-7 7" />
            </button>
          </div>
          <ol className="epoch-strip" aria-label={`${Noun}s`}>
            {frames.map((frame, at) => (
              <li key={frame.id}>
                <button
                  type="button"
                  className="epoch-chip"
                  data-state={frame.state}
                  aria-pressed={show !== "sum" && at === shown}
                  disabled={frame.state !== "ready"}
                  title={chipTitle(frame)}
                  onClick={() => {
                    props.onPlaying(false)
                    props.onIndex(at)
                  }}
                >
                  {frame.label}
                </button>
              </li>
            ))}
          </ol>
          <label className="epoch-option">
            Blink
            <input
              type="range"
              min={100}
              max={1500}
              step={50}
              value={intervalMs}
              aria-valuetext={`${intervalMs} milliseconds`}
              title={`${intervalMs} ms between frames`}
              onChange={(event) => setIntervalMs(Number(event.target.value))}
            />
            <span className="epoch-ms">{intervalMs} ms</span>
          </label>
          <label className="epoch-option">
            Show
            <select value={show} onChange={(event) => setShow(event.target.value as Show)}>
              {(Object.keys(labels) as Show[]).map((id) => (
                <option key={id} value={id}>
                  {labels[id]}
                </option>
              ))}
            </select>
          </label>
          {show === "swipe" ? (
            <label className="epoch-option" title="Divider position">
              Divider
              <input
                type="range"
                min={0}
                max={1}
                step={0.01}
                value={split}
                aria-valuetext={`${Math.round(split * 100)} percent`}
                onChange={(event) => setSplit(Number(event.target.value))}
              />
            </label>
          ) : null}
          <label className="epoch-option epoch-loop">
            <input type="checkbox" checked={loop} onChange={(event) => setLoop(event.target.checked)} />
            Loop
          </label>
          <label className="epoch-option epoch-loop" title="Detect sources in every frame, match them, and fit position against time">
            <input type="checkbox" checked={trackMovers} onChange={(event) => setTrackMovers(event.target.checked)} />
            Track movers
          </label>
          <label className="epoch-option epoch-loop" title="Linear stretch, inverted greyscale: the classic WiseView motion blink">
            <input
              type="checkbox"
              checked={invert && linear}
              onChange={(event) => {
                const on = event.target.checked
                setInvert(on)
                setLinear(on)
                if (on) setIntervalMs(500)
              }}
            />
            WiseView look
          </label>
          <label className="epoch-option epoch-loop">
            <input type="checkbox" checked={invert} onChange={(event) => setInvert(event.target.checked)} />
            Invert
          </label>
          <label className="epoch-option epoch-loop">
            <input type="checkbox" checked={linear} onChange={(event) => setLinear(event.target.checked)} />
            Linear
          </label>
          <label className="epoch-option" title="Leave blank for the auto shared stretch">
            Min
            <input
              className="epoch-stretch"
              type="text"
              inputMode="decimal"
              placeholder={autoStretch ? autoStretch.low.toPrecision(3) : "auto"}
              value={manualLow}
              onChange={(event) => setManualLow(event.target.value)}
            />
          </label>
          <label className="epoch-option" title="Leave blank for the auto shared stretch">
            Max
            <input
              className="epoch-stretch"
              type="text"
              inputMode="decimal"
              placeholder={autoStretch ? autoStretch.high.toPrecision(3) : "auto"}
              value={manualHigh}
              onChange={(event) => setManualHigh(event.target.value)}
            />
          </label>
          <label className="epoch-option" title="How hard difference and motion glow push residuals">
            Gain
            <input type="range" min={0.5} max={4} step={0.1} value={diffGain} onChange={(event) => setDiffGain(Number(event.target.value))} />
            <span className="epoch-ms">{diffGain.toFixed(1)}×</span>
          </label>
          <label className="epoch-option" title={`Second frame for overlay, swipe, difference and motion glow. Default is the first or previous ${noun}.`}>
            vs
            <select value={lockB === null ? "" : String(lockB)} onChange={(event) => setLockB(event.target.value === "" ? null : Number(event.target.value))}>
              <option value="">auto</option>
              {ready.map((at) => (
                <option key={frames[at]!.id} value={at} disabled={at === shown}>
                  {frames[at]!.label}
                </option>
              ))}
            </select>
          </label>
        </div>
      ) : null}
      <figcaption>
        {current && show !== "sum" ? (
          <span className="epoch-now">
            {Noun} {(ready.indexOf(shown ?? 0) + 1) || 1} of {ready.length} · {frameSpan(current)}
            {current.detail ? ` · ${current.detail}` : ""}
          </span>
        ) : current ? (
          <span className="epoch-now">
            {ready.length} {noun}s added · shared stretch
          </span>
        ) : null}
        {current && (show === "previous" || show === "first") ? (
          <span className="delta-legend">
            white: brighter now · black: brighter in {reference?.label ?? "the other"} · calibrated ×{diffGain.toFixed(1)}
          </span>
        ) : null}
        {current && show === "onion" ? (
          <span className="delta-legend">50% this {noun} + 50% {reference?.label ?? "reference"}: movers double, fixed sky sits still</span>
        ) : null}
        {current && show === "swipe" ? (
          <span className="delta-legend">
            left: {current.label} · right: {reference?.label ?? "reference"} · same stretch and sky grid
          </span>
        ) : null}
        {current && show === "glow" ? (
          <span className="delta-legend">lime where |this − {reference?.label ?? "reference"}| is large · fixed sky stays grey</span>
        ) : null}
        {motion && !props.busy ? <MotionSummary motion={motion} noun={noun} bands={props.bands} onPick={pick} /> : null}
        {props.bandsLabel ? <span>{props.bandsLabel}</span> : null}
        {stretch ? (
          <span title="Shared display stretch across every frame">
            stretch {stretch.low.toPrecision(3)}–{stretch.high.toPrecision(3)} MJy/sr
            {invert ? " · inverted" : ""}
            {linear ? " · linear" : ""}
          </span>
        ) : null}
        {mosaic ? (
          <span>
            {mosaic.info.width}×{mosaic.info.height} px
          </span>
        ) : null}
        <span className={props.busy ? "status busy" : "status"}>{props.status}</span>
        {mosaic && clicksWork(mosaic) ? <span className="caption-hint">Click a pixel for its spectrum.</span> : null}
      </figcaption>
      {coverage !== undefined && coverage < 0.2 ? (
        <p className="release-hint" role="status">
          Little or no data at the field centre in this frame ({Math.round(coverage * 100)}% of the centre aperture).
        </p>
      ) : null}
      {failed?.error ? (
        <p className="error" role="alert">
          {failed.label}: {failed.error}
        </p>
      ) : null}
      {props.children}
    </figure>
  )
}

function Glyph({ d }: { d: string }) {
  return (
    <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" strokeWidth="2" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true" focusable="false">
      <path d={d} />
    </svg>
  )
}

export function frameSpan(frame: { mjd: number; mjd_end: number }): string {
  const start = formatMjdDate(frame.mjd)
  const end = formatMjdDate(frame.mjd_end)
  return start === end ? start : `${start} to ${end}`
}

function chipTitle(frame: ConsoleFrame): string {
  const span = `${frameSpan(frame)}${frame.detail ? ` · ${frame.detail}` : ""}`
  if (frame.state === "empty") return `${span} · no data in these bands`
  if (frame.state === "failed") return `${span} · ${frame.error ?? "failed"}`
  if (frame.state === "ready") return span
  return `${span} · loading`
}

/** What the motion search found, in words, with each mover clickable for its spectrum. */
function MotionSummary(props: { motion: MotionSearch; noun: Noun; bands?: readonly number[]; onPick: (pick: { x: number; y: number }) => void }) {
  const { motion } = props
  if (motion.epochs < MIN_EPOCHS) {
    return (
      <span className="epoch-motion quiet">
        Motion fit needs {MIN_EPOCHS} {props.noun}s at different times; this set has {motion.epochs}.
      </span>
    )
  }
  const floor = `noise floor ${motion.floor.toFixed(1)}″/yr over ${motion.tracked} tracked sources`
  if (motion.movers.length === 0) {
    const shortOnly = props.bands !== undefined && !props.bands.some((band) => band > 68)
    return (
      <span className="epoch-motion quiet">
        No mover above 3σ ({floor}).
        {shortOnly ? " Cold brown dwarfs barely show below 3 µm; try D5 or D6." : ""}
      </span>
    )
  }
  return (
    <span className="epoch-motion">
      {motion.movers.slice(0, 4).map((mover, at) => (
        <button
          key={at}
          type="button"
          className={`mover-chip ${mover.tier}`}
          title={describe(mover)}
          onClick={() => props.onPick({ x: Math.floor(mover.x), y: Math.floor(mover.y) })}
        >
          {mover.tier === "candidate" ? "Mover" : "Possible"} {mover.pm.toFixed(1)} ± {mover.error.toFixed(1)}″/yr{" "}
          {heading(mover.pmRa, mover.pmDec)} · {mover.significance.toFixed(1)}σ
        </button>
      ))}
      {motion.movers.length > 4 ? <span>+{motion.movers.length - 4} more</span> : null}
      <span className="quiet">
        {floor} · arrows show {ARROW_YEARS} years
      </span>
    </span>
  )
}
