import { formatBands, parseBands } from "../specific/bounds.ts"
import type { Pin } from "../../url-state/view.ts"

export type CompositeRangeCommit = { mode: "composite" } | { mode: "range"; pins: Pin[] }

const ALL_MARKERS = new Set(["", "all", "1-102"])

/** Inclusive UI intervals become the zero-based engine pin shape.
 * Empty / "all" / "1-102" means the unfiltered composite stack. */
export function rangeCommit(expression: string): CompositeRangeCommit {
  const trimmed = expression.trim().toLowerCase()
  if (ALL_MARKERS.has(trimmed)) return { mode: "composite" }
  const canonical = formatBands(parseBands(expression))
  if (canonical === "1-102") return { mode: "composite" }
  return {
    mode: "range",
    pins: canonical.split(";").map((interval) => {
      const [lo, last] = interval.split("-").map(Number)
      const hi = last ?? lo
      const width = hi - lo + 1
      return { center: lo - 1 + Math.floor((width - 1) / 2), width }
    }),
  }
}
