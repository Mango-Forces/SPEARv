import { readDisplay, writeDisplay, displayOf, type DisplayState } from "./display.ts"
import type { Bounds } from "../features/specific/bounds.ts"
import { DEFAULT_COLOUR_BANDS, type ColourConfigValue } from "./colour.ts"
import { isReleaseId, parseBands, parseUtc } from "../features/specific/bounds.ts"
import { DEFAULT_BOUNDS, isDefaultBounds, specificBoundsParams } from "../features/specific/link.ts"
import { MAX_FOV, formatNumber } from "./position.ts"

export { displayOf, withDisplay } from "./display.ts"

export type Mode = "composite" | "slider" | "range" | "delta" | "specific"

/** The three modes the UI exposes. `mode` also accepts the slider, range and
 * delta render values, so links that carry them still draw what they ask for. */
export type Workspace = "color" | "composite" | "specific"

export type Pin = {
  center: number
  width: number
  visit?: number
  sign?: "+" | "-"
}

export type View = {
  ra: number
  dec: number
  fovL: number
  fovH: number
  fovLinked: boolean
  mode: Mode
  dr: string
  pins: Pin[]
  /** Marker opacity per catalog. `null` means the layer is off and the param stays out of the URL. */
  gaia: number | null
  wise: number | null
  mask: boolean
  /** Delta candidate labels. On by default; `candidates=off` hides them. */
  candidates: boolean
  /** `movers=on` link field. Parsed and written back so links round-trip;
   * the UI exposes no movers action. */
  movers: boolean
  /** False-colour three-band render instead of the selected mode. */
  color: boolean
  /** Applied Specific bounds. `undefined` means the default all-bounds value.
   * Stored across mode changes so a Show/mode/layer commit never drops the
   * applied table bounds; only Specific queries use them. */
  bounds?: Bounds
  /** Custom false-colour recipe in API numbering (0..101). `undefined` means
   * the server defaults (red 91, green 41, blue 6, width 5 each in UI
   * numbering). Stored across mode changes so leaving Colour never loses it. */
  colour?: ColourConfigValue
  /** Display-only settings survive every mode and never enter science job identity. */
  display?: DisplayState
  /** Invalid links are an error screen, never a substitute science view. Not serialized. */
  urlErrors?: string[]
}

const DEFAULTS: View = {
  ra: 56.75,
  dec: 24.12,
  fovL: 0.1,
  fovH: 0.1,
  fovLinked: true,
  mode: "slider",
  dr: "qr3",
  pins: [{ center: 42, width: 5 }],
  gaia: null,
  wise: null,
  mask: true,
  candidates: true,
  movers: false,
  color: false,
}

export function workspaceOf(view: View): Workspace {
  if (view.mode === "specific") return "specific"
  return view.color ? "color" : "composite"
}

/** Clicking a mode is an explicit move to that workspace. A slider, range or
 * delta render mode from a link survives until the user picks one of the three. */
export function setWorkspace(view: View, workspace: Workspace): View {
  if (workspace === "specific") return { ...view, mode: "specific", color: false }
  return { ...view, mode: "composite", color: workspace === "color" }
}

function readParameter<T>(params: URLSearchParams, key: string, fallback: T, parse: (value: string) => T, errors: string[]): T {
  const raw = params.get(key)
  if (raw === null) return fallback
  try {
    return parse(raw)
  } catch (cause) {
    errors.push(`${key}: ${cause instanceof Error ? cause.message : "Invalid value."}`)
    return fallback
  }
}

function decimal(value: string): number {
  return /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)(?:e[+-]?\d+)?$/i.test(value.trim()) ? Number(value) : NaN
}

function urlFov(value: string): number {
  const fov = decimal(value)
  if (!Number.isFinite(fov) || fov <= 0 || fov > MAX_FOV) {
    throw new Error(`Use decimal degrees, more than 0 and at most ${MAX_FOV}.`)
  }
  return fov
}

/** Opacity in the URL is 0..1. Missing or unreadable means the layer is off. */
function opacityOr(value: string | null): number | null {
  if (value === null || value.trim() === "") return null
  const parsed = Number(value)
  if (!Number.isFinite(parsed)) return null
  return Math.min(1, Math.max(0, parsed))
}

function isDefaultColour(colour: ColourConfigValue): boolean {
  const bands = colour.colorBands
  return (
    bands.r.pin === DEFAULT_COLOUR_BANDS.r.pin &&
    bands.r.width === DEFAULT_COLOUR_BANDS.r.width &&
    bands.g.pin === DEFAULT_COLOUR_BANDS.g.pin &&
    bands.g.width === DEFAULT_COLOUR_BANDS.g.width &&
    bands.b.pin === DEFAULT_COLOUR_BANDS.b.pin &&
    bands.b.width === DEFAULT_COLOUR_BANDS.b.width
  )
}

