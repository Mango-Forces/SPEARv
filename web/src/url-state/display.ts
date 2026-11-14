import type { View } from "./view.ts"

export type Stretch = { kind: "linear" } | { kind: "asinh" } | { kind: "gamma"; gamma: number }
export type BlackWhite = { mode: "auto" } | { mode: "manual"; low: number; high: number }
export type LayerDisplay = { enabled: boolean; tint: string; gain: number; blackWhite: BlackWhite; stretch: Stretch }
export type DisplayState = {
  color: { r: LayerDisplay; g: LayerDisplay; b: LayerDisplay }
  composite: { gain: number; blackWhite: BlackWhite; stretch: Stretch; invert: boolean }
}
export type ResolvedLayer = { channel: number; enabled: boolean; tint: readonly number[]; gain: number; low: number | null; span: number | null; stretch: Stretch }
export type ResolvedDisplay = { layers: ResolvedLayer[]; lift: number; invert: boolean }
export const DISPLAY_KEYS = ["r", "g", "b"] as const
const AUTO: BlackWhite = Object.freeze({ mode: "auto" })
const GAMMA: Stretch = Object.freeze({ kind: "gamma", gamma: 0.7 })
const layer = (tint: string): LayerDisplay => Object.freeze({ enabled: true, tint, gain: 1, blackWhite: AUTO, stretch: GAMMA })
export const DEFAULT_DISPLAY: DisplayState = Object.freeze({
  color: Object.freeze({ r: layer("#ff0000"), g: layer("#00ff00"), b: layer("#0000ff") }),
  composite: Object.freeze({ gain: 1, blackWhite: AUTO, stretch: GAMMA, invert: false }),
})
function gain(value: unknown): number {
  return typeof value === "number" && Number.isFinite(value) ? Math.max(0, Math.min(2, value)) : 1
}
function stretch(value: Stretch | undefined): Stretch {
  if (value?.kind === "linear" || value?.kind === "asinh") return { kind: value.kind }
  return value?.kind === "gamma" && Number.isFinite(value.gamma)
    ? { kind: "gamma", gamma: Math.max(0.2, Math.min(3, value.gamma)) } : GAMMA
}
function blackWhite(value: BlackWhite | undefined): BlackWhite {
  return value?.mode === "manual" && Number.isFinite(value.low) && Number.isFinite(value.high) && value.high > value.low
    ? { mode: "manual", low: value.low, high: value.high } : AUTO
}
/** Untouched points must not turn an Auto URL setting into six-digit Manual values. */
export function toneBlurCommit(black: string, white: string, edited: boolean, policy?: (low: number, high: number) => string | undefined):
  { kind: "ignore" } | { kind: "error"; message: string } | { kind: "manual"; blackWhite: BlackWhite } {
  if (!edited) return { kind: "ignore" }
  const low = Number(black)
  const high = Number(white)
  if (black.trim() === "" || white.trim() === "" || !Number.isFinite(low) || !Number.isFinite(high) || high <= low)
    return { kind: "error", message: "Enter finite points with White greater than Black." }
  const rejected = policy?.(low, high)
  if (rejected) return { kind: "error", message: rejected }
  return { kind: "manual", blackWhite: { mode: "manual", low, high } }
}
/** The server keeps Color points as f32; mirror its finite positive span rule. */
export function colorPointError(low: number, high: number): string | undefined {
  const span = Math.fround(high - low)
  if (!Number.isFinite(Math.fround(low)) || !Number.isFinite(Math.fround(high)) || !Number.isFinite(span) || span <= 0)
    return "Color points must be finite 32-bit values with a positive span."
  return undefined
}
export function normalizeDisplay(value: DisplayState): DisplayState {
  const color = Object.fromEntries(DISPLAY_KEYS.map((key) => {
    const current = value.color[key]
    return [key, { enabled: current.enabled !== false, tint: /^#[\da-f]{6}$/i.test(current.tint) ? current.tint.toLowerCase() : DEFAULT_DISPLAY.color[key].tint,
      gain: gain(current.gain), blackWhite: blackWhite(current.blackWhite), stretch: stretch(current.stretch) }]
  })) as DisplayState["color"]
  return { color, composite: { gain: gain(value.composite.gain), blackWhite: blackWhite(value.composite.blackWhite), stretch: stretch(value.composite.stretch), invert: value.composite.invert === true } }
}
export function displayOf(view: Pick<View, "display">): DisplayState { return view.display ? normalizeDisplay(view.display) : DEFAULT_DISPLAY }
export function withDisplay(view: View, display: DisplayState): View {
  const next = normalizeDisplay(display)
  return { ...view, display: isDefaultDisplay(next) ? undefined : next }
}
export function isDefaultDisplay(display: DisplayState): boolean {
  const params = new URLSearchParams()
  writeDisplay(params, display)
  return params.size === 0
}
function readStretch(raw: string | null): Stretch {
  if (raw === "linear" || raw === "asinh") return { kind: raw }
  if (raw?.startsWith("gamma:") && raw.slice(6).trim() !== "" && Number.isFinite(Number(raw.slice(6)))) return stretch({ kind: "gamma", gamma: Number(raw.slice(6)) })
  return GAMMA
}
function readBw(raw: string | null): BlackWhite {
  const parts = raw?.split(",")
  return parts?.length === 2 && parts.every((part) => part.trim() !== "") ? blackWhite({ mode: "manual", low: Number(parts[0]), high: Number(parts[1]) }) : AUTO
}
function readGain(raw: string | null): number { return raw !== null && raw.trim() !== "" ? gain(Number(raw)) : 1 }
export function readDisplay(params: URLSearchParams): DisplayState | undefined {
  const off = params.get("off")?.split(",") ?? []
  const disabled = off.every((key) => DISPLAY_KEYS.some((allowed) => key === allowed)) ? off : []
  const color = Object.fromEntries(DISPLAY_KEYS.map((key) => {
    const tint = params.get(`tint_${key}`)
    return [key, { enabled: !disabled.includes(key), tint: tint && /^[\da-f]{6}$/i.test(tint) ? `#${tint.toLowerCase()}` : DEFAULT_DISPLAY.color[key].tint,
      gain: readGain(params.get(`gain_${key}`)), blackWhite: readBw(params.get(`bw_${key}`)), stretch: readStretch(params.get(`stretch_${key}`)) }]
  })) as DisplayState["color"]
  const display = { color, composite: { gain: readGain(params.get("gain_c")), blackWhite: readBw(params.get("bw_c")), stretch: readStretch(params.get("stretch_c")), invert: params.get("invert_c") === "1" } }
  return isDefaultDisplay(display) ? undefined : display
}
export function writeDisplay(params: URLSearchParams, display: DisplayState): void {
  const write = (key: string, value: DisplayState["composite"] | LayerDisplay) => {
    if (value.gain !== 1) params.set(`gain_${key}`, String(value.gain))
    if (value.blackWhite.mode === "manual") params.set(`bw_${key}`, `${value.blackWhite.low},${value.blackWhite.high}`)
    if (value.stretch.kind !== "gamma" || value.stretch.gamma !== 0.7) params.set(`stretch_${key}`, value.stretch.kind === "gamma" ? `gamma:${value.stretch.gamma}` : value.stretch.kind)
  }
  DISPLAY_KEYS.forEach((key) => {
    const value = display.color[key]
    if (value.tint !== DEFAULT_DISPLAY.color[key].tint) params.set(`tint_${key}`, value.tint.slice(1))
    write(key, value)
  })
  const off = DISPLAY_KEYS.filter((key) => !display.color[key].enabled)
  if (off.length) params.set("off", off.join(","))
  write("c", display.composite)
  if (display.composite.invert) params.set("invert_c", "1")
}
/** Auto uses the exact server span, not a rounded high-low subtraction. */
export function resolveDisplay(display: DisplayState, metadata: {
  source_recipe: { mode: string }
  channels: { index: number; default_black: number | null; default_span: number | null }[]
} | null): ResolvedDisplay {
  const color = metadata?.source_recipe.mode === "color"
  const layers = (metadata?.channels ?? []).map((channel): ResolvedLayer => {
    const settings = color ? display.color[DISPLAY_KEYS[2 - channel.index]] : display.composite
    const tint = color ? display.color[DISPLAY_KEYS[2 - channel.index]].tint : "#ffffff"
    return { channel: channel.index, enabled: color ? display.color[DISPLAY_KEYS[2 - channel.index]].enabled : true,
      tint: [1, 3, 5].map((offset) => parseInt(tint.slice(offset, offset + 2), 16) / 255), gain: settings.gain,
      low: settings.blackWhite.mode === "auto" ? channel.default_black : settings.blackWhite.low,
      span: settings.blackWhite.mode === "auto" ? channel.default_span : settings.blackWhite.high - settings.blackWhite.low, stretch: settings.stretch }
  })
  return { layers, lift: color ? 1 : 1.15, invert: !color && display.composite.invert }
}
