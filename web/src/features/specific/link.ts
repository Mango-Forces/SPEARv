import { formatBands, type Bounds } from "./bounds.ts"
import { formatNumber } from "../../url-state/position.ts"

/** The canonical all-bounds value. Specific copy links omit the whole bounds set
 * when every field is still at this default. */
export const DEFAULT_BOUNDS: Bounds = {
  fromUtc: null,
  toUtc: null,
  releases: ["dr1", "qr2", "qr3"],
  bands: Array.from({ length: 102 }, (_, index) => index + 1),
}

/** Canonical value identity, independent of object identity or field order. */
export function boundsKey(bounds: Bounds): string {
  return JSON.stringify([
    bounds.fromUtc,
    bounds.toUtc,
    [...bounds.releases].sort(),
    [...bounds.bands].sort((first, second) => first - second),
  ])
}

export function isDefaultBounds(bounds: Bounds): boolean {
  return boundsKey(bounds) === boundsKey(DEFAULT_BOUNDS)
}

/** Emit only the canonical bounds set. The whole set is omitted only for the
 * default all-bounds value; open ends are omitted, while releases and bands are
 * always explicit once any bound is non-default. */
export function specificBoundsParams(bounds: Bounds): Array<[string, string]> {
  if (isDefaultBounds(bounds)) return []
  const params: Array<[string, string]> = []
  if (bounds.fromUtc) params.push(["from_utc", bounds.fromUtc])
  if (bounds.toUtc) params.push(["to_utc", bounds.toUtc])
  params.push(["releases", [...new Set(bounds.releases)].sort().join(",")])
  params.push(["bands", formatBands([...new Set(bounds.bands)])])
  return params
}

/** Specific copy links carry the location and applied bounds only; overlay,
 * colour and display parameters stay out. */
export function specificViewSearch(input: { ra: number; dec: number; fovL: number; fovH: number; bounds: Bounds }): string {
  const params = new URLSearchParams()
  params.set("ra", formatNumber(input.ra))
  params.set("dec", formatNumber(input.dec))
  params.set("fov_l", formatNumber(input.fovL))
  params.set("fov_h", formatNumber(input.fovH))
  params.set("mode", "specific")
  for (const [key, value] of specificBoundsParams(input.bounds)) params.set(key, value)
  return params.toString()
}
