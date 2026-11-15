import type { Bounds } from "./bounds.ts"
import { formatNumber } from "../../url-state/position.ts"

export type EpochVisit = { index: number; mjd: number; mjd_end: number; exposures: number }

export type ExposureQuality = {
  dqa_l1?: string | null
  dqa_l2?: string | null
  psf_fwhm?: number | null
  finast?: number | null
  aurora?: boolean | null
  airglow?: boolean | null
  shuttle_glow?: boolean | null
  n_streaks?: number | null
  src_frac?: number | null
}

export type ExposureRow = {
  id: string
  obsid: string
  detector: number
  version: string
  mjd: number
  visit: number | null
  wl_min_um: number
  wl_max_um: number
  quality?: ExposureQuality | null
}

export const DETECTORS = [1, 2, 3, 4, 5, 6] as const
const BANDS_PER_DETECTOR = 17

/** Interface band numbers (1-102) of one detector array. */
export function detectorBands(detector: number): number[] {
  return Array.from({ length: BANDS_PER_DETECTOR }, (_, index) => (detector - 1) * BANDS_PER_DETECTOR + index + 1)
}

/** Interface bands (1-102) → engine 0-based band indices. */
export function engineBands(bands: readonly number[]): number[] {
  return [...new Set(bands)].map((band) => band - 1).sort((a, b) => a - b)
}

export function mjdToMs(mjd: number): number {
  return (mjd - 40587) * 86400000
}

export function visitsInBounds(visits: readonly EpochVisit[], bounds: Pick<Bounds, "fromUtc" | "toUtc">): EpochVisit[] {
  const from = bounds.fromUtc ? Date.parse(bounds.fromUtc) : -Infinity
  const to = bounds.toUtc ? Date.parse(bounds.toUtc) : Infinity
  return visits.filter((visit) => mjdToMs(visit.mjd_end) >= from && mjdToMs(visit.mjd) <= to)
}

export function exposuresInBounds(rows: readonly ExposureRow[], bounds: Pick<Bounds, "fromUtc" | "toUtc">): ExposureRow[] {
  const from = bounds.fromUtc ? Date.parse(bounds.fromUtc) : -Infinity
  const to = bounds.toUtc ? Date.parse(bounds.toUtc) : Infinity
  return rows.filter((row) => {
    const ms = mjdToMs(row.mjd)
    return ms >= from && ms <= to
  })
}

/** Body of `POST /api/v1/frame`. */
export function frameBody(
  view: { ra: number; dec: number; fovL: number; fovH: number; dr: string; mask: boolean },
  bands: readonly number[],
  selection: { visit: number } | { exposures: string[] },
) {
  return {
    ra: view.ra,
    dec: view.dec,
    fov_l: view.fovL,
    fov_h: view.fovH,
    dr: view.dr,
    mask: view.mask,
    bands: engineBands(bands),
    ...("visit" in selection ? { visit: selection.visit } : { exposures: selection.exposures }),
  }
}

export function stepReady(ready: readonly number[], from: number, direction: 1 | -1): number {
  if (ready.length === 0) return from
  const later = direction === 1 ? ready.find((index) => index > from) : [...ready].reverse().find((index) => index < from)
  return later ?? (direction === 1 ? ready[0]! : ready[ready.length - 1]!)
}

/** One sector's exposure ids, ordered by time. */
export function groupExposuresByVisit(rows: readonly ExposureRow[]): Map<number, string[]> {
  const groups = new Map<number, string[]>()
  for (const row of rows) {
    if (row.visit === null || row.visit === undefined) continue
    const list = groups.get(row.visit) ?? []
    list.push(row.id)
    groups.set(row.visit, list)
  }
  return groups
}

export function qualityFlags(quality: ExposureQuality | null | undefined): string[] {
  if (!quality) return []
  const flags: string[] = []
  if (quality.dqa_l1 && quality.dqa_l1.toLowerCase() !== "pass") flags.push(`L1 ${quality.dqa_l1}`)
  if (quality.dqa_l2 && quality.dqa_l2.toLowerCase() !== "pass") flags.push(`L2 ${quality.dqa_l2}`)
  if (quality.aurora) flags.push("aurora")
  if (quality.airglow) flags.push("airglow")
  if (quality.shuttle_glow) flags.push("shuttle glow")
  if ((quality.n_streaks ?? 0) > 0) flags.push(`${quality.n_streaks} streaks`)
  if (quality.finast !== null && quality.finast !== undefined && quality.finast !== 0) flags.push(`finast ${quality.finast}`)
  return flags
}

export function formatMjdDate(mjd: number): string {
  return new Date(mjdToMs(mjd)).toLocaleDateString("en-GB", {
    day: "numeric",
    month: "short",
    year: "numeric",
    timeZone: "UTC",
  })
}

/** Stable label for a sky region used only in debug and tests. */
export function regionKey(view: { ra: number; dec: number; fovL: number; fovH: number; dr: string }): string {
  return [formatNumber(view.ra), formatNumber(view.dec), formatNumber(view.fovL), formatNumber(view.fovH), view.dr].join(":")
}