function colourPin(value: string): number {
  if (value.trim() === "") throw new Error("Colour band must be an integer 0..101.")
  const parsed = decimal(value)
  if (!Number.isInteger(parsed) || parsed < 0 || parsed > 101) throw new Error("Colour band must be an integer 0..101.")
  return parsed
}

function colourWidth(value: string): number {
  if (value.trim() === "") throw new Error("Colour width must be an integer 1..102.")
  const parsed = decimal(value)
  if (!Number.isInteger(parsed) || parsed < 1 || parsed > 102) throw new Error("Colour width must be an integer 1..102.")
  return parsed
}

const COLOUR_KEYS = ["color_r", "color_r_width", "color_g", "color_g_width", "color_b", "color_b_width"] as const

/** Missing channels use defaults; invalid channels block the link, not just the recipe. */
function readColour(params: URLSearchParams, errors: string[]): ColourConfigValue | undefined {
  if (!COLOUR_KEYS.some((key) => params.has(key))) return undefined
  const before = errors.length
  const value: ColourConfigValue = {
    colorBands: {
      r: { pin: readParameter(params, "color_r", DEFAULT_COLOUR_BANDS.r.pin, colourPin, errors), width: readParameter(params, "color_r_width", DEFAULT_COLOUR_BANDS.r.width, colourWidth, errors) },
      g: { pin: readParameter(params, "color_g", DEFAULT_COLOUR_BANDS.g.pin, colourPin, errors), width: readParameter(params, "color_g_width", DEFAULT_COLOUR_BANDS.g.width, colourWidth, errors) },
      b: { pin: readParameter(params, "color_b", DEFAULT_COLOUR_BANDS.b.pin, colourPin, errors), width: readParameter(params, "color_b_width", DEFAULT_COLOUR_BANDS.b.width, colourWidth, errors) },
    },
  }
  if (errors.length > before) return undefined
  return isDefaultColour(value) ? undefined : value
}

/** Validate each filter separately so a link can name every bad parameter. */
function readBounds(params: URLSearchParams, errors: string[]): Bounds | undefined {
  if (!params.has("from_utc") && !params.has("to_utc") && !params.has("releases") && !params.has("bands")) return undefined
  const before = errors.length

  const bounds: Bounds = {
    fromUtc: readParameter(params, "from_utc", null, parseUtc, errors),
    toUtc: readParameter(params, "to_utc", null, parseUtc, errors),
    releases: readParameter(params, "releases", DEFAULT_BOUNDS.releases, (value) => {
      const releases = value.split(",")
      if (releases.some((release) => !isReleaseId(release))) throw new Error("Use qr2, qr3, dr1, or a comma list of those.")
      return [...new Set(releases)].sort()
    }, errors),
    bands: readParameter(params, "bands", DEFAULT_BOUNDS.bands, parseBands, errors),
  }
  if (bounds.fromUtc && bounds.toUtc && bounds.fromUtc > bounds.toUtc) {
    errors.push("from_utc: UTC start must not be after to_utc.")
  }
  if (errors.length > before) return undefined
  return isDefaultBounds(bounds) ? undefined : bounds
}

export function parsePin(raw: string): Pin | null {
  const parts = raw.split(":")
  const center = Number(parts[0])
  const width = Number(parts[1] ?? "5")
  if (!Number.isInteger(center) || !Number.isInteger(width)) return null
  if (center < 0 || center > 101 || width < 1 || width > 102) return null
  const visitText = parts[2]
  const visit = visitText && visitText !== "" ? Number(visitText.replace(/^v/, "")) : undefined
  if (visit !== undefined && !Number.isInteger(visit)) return null
  const signText = parts[3]
  const sign = signText === "+" || signText === "-" ? signText : undefined
  if (signText && signText !== "" && !sign) return null
  return { center, width, visit, sign }
}

export function formatPin(pin: Pin): string {
  if (pin.visit === undefined && !pin.sign) return `${pin.center}:${pin.width}`
  const visit = pin.visit === undefined ? "" : `v${pin.visit}`
  return `${pin.center}:${pin.width}:${visit}:${pin.sign ?? ""}`
}

