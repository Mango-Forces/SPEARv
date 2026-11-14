import { bandNumber, detectorNumber } from "./bands"
import { SpectrumChart } from "./SpectrumChart"
import { useSpectrum, type SpectrumPoint, type SpectrumRequest } from "./useSpectrum"
import "./spectrum-panel.css"

export type SidebarSpectrumProps = SpectrumRequest

/** The clicked-pixel spectrum for the sidebar: chart first, quality status next,
 * then the full detector/band table collapsed under Band measurements. */
export function SidebarSpectrum(props: SidebarSpectrumProps) {
  const state = useSpectrum(props)
  const hasPoint = props.point !== null

  return (
    <section className="spectrum-panel" aria-label="Spectrum">
      <div className="spectrum-panel-head">
        <h2>Spectrum</h2>
        {props.release ? <span className="spectrum-panel-release">{props.release}</span> : null}
      </div>
      {props.point ? (
        <dl className="spectrum-panel-meta">
          <div>
            <dt>Right ascension</dt>
            <dd>{coordinate(props.point.ra)}°</dd>
          </div>
          <div>
            <dt>Declination</dt>
            <dd>{coordinate(props.point.dec)}°</dd>
          </div>
          <div>
            <dt>Pixel</dt>
            <dd>
              {props.point.x}, {props.point.y}
            </dd>
          </div>
        </dl>
      ) : null}
      {!hasPoint ? <p className="spectrum-panel-state">Click the image to sample a spectrum</p> : null}
      {hasPoint && (state.status === "loading" || state.status === "idle") ? (
        <p className="spectrum-panel-state" role="status" aria-busy="true">
          Reading the pixel…
        </p>
      ) : null}
      {hasPoint && state.status === "failed" ? (
        <p className="spectrum-panel-error" role="alert">
          {state.error}
        </p>
      ) : null}
      {hasPoint && state.status === "ready" && state.body.points.length === 0 ? (
        <p className="spectrum-panel-state" role="status">
          No bands have a sample at this pixel.
        </p>
      ) : null}
      {hasPoint && state.status === "ready" && state.body.points.length > 0 ? (
        <SpectrumResult points={state.body.points} meaning={state.body.meaning} />
      ) : null}
    </section>
  )
}

function SpectrumResult(props: { points: SpectrumPoint[]; meaning: string }) {
  const valid = props.points.filter((point) => !point.invalid && point.flux != null && Number.isFinite(point.flux))
  const flaggedFinite = props.points.filter(
    (point) => point.invalid && point.flux != null && Number.isFinite(point.flux),
  )
  const total = props.points.length
  const summaryCounts = `${valid.length} valid · ${total - valid.length} flagged`

  return (
    <>
      <SpectrumChart points={props.points} />
      {valid.length === 0 ? (
        <p className="spectrum-panel-quality" data-tone="empty" role="status">
          No valid measurements. All {total} bands are flagged
          {flaggedFinite.length > 0
            ? `; ${flaggedFinite.length} finite rejected values are shown as untrusted diagnostics only.`
            : "."}
        </p>
      ) : (
        <p className="spectrum-panel-quality" data-tone="ok" role="status">
          {valid.length} of {total} bands valid
          {total - valid.length > 0 ? `, ${total - valid.length} flagged` : ""}. Flagged values are untrusted
          diagnostics only.
        </p>
      )}
      <p className="spectrum-panel-caption">{props.meaning}</p>
      <details className="spectrum-panel-details">
        <summary>
          <span className="spectrum-panel-details-title">Band measurements</span>{" "}
          <span className="spectrum-panel-details-counts">{summaryCounts}</span>
        </summary>
        <div className="spectrum-panel-table-wrap">
          <table className="spectrum-panel-table">
            <caption>Detector bands at this pixel · MJy/sr = sky brightness</caption>
            <thead>
              <tr>
                <th scope="col">Band</th>
                <th scope="col">Detector</th>
                <th scope="col">Wavelength (µm)</th>
                <th scope="col">Flux (MJy/sr)</th>
                <th scope="col">Error (MJy/sr)</th>
                <th scope="col">Flags</th>
              </tr>
            </thead>
            <tbody>
              {props.points.map((point, index) => (
                <tr key={`${point.band ?? "loose"}-${index}`} className={point.invalid ? "flagged" : undefined}>
                  <td className="num">{bandNumber(point.band)}</td>
                  <td className="num">{detectorNumber(point.band)}</td>
                  <td className="num">{point.wavelength_um.toFixed(3)}</td>
                  <td className="num">{measurement(point.flux)}</td>
                  <td className="num">{measurement(point.error)}</td>
                  <td className="flags">{flagCell(point)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </details>
    </>
  )
}

function flagCell(point: SpectrumPoint): string {
  if (point.flags.length > 0) return point.flags.join(", ")
  if (point.invalid) return "Flagged"
  return "None"
}

function measurement(value: number | null | undefined): string {
  if (value == null || !Number.isFinite(value)) return "No data"
  return value.toPrecision(3)
}

function coordinate(value: number): string {
  return Number.isFinite(value) ? value.toFixed(4) : "No data"
}
