import { useEffect, useMemo, useState } from "react"
import { skyAtPixel, type CatalogStatusReport } from "../overlay/ViewerFrame"
import type { SpectrumPointTarget } from "../spectrum-panel"
import type { View } from "../../url-state/view"
import { formatBands, type Bounds } from "./bounds"
import { formatMjdDate, mjdToMs, qualityFlags, type ExposureRow } from "./frames"
import { useCalibratedFrames } from "./useCalibratedFrames"
import { FrameConsole, type ConsoleFrame } from "./FrameConsole"
import "./specific.css"

/** Blink: one calibrated frame per visit from the engine, played through the
 * shared console, plus an exposure table with header quality so any
 * hand-picked set can become a custom frame. */
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
  const [tableOpen, setTableOpen] = useState(false)
  const busy = epochs.phase === "listing" || epochs.phase === "loading"
  const ready = epochs.frames.filter((frame) => frame.state === "ready" && frame.plane)

  useEffect(() => {
    props.onBusy({ run, busy: armed && busy })
  }, [props.onBusy, run, armed, busy])

  useEffect(() => {
    setIndex(0)
    setCustom([])
    setPicked([])
    setCustomRun(0)
  }, [run])

  const frames = useMemo<ConsoleFrame[]>(
    () =>
      epochs.frames.map((frame) => ({
        id: frame.id,
        label: frame.label,
        mjd: frame.mjd,
        mjd_end: frame.mjd_end,
        state: frame.state,
        plane: frame.plane,
        error: frame.error,
        detail: `${frame.exposureIds.length || "?"} exposures${frame.kind === "custom" ? " · custom" : ""}`,
        // Custom frames reuse exposures from the visits, so they are not new epochs.
        epoch: frame.kind === "visit",
      })),
    [epochs.frames],
  )

  const settled = epochs.frames.filter((frame) => frame.state !== "queued" && frame.state !== "loading").length
  const status = !armed
    ? "idle"
    : epochs.phase === "listing"
      ? "listing exposures"
      : epochs.phase === "loading"
        ? `rendering frame ${Math.min(settled + 1, epochs.frames.length)} of ${epochs.frames.length}`
        : epochs.phase === "failed"
          ? "failed"
          : `${ready.length} frame${ready.length === 1 ? "" : "s"}`

  const composeCustom = () => {
    if (picked.length === 0) return
    const id = `custom-${Date.now()}`
    setCustom((list) => [...list, { id, label: `Custom ${list.length + 1}`, exposureIds: picked }])
    setCustomRun((value) => value + 1)
    setPicked([])
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
      <FrameConsole
        frames={frames}
        index={index}
        onIndex={setIndex}
        playing={playing}
        onPlaying={setPlaying}
        noun="epoch"
        view={view}
        canvasKey={`epochs-${run}-${customRun}`}
        bandsLabel={`bands ${formatBands(bounds.bands)}`}
        bands={bounds.bands}
        busy={busy}
        status={status}
        onCatalogStatus={props.onCatalogStatus}
        onPick={(pixel, grid) => {
          const sky = skyAtPixel(pixel.x, pixel.y, grid.width, grid.height, view.ra, view.dec)
          props.onPointChange({ x: pixel.x, y: pixel.y, ra: sky.ra, dec: sky.dec })
          props.onOpenSpectrum()
        }}
        empty={
          !armed ? (
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
          )
        }
      >
        {armed && epochs.phase === "done" && ready.length === 1 ? (
          <p className="release-hint">
            Only one epoch has data here, so there is nothing to blink yet. Another release or a wider date range may hold more.
          </p>
        ) : null}
        {epochs.error ? (
          <p className="error" role="alert">
            {epochs.error}
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
      </FrameConsole>
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
      <td>{props.row.visit ?? "-"}</td>
      <td>D{props.row.detector}</td>
      <td title={flags.join(", ") || (pass ? "Pass" : "Unknown")}>
        {flags.length ? flags.join(", ") : pass ? "Pass" : "-"}
        {props.row.quality?.psf_fwhm != null ? ` · ${props.row.quality.psf_fwhm.toFixed(1)}″` : ""}
      </td>
      <td title={props.row.version}>{props.row.obsid}</td>
    </tr>
  )
}
