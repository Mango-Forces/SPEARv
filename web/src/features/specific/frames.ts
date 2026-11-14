import type { Bounds } from "./bounds.ts"
import { formatNumber } from "../../url-state/position.ts"

/** One pass of SPHEREx over the field, as the coverage route reports it. */
export type EpochVisit = { index: number; mjd: number; mjd_end: number; exposures: number }

export const DETECTORS = [1, 2, 3, 4, 5, 6] as const
const BANDS_PER_DETECTOR = 17

/** Interface band numbers (1-102) of one detector array. */
export function detectorBands(detector: number): number[] {
  return Array.from({ length: BANDS_PER_DETECTOR }, (_, index) => (detector - 1) * BANDS_PER_DETECTOR + index + 1)
}

export function mjdToMs(mjd: number): number {
  return (mjd - 40587) * 86400000
}

/** Visits that overlap the applied UTC window. Open ends keep everything. */
export function visitsInBounds(visits: readonly EpochVisit[], bounds: Pick<Bounds, "fromUtc" | "toUtc">): EpochVisit[] {
  const from = bounds.fromUtc ? Date.parse(bounds.fromUtc) : -Infinity
  const to = bounds.toUtc ? Date.parse(bounds.toUtc) : Infinity
  return visits.filter((visit) => mjdToMs(visit.mjd_end) >= from && mjdToMs(visit.mjd) <= to)
}

/** Interface bands (1-102) as engine pins (0-based centre, width): one pin per
 * consecutive run, centred the way the engine expands a pin. */
export function bandPins(bands: readonly number[]): { center: number; width: number }[] {
  const sorted = [...new Set(bands)].sort((a, b) => a - b)
  return sorted.flatMap((band, index) => {
    if (index > 0 && sorted[index - 1] === band - 1) return []
    const end = sorted.slice(index).findIndex((value, offset) => value !== band + offset)
    const width = end < 0 ? sorted.length - index : end
    return [{ center: band - 1 + Math.floor((width - 1) / 2), width }]
  })
}

/** The render query for one epoch: the chosen bands, stacked over one visit only. */
export function frameSearch(
  view: { ra: number; dec: number; fovL: number; fovH: number; dr: string; mask: boolean },
  bands: readonly number[],
  visit: number,
): string {
  const params = new URLSearchParams()
  params.set("ra", formatNumber(view.ra))
  params.set("dec", formatNumber(view.dec))
  params.set("fov_l", formatNumber(view.fovL))
  params.set("fov_h", formatNumber(view.fovH))
  params.set("dr", view.dr)
  params.set("mask", view.mask ? "on" : "off")
  params.set("mode", "range")
  bandPins(bands).forEach((pin, index) => {
    params.append("pin", index === 0 ? `${pin.center}:${pin.width}:v${visit}:` : `${pin.center}:${pin.width}`)
  })
  return params.toString()
}

/** Next ready frame after `from`, wrapping. `from` itself when nothing else is ready. */
export function stepReady(ready: readonly number[], from: number, direction: 1 | -1): number {
  if (ready.length === 0) return from
  const later = direction === 1 ? ready.find((index) => index > from) : [...ready].reverse().find((index) => index < from)
  return later ?? (direction === 1 ? ready[0] : ready[ready.length - 1])
}