export function readView(search: string): View {
  const params = new URLSearchParams(search)
  const errors: string[] = []
  if (params.has("fov")) errors.push(`fov is unsupported. Use fov_l and fov_h in degrees, more than 0 and at most ${MAX_FOV}.`)
  const mode = params.get("mode")
  const workspace = mode === "specific" ? "specific" : mode === "color" ? "color" : undefined
  const linkMode = mode === "composite" || mode === "range" || mode === "slider" || mode === "delta" ? mode : undefined
  const rawPins = params.getAll("pin")
  const pins = rawPins
    .map(parsePin)
    .filter((pin): pin is Pin => pin !== null)
  const mask = params.get("mask")
  const candidates = params.get("candidates")
  const ra = readParameter(params, "ra", DEFAULTS.ra, (value) => {
    const ra = decimal(value)
    if (!Number.isFinite(ra) || ra < 0 || ra > 360) throw new Error("Use decimal degrees from 0 to 360.")
    return ra
  }, errors)
  const dec = readParameter(params, "dec", DEFAULTS.dec, (value) => {
    const dec = decimal(value)
    if (!Number.isFinite(dec) || dec < -90 || dec > 90) throw new Error("Use decimal degrees from -90 to 90.")
    return dec
  }, errors)
  const fovL = readParameter(params, "fov_l", DEFAULTS.fovL, urlFov, errors)
  const fovH = readParameter(params, "fov_h", params.has("fov_l") ? fovL : DEFAULTS.fovH, urlFov, errors)
  const bounds = readBounds(params, errors)
  const colour = readColour(params, errors)
  return {
    ra,
    dec,
    fovL: params.has("fov_l") ? fovL : fovH,
    fovH,
    fovLinked: (!params.has("fov_l") || fovL === fovH) && params.get("fov_linked") !== "0",
    mode:
      workspace === "specific"
        ? "specific"
        : workspace === "color" || params.get("color") === "1"
          ? "composite"
          : linkMode ?? DEFAULTS.mode,
    dr: params.get("dr") || DEFAULTS.dr,
    pins: params.get("pins") === "none" ? [] : rawPins.length > 0 ? pins : DEFAULTS.pins,
    mask: !(mask === "off" || mask === "0" || mask === "false"),
    candidates: !(candidates === "off" || candidates === "0" || candidates === "false"),
    movers: params.get("movers") === "on" || params.get("movers") === "1" || params.get("movers") === "true",
    color: workspace === "color" || params.get("color") === "1",
    gaia: opacityOr(params.get("gaia")),
    wise: opacityOr(params.get("wise")),
    bounds,
    colour,
    display: readDisplay(params),
    urlErrors: errors.length ? errors : undefined,
  }
}

/** Read the URL once, before the first render. Starting from `DEFAULTS` and
 * replacing the snapshot in `subscribe` would render the default view first and
 * fire a request for it before the URL's view replaced it. */
// The root page defaults to colour mode. A copied link always carries its own
// parameters, so only the bare page redirects.
if (typeof window !== "undefined" && (window.location.search === "" || window.location.search === "?")) {
  window.history.replaceState(null, "", "/?ra=56.75&dec=24.12&fov_l=1&fov_h=1&mode=color&mask=off")
}

let snapshot: View = typeof window === "undefined" ? DEFAULTS : readView(window.location.search)

export function currentView(): View {
  return snapshot
}

export function viewSearch(view: View): string {
  const params = new URLSearchParams()
  params.set("ra", formatNumber(view.ra))
  params.set("dec", formatNumber(view.dec))
  params.set("fov_l", formatNumber(view.fovL))
  params.set("fov_h", formatNumber(view.fovH))
  if (!view.fovLinked) params.set("fov_linked", "0")
  if (view.mode === "specific") params.set("mode", "specific")
  else if (view.color) params.set("mode", "color")
  else params.set("mode", view.mode)
  params.set("dr", view.dr)
  params.set("mask", view.mask ? "on" : "off")
  if (view.pins.length === 0) params.set("pins", "none")
  for (const pin of view.pins) params.append("pin", formatPin(pin))
  if (view.gaia !== null) params.set("gaia", formatNumber(view.gaia))
  if (view.wise !== null) params.set("wise", formatNumber(view.wise))
  if (!view.candidates) params.set("candidates", "off")
  if (view.movers) params.set("movers", "on")
  if (view.bounds && !isDefaultBounds(view.bounds)) {
    for (const [key, value] of specificBoundsParams(view.bounds)) params.set(key, value)
  }
  if (view.colour && !isDefaultColour(view.colour)) {
    params.set("color_r", String(view.colour.colorBands.r.pin))
    params.set("color_r_width", String(view.colour.colorBands.r.width))
    params.set("color_g", String(view.colour.colorBands.g.pin))
    params.set("color_g_width", String(view.colour.colorBands.g.width))
    params.set("color_b", String(view.colour.colorBands.b.pin))
    params.set("color_b_width", String(view.colour.colorBands.b.width))
  }
  writeDisplay(params, displayOf(view))
  return params.toString()
}

export function writeView(view: View, history: "push" | "replace" = "push") {
  const next = `${window.location.pathname}?${viewSearch(view)}`
  if (history === "replace") window.history.replaceState(null, "", next)
  else window.history.pushState(null, "", next)
}

export function commit(view: View, history: "push" | "replace" = "push") {
  if (viewSearch(view) === viewSearch(snapshot)) return
  snapshot = view
  writeView(view, history)
  window.dispatchEvent(new PopStateEvent("popstate"))
}

export function subscribe(onStoreChange: () => void) {
  const onPop = () => {
    snapshot = readView(window.location.search)
    onStoreChange()
  }
  window.addEventListener("popstate", onPop)
  return () => window.removeEventListener("popstate", onPop)
}
