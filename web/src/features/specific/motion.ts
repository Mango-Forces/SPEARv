/** Multi-epoch motion search, the catalog proper-motion method surveys use:
 * detect sources in every epoch, match them across epochs, and fit position
 * against time. A two-frame difference cannot tell a mover from a PSF or
 * registration residual; a straight track through every epoch can.
 *
 * Grid convention matches `skyAtPixel`: north up, east left, 6.15″ pixels. */

import type { FramePlane } from "./calibrate"

export const PIXEL_ARCSEC = 6.15

export type Source = { x: number; y: number; snr: number; saturated: boolean }

export type EpochSources = { years: number; sources: Source[] }

export type Mover = {
  /** Fitted position at the newest tracked epoch, grid pixels. */
  x: number
  y: number
  /** Fitted track: one point per matched epoch, grid pixels. */
  track: { years: number; x: number; y: number }[]
  /** Proper motion, arcsec per year. `pmRa` is RA·cos(Dec), positive east. */
  pmRa: number
  pmDec: number
  pm: number
  /** One-sigma uncertainty of `pm` from the fit residuals, arcsec per year. */
  error: number
  /** pm / error, with the error floored so a perfect fit does not explode. */
  significance: number
  /** `candidate` at 4σ and above; `possible` between 3σ and 4σ. */
  tier: "candidate" | "possible"
  epochs: number
  snr: number
  saturated: boolean
}

export type MotionSearch = {
  movers: Mover[]
  /** Sources tracked through enough epochs to fit. */
  tracked: number
  /** Median fitted motion of every tracked source: the field's noise floor. */
  floor: number
  epochs: number
}

const DETECT_SIGMA = 6
/** A peak this strong beside a masked hole is the rim of a saturated core. */
const SATURATED_SNR = 20
const MATCH_PX = 3
const ERROR_FLOOR = 0.3
/** At or above this a track is a candidate; between POSSIBLE and this, a weak one. */
const MIN_SIGNIFICANCE = 4
const POSSIBLE_SIGNIFICANCE = 3
const MIN_PM = 2
const FLOOR_FACTOR = 3
const MAX_SOURCES = 4000
/** Fewer points leave one degree of freedom, and the fit error means nothing. */
export const MIN_EPOCHS = 4
/** A faint source this close to a star this much brighter sits in its wing. */
const HALO_PX = 10
const HALO_RATIO = 50

/** Find movers across planes taken at `years` (any zero point). */
export function searchMotion(epochs: { years: number; plane: FramePlane }[]): MotionSearch {
  const detected = epochs
    .map((epoch) => ({ years: epoch.years, sources: detectSources(epoch.plane) }))
    .sort((a, b) => a.years - b.years)
  return trackSources(detected)
}

/** Fit tracks for sources already detected per epoch. */
export function trackSources(detected: EpochSources[]): MotionSearch {
  if (detected.length < MIN_EPOCHS) return { movers: [], tracked: 0, floor: 0, epochs: detected.length }
  const reference = detected.reduce((best, epoch) => (epoch.sources.length > best.sources.length ? epoch : best))
  const grids = detected.map((epoch) => bucket(epoch.sources))
  const fits = reference.sources.flatMap((source) => {
    const track = detected.flatMap((epoch, at) => {
      const match = nearest(grids[at]!, source.x, source.y)
      return match ? [{ years: epoch.years, x: match.x, y: match.y }] : []
    })
    if (track.length < MIN_EPOCHS) return []
    const fx = fitLine(track.map((point) => point.years), track.map((point) => point.x))
    const fy = fitLine(track.map((point) => point.years), track.map((point) => point.y))
    // East is left and north is up, so both axes flip sign.
    const pmRa = -fx.slope * PIXEL_ARCSEC
    const pmDec = -fy.slope * PIXEL_ARCSEC
    const pm = Math.hypot(pmRa, pmDec)
    const error = Math.hypot(fx.error, fy.error) * PIXEL_ARCSEC
    const last = track[track.length - 1]!.years
    return [
      {
        x: fx.at(last),
        y: fy.at(last),
        track: track.map((point) => ({ years: point.years, x: fx.at(point.years), y: fy.at(point.years) })),
        pmRa,
        pmDec,
        pm,
        error,
        significance: pm / Math.max(error, ERROR_FLOOR),
        tier: pm / Math.max(error, ERROR_FLOOR) >= MIN_SIGNIFICANCE ? ("candidate" as const) : ("possible" as const),
        epochs: track.length,
        snr: source.snr,
        saturated: source.saturated,
      },
    ]
  })
  const floor = median(fits.map((fit) => fit.pm))
  const movers = fits
    .filter((fit) => fit.significance >= POSSIBLE_SIGNIFICANCE && fit.pm >= Math.max(MIN_PM, floor * FLOOR_FACTOR))
    .filter((fit) => !inHalo(fit, reference.sources))
    .sort((a, b) => b.significance - a.significance)
  return { movers, tracked: fits.length, floor, epochs: detected.length }
}

