import { detectorNumber } from "./bands"
import type { SpectrumPoint } from "./useSpectrum"

const WIDTH = 380
const HEIGHT = 190
const LEFT = 48
const RIGHT = 10
const TOP = 12
const BOTTOM = 30

/** Plot the reduced spectrum. Valid bands carry a 1σ error bar and are joined
 * as a spectrum; rejected bands keep whatever finite flux the server reported
 * as isolated untrusted diagnostics, never connected. */
export function SpectrumChart(props: { points: SpectrumPoint[] }) {
  const plotted = props.points.flatMap((point) => {
    const flux = point.flux
    if (flux == null || !Number.isFinite(flux)) return []
    return [{ point, flux }]
  })
  if (plotted.length === 0) {
    return (
      <p className="spectrum-panel-note" role="status">
        No finite flux at this pixel. Open Band measurements below for the per-band flags.
      </p>
    )
  }

  const measured = plotted
    .filter(({ point }) => !point.invalid)
    .slice()
    .sort((a, b) => a.point.wavelength_um - b.point.wavelength_um)
  const flagged = plotted.filter(({ point }) => point.invalid)
  const wavelengths = props.points.map((point) => point.wavelength_um).filter(Number.isFinite)
  const minWl = Math.min(...wavelengths)
  const maxWl = Math.max(...wavelengths)
  const wlSpan = maxWl - minWl || 1
  const wlLow = minWl - wlSpan * 0.02
  const wlHigh = maxWl + wlSpan * 0.02

  const lows = measured.map(({ point, flux }) => flux - (point.error ?? 0))
  const highs = measured.map(({ point, flux }) => flux + (point.error ?? 0))
  const fluxLow = Math.min(0, ...lows, ...plotted.map(({ flux }) => flux))
  const fluxHigh = Math.max(0.000001, ...highs, ...plotted.map(({ flux }) => flux))
  const fluxPad = (fluxHigh - fluxLow) * 0.06 || 1
  const yLow = fluxLow - fluxPad
  const yHigh = fluxHigh + fluxPad

  const xOf = (wavelength: number) => LEFT + ((wavelength - wlLow) / (wlHigh - wlLow)) * (WIDTH - LEFT - RIGHT)
  const yOf = (flux: number) => HEIGHT - BOTTOM - ((flux - yLow) / (yHigh - yLow)) * (HEIGHT - TOP - BOTTOM)
  const xTicks = Array.from({ length: 5 }, (_, index) => minWl + ((maxWl - minWl) * index) / 4)
  const yTicks = Array.from({ length: 3 }, (_, index) => yLow + ((yHigh - yLow) * index) / 2)
  const wlDigits = maxWl - minWl < 0.1 ? 3 : 2

  return (
    <>
      <svg
        className="spectrum-panel-chart"
        viewBox={`0 0 ${WIDTH} ${HEIGHT}`}
        role="img"
        aria-label={`Flux against wavelength from ${minWl.toFixed(2)} to ${maxWl.toFixed(2)} micrometres, ${measured.length} valid, ${flagged.length} flagged untrusted`}
      >
        {yTicks.map((tick, index) => (
          <g key={`y-${index}`}>
            <line className="grid" x1={LEFT} x2={WIDTH - RIGHT} y1={yOf(tick)} y2={yOf(tick)} />
            <text x={LEFT - 6} y={yOf(tick) + 3} textAnchor="end">
              {axisNumber(tick)}
            </text>
          </g>
        ))}
        {xTicks.map((tick, index) => (
          <g key={`x-${index}`}>
            <line className="tick" x1={xOf(tick)} x2={xOf(tick)} y1={HEIGHT - BOTTOM} y2={HEIGHT - BOTTOM + 4} />
            <text x={xOf(tick)} y={HEIGHT - BOTTOM + 14} textAnchor="middle">
              {tick.toFixed(wlDigits)}
            </text>
          </g>
        ))}
        <line className="axis-line" x1={LEFT} x2={WIDTH - RIGHT} y1={HEIGHT - BOTTOM} y2={HEIGHT - BOTTOM} />
        <line className="axis-line" x1={LEFT} x2={LEFT} y1={TOP} y2={HEIGHT - BOTTOM} />
        {measured.length >= 2 ? (
          <polyline
            className="valid-line"
            fill="none"
            points={measured.map(({ point, flux }) => `${xOf(point.wavelength_um)},${yOf(flux)}`).join(" ")}
          />
        ) : null}
        {measured.map(({ point, flux }, index) => {
          const x = xOf(point.wavelength_um)
          const y = yOf(flux)
          const error = point.error
          return (
            <g key={`measured-${index}`}>
              <title>{pointText(point, flux)}</title>
              {error != null && Number.isFinite(error) && error > 0 ? (
                <line className="error-bar" x1={x} x2={x} y1={yOf(flux - error)} y2={yOf(flux + error)} />
              ) : null}
              <circle className="measured" cx={x} cy={y} r={2.4} />
            </g>
          )
        })}
        {flagged.map(({ point, flux }, index) => (
          <g key={`flagged-${index}`}>
            <title>{`${pointText(point, flux)} (untrusted diagnostic)`}</title>
            <rect className="flagged" x={xOf(point.wavelength_um) - 2.6} y={yOf(flux) - 2.6} width={5.2} height={5.2} />
          </g>
        ))}
        <text className="axis-label" x={(LEFT + WIDTH - RIGHT) / 2} y={HEIGHT - 4} textAnchor="middle">
          Wavelength (µm)
        </text>
        <text className="axis-label" x={LEFT} y={TOP - 2}>
          MJy/sr
        </text>
      </svg>
      <ul className="spectrum-panel-legend" aria-label="Chart legend">
        <li>
          <span className="swatch measured" aria-hidden="true" />
          Valid · {measured.length}
        </li>
        <li>
          <span className="swatch flagged" aria-hidden="true" />
          Flagged, untrusted · {flagged.length}
        </li>
      </ul>
    </>
  )
}

function pointText(point: SpectrumPoint, flux: number): string {
  const band = point.band == null ? "no band" : `band ${point.band + 1}, detector ${detectorNumber(point.band)}`
  const error = point.error == null ? "" : `, error ${point.error.toPrecision(3)}`
  const flags = point.flags.length > 0 ? `, ${point.flags.join(", ")}` : point.invalid ? ", flagged" : ""
  return `${band}, ${point.wavelength_um.toFixed(3)} µm, ${flux.toPrecision(3)} MJy/sr${error}${flags}`
}

function axisNumber(value: number): string {
  const size = Math.abs(value)
  if (size >= 100) return value.toFixed(0)
  if (size >= 10) return value.toFixed(1)
  return value.toFixed(2)
}
