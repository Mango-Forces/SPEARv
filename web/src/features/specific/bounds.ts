import { parseUtcText } from "./date.ts"

export type Bounds = {
  fromUtc: string | null
  toUtc: string | null
  releases: string[]
  bands: number[]
}

export function parseBands(expression: string): number[] {
  const bands = expression.split(";").flatMap((part) => {
    const match = /^\s*(\d+)\s*(?:-\s*(\d+)\s*)?$/.exec(part)
    if (!match) throw new Error("Use band numbers or inclusive ranges separated by semicolons, e.g. 1-8;44.")
    const first = Number(match[1])
    const last = Number(match[2] ?? match[1])
    if (first < 1 || last > 102 || first > last) throw new Error("Bands must be ascending integers from 1 to 102, e.g. 1-8;44.")
    return Array.from({ length: last - first + 1 }, (_, index) => first + index)
  })
  return [...new Set(bands)].sort((a, b) => a - b)
}

export function formatBands(bands: readonly number[]): string {
  const sorted = [...new Set(bands)].sort((a, b) => a - b)
  return sorted.flatMap((band, index) => {
    if (index > 0 && sorted[index - 1] === band - 1) return []
    const end = sorted.slice(index).findIndex((value, offset) => value !== band + offset)
    const last = sorted[end < 0 ? sorted.length - 1 : index + end - 1]
    return [last === band ? String(band) : `${band}-${last}`]
  }).join(";")
}

/** Accept canonical ISO links and fixed English-month UTC text; retain ISO milliseconds. */
export function parseUtc(value: string, day: "start" | "end" = "start"): string | null {
  return parseUtcText(value, day)
}

export function parseBounds(draft: { fromUtc: string; toUtc: string; releases: string[]; bands: string }): Bounds {
  const fromUtc = parseUtc(draft.fromUtc)
  const toUtc = parseUtc(draft.toUtc, "end")
  if (fromUtc && toUtc && fromUtc > toUtc) throw new Error("UTC start must not be after finish.")
  if (draft.releases.length === 0 || draft.releases.some((release) => release !== "qr2" && release !== "qr3")) {
    throw new Error("Select QR2, QR3, or both.")
  }
  return { fromUtc, toUtc, releases: [...new Set(draft.releases)].sort(), bands: parseBands(draft.bands) }
}