function inHalo(fit: { x: number; y: number; snr: number }, sources: Source[]): boolean {
  return sources.some(
    (other) => other.snr >= fit.snr * HALO_RATIO && Math.hypot(other.x - fit.x, other.y - fit.y) < HALO_PX,
  )
}

/** Local maxima above the robust noise, with a 3×3 flux centroid. A star
 * whose core is masked (saturated) rings with several peaks, so those merge
 * into one source with a wide centroid instead. */
export function detectSources(plane: FramePlane): Source[] {
  const { width: w, height: h, values: v } = plane
  const { median: med, sigma } = robustStats(v)
  if (!(sigma > 0)) return []
  const normal: Source[] = []
  const saturated: { peak: number; x: number; y: number }[] = []
  for (let y = 1; y < h - 1; y++) {
    for (let x = 1; x < w - 1; x++) {
      const centre = v[y * w + x]!
      if (!Number.isFinite(centre) || centre - med < DETECT_SIGMA * sigma) continue
      let finite = 0
      let isPeak = true
      for (let dy = -1; dy <= 1 && isPeak; dy++) {
        for (let dx = -1; dx <= 1; dx++) {
          const q = v[(y + dy) * w + x + dx]!
          if (!Number.isFinite(q)) continue
          finite += 1
          if (q > centre) {
            isPeak = false
            break
          }
        }
      }
      if (!isPeak || finite < 5) continue
      const snr = (centre - med) / sigma
      if (snr > SATURATED_SNR && maskedNear(plane, x, y, 2)) {
        saturated.push({ peak: centre, x, y })
        continue
      }
      const c = gaussianPeak(plane, x, y, med) ?? centroid(plane, x, y, 1, med, 0)
      if (c) normal.push({ x: c.x, y: c.y, snr, saturated: false })
    }
  }
  // Group ring peaks by the masked hole they touch; one hole is one star.
  const holes = saturated.length ? maskedHoles(plane) : undefined
  const cores = new Map<number, { peak: number; hole: Hole }>()
  for (const peak of saturated) {
    const label = holes ? holeNear(holes, plane, peak.x, peak.y, 2) : undefined
    if (label === undefined) continue
    const seen = cores.get(label)
    if (!seen || peak.peak > seen.peak) cores.set(label, { peak: peak.peak, hole: holes!.list[label]! })
  }
  const merged = [...cores.values()].flatMap((core) => {
    const reach = Math.ceil(core.hole.radius) + 3
    const c = centroid(plane, Math.round(core.hole.x), Math.round(core.hole.y), reach, med, 0.25 * (core.peak - med))
    return c ? [{ x: c.x, y: c.y, snr: (core.peak - med) / sigma, saturated: true }] : []
  })
  const kept = normal.filter((source) => cores.size === 0 || merged.every((core) => Math.hypot(core.x - source.x, core.y - source.y) >= 7))
  return [...kept, ...merged].sort((a, b) => b.snr - a.snr).slice(0, MAX_SOURCES)
}

type Hole = { x: number; y: number; radius: number }

/** 4-connected components of masked pixels, with centre and size. */
function maskedHoles(plane: FramePlane): { labels: Int32Array; list: Hole[] } {
  const { width: w, height: h, values: v } = plane
  const labels = new Int32Array(w * h).fill(-1)
  const list: Hole[] = []
  const queue: number[] = []
  for (let start = 0; start < v.length; start++) {
    if (Number.isFinite(v[start]!) || labels[start] !== -1) continue
    const label = list.length
    let sx = 0
    let sy = 0
    let n = 0
    labels[start] = label
    queue.push(start)
    while (queue.length) {
      const at = queue.pop()!
      const x = at % w
      const y = (at - x) / w
      sx += x
      sy += y
      n += 1
      for (const next of [x > 0 ? at - 1 : -1, x < w - 1 ? at + 1 : -1, y > 0 ? at - w : -1, y < h - 1 ? at + w : -1]) {
        if (next < 0 || labels[next] !== -1 || Number.isFinite(v[next]!)) continue
        labels[next] = label
        queue.push(next)
      }
    }
    list.push({ x: sx / n, y: sy / n, radius: Math.sqrt(n / Math.PI) })
  }
  return { labels, list }
}

function holeNear(holes: { labels: Int32Array }, plane: FramePlane, x: number, y: number, radius: number): number | undefined {
  const { width: w, height: h } = plane
  for (let yy = Math.max(0, y - radius); yy <= Math.min(h - 1, y + radius); yy++) {
    for (let xx = Math.max(0, x - radius); xx <= Math.min(w - 1, x + radius); xx++) {
      const label = holes.labels[yy * w + xx]!
      if (label !== -1) return label
    }
  }
  return undefined
}

/** Three-point Gaussian peak fit on each axis. A plain centroid pulls toward
 * the brightest pixel (pixel-locking), which shrinks a slow track. */
function gaussianPeak(plane: FramePlane, x: number, y: number, med: number) {
  const { width: w, values: v } = plane
  const at = (xx: number, yy: number) => v[yy * w + xx]! - med
  const offset = (left: number, centre: number, right: number) => {
    if (!(left > 0 && centre > 0 && right > 0)) return undefined
    const l = Math.log(left)
    const c = Math.log(centre)
    const r = Math.log(right)
    const curve = l - 2 * c + r
    if (!(curve < 0)) return undefined
    const shift = (l - r) / (2 * curve)
    return Math.abs(shift) <= 0.5 ? shift : undefined
  }
  const dx = offset(at(x - 1, y), at(x, y), at(x + 1, y))
  const dy = offset(at(x, y - 1), at(x, y), at(x, y + 1))
  return dx === undefined || dy === undefined ? undefined : { x: x + dx, y: y + dy }
}

function centroid(plane: FramePlane, x: number, y: number, radius: number, med: number, cut: number) {
  const { width: w, height: h, values: v } = plane
  let wx = 0
  let wy = 0
  let ww = 0
  for (let yy = Math.max(0, y - radius); yy <= Math.min(h - 1, y + radius); yy++) {
    for (let xx = Math.max(0, x - radius); xx <= Math.min(w - 1, x + radius); xx++) {
      const q = v[yy * w + xx]! - med
      if (!Number.isFinite(q) || q <= cut) continue
      wx += xx * q
      wy += yy * q
      ww += q
    }
  }
  return ww > 0 ? { x: wx / ww, y: wy / ww } : undefined
}

function maskedNear(plane: FramePlane, x: number, y: number, radius: number): boolean {
  const { width: w, height: h, values: v } = plane
  for (let yy = Math.max(0, y - radius); yy <= Math.min(h - 1, y + radius); yy++) {
    for (let xx = Math.max(0, x - radius); xx <= Math.min(w - 1, x + radius); xx++) {
      if (!Number.isFinite(v[yy * w + xx]!)) return true
    }
  }
  return false
}

/** Median and MAD sigma of the finite samples. */
export function robustStats(values: Float32Array): { median: number; sigma: number } {
  const finite = Float32Array.from(values.filter((value) => Number.isFinite(value))).sort()
  if (finite.length === 0) return { median: 0, sigma: 0 }
  const med = finite[finite.length >> 1]!
  const deviations = finite.map((value) => Math.abs(value - med)).sort()
  return { median: med, sigma: 1.4826 * deviations[deviations.length >> 1]! }
}

function fitLine(ts: number[], vs: number[]) {
  const n = ts.length
  const mt = ts.reduce((a, b) => a + b, 0) / n
  const mv = vs.reduce((a, b) => a + b, 0) / n
  const sxx = ts.reduce((acc, t) => acc + (t - mt) ** 2, 0)
  const slope = sxx > 0 ? ts.reduce((acc, t, i) => acc + (t - mt) * (vs[i]! - mv), 0) / sxx : 0
  const at = (t: number) => mv + slope * (t - mt)
  const residual = ts.reduce((acc, t, i) => acc + (vs[i]! - at(t)) ** 2, 0)
  const rms = Math.sqrt(residual / Math.max(1, n - 2))
  return { slope, at, error: n > 2 && sxx > 0 ? rms / Math.sqrt(sxx) : Infinity }
}

type Grid = Map<string, Source[]>

function bucket(sources: Source[]): Grid {
  const grid: Grid = new Map()
  for (const source of sources) {
    const key = `${Math.floor(source.x / MATCH_PX)},${Math.floor(source.y / MATCH_PX)}`
    const list = grid.get(key)
    if (list) list.push(source)
    else grid.set(key, [source])
  }
  return grid
}

function nearest(grid: Grid, x: number, y: number): Source | undefined {
  const cx = Math.floor(x / MATCH_PX)
  const cy = Math.floor(y / MATCH_PX)
  let best: Source | undefined
  let bestDistance = MATCH_PX
  for (let dy = -1; dy <= 1; dy++) {
    for (let dx = -1; dx <= 1; dx++) {
      for (const source of grid.get(`${cx + dx},${cy + dy}`) ?? []) {
        const distance = Math.hypot(source.x - x, source.y - y)
        if (distance < bestDistance) {
          best = source
          bestDistance = distance
        }
      }
    }
  }
  return best
}

function median(values: number[]): number {
  if (values.length === 0) return 0
  const sorted = [...values].sort((a, b) => a - b)
  return sorted[sorted.length >> 1]!
}

/** Compass direction of a motion, east positive in `pmRa`. */
export function heading(pmRa: number, pmDec: number): string {
  const angle = (Math.atan2(pmRa, pmDec) * 180) / Math.PI
  const names = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"]
  return names[(Math.round(((angle + 360) % 360) / 45) % 8)]!
}
