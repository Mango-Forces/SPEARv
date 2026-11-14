import type { DisplayState, LayerDisplay } from "../../url-state/display.ts"

export type ServerBlackWhite = LayerDisplay["blackWhite"]
export type ServerStretch = LayerDisplay["stretch"]
export type ServerColorLayer = {
  enabled: boolean
  tint: string
  gain: number
  black_white: ServerBlackWhite
  stretch: ServerStretch
}
export type ServerColorDisplay = { r: ServerColorLayer; g: ServerColorLayer; b: ServerColorLayer }

export type DisplayArtifactState =
  | "not_requested"
  | "queued"
  | "rendering"
  | "ready"
  | "failed"
  | "superseded"
  | "evicted"
export type DisplayArtifact = {
  state: DisplayArtifactState
  width: number
  height: number
  url: string
  bytes: number | null
  error: string | null
  completed_rows: number
  total_rows: number
}
export type DisplayChannelMetadata = {
  index: 0 | 1 | 2
  role: "blue" | "green" | "red"
  center_band: number | null
  width: number
  bands: number[]
  wavelength_min_um: number
  wavelength_max_um: number
  finite_count: number
  default_black: number | null
  default_white: number | null
  default_span: number | null
}
export type DisplayManifest = {
  job_id: string
  revision: string
  current: boolean
  width: number
  height: number
  display: ServerColorDisplay
  channels: DisplayChannelMetadata[]
  preview: DisplayArtifact
  full: DisplayArtifact
}
export type ColorDisplayPhase = "queued" | "creating" | "rendering" | "ready" | "failed" | "expired"

export function serverColorDisplay(display: DisplayState): ServerColorDisplay {
  const layer = (value: LayerDisplay): ServerColorLayer => ({
    enabled: value.enabled,
    tint: value.tint.toLowerCase(),
    gain: value.gain,
    black_white: value.blackWhite,
    stretch: value.stretch,
  })
  return { r: layer(display.color.r), g: layer(display.color.g), b: layer(display.color.b) }
}

export function colorDisplayKey(display: DisplayState): string {
  return JSON.stringify(serverColorDisplay(display))
}

/** Compare a manifest echo with the complete local settings it was requested with. */
export function sameServerColorDisplay(a: ServerColorDisplay, b: ServerColorDisplay): boolean {
  const sameLayer = (left: ServerColorLayer, right: ServerColorLayer) =>
    left.enabled === right.enabled && left.tint === right.tint && left.gain === right.gain &&
    sameBlackWhite(left.black_white, right.black_white) && sameStretch(left.stretch, right.stretch)
  return sameLayer(a.r, b.r) && sameLayer(a.g, b.g) && sameLayer(a.b, b.b)
}

function sameBlackWhite(a: ServerBlackWhite, b: ServerBlackWhite): boolean {
  if (a.mode !== b.mode) return false
  if (a.mode === "auto" && b.mode === "auto") return true
  return a.mode === "manual" && b.mode === "manual" && a.low === b.low && a.high === b.high
}

function sameStretch(a: ServerStretch, b: ServerStretch): boolean {
  if (a.kind !== b.kind) return false
  if (a.kind === "gamma" && b.kind === "gamma") return a.gamma === b.gamma
  return true
}

export function validateDisplayManifest(
  value: unknown,
  expected: { jobId: string; revision: string; width: number; height: number },
): DisplayManifest {
  const bad = () => {
    throw new Error("The server display manifest does not match this mosaic revision.")
  }
  if (!record(value) || value.job_id !== expected.jobId || value.revision !== expected.revision ||
      typeof value.current !== "boolean" || !integer(value.width, 1) || !integer(value.height, 1) ||
      value.width !== expected.width || value.height !== expected.height || !Number.isSafeInteger(expected.width * expected.height) || !record(value.display) ||
      !Array.isArray(value.channels) || value.channels.length !== 3) return bad()
  const display = value.display
  const layers = (["r", "g", "b"] as const).map((key): ServerColorLayer => {
    const layer = display[key]
    if (!record(layer) || Object.keys(layer).length !== 5 || typeof layer.enabled !== "boolean" || typeof layer.tint !== "string" ||
        !/^#[\da-f]{6}$/.test(layer.tint) || !finiteIn(layer.gain, 0, 2) || !record(layer.black_white) ||
        !validBlackWhite(layer.black_white) || !record(layer.stretch) || !validStretch(layer.stretch)) return bad()
    const blackWhite = layer.black_white
    const stretch = layer.stretch
    return {
      enabled: layer.enabled,
      tint: layer.tint,
      gain: layer.gain,
      black_white: blackWhite.mode === "auto"
        ? { mode: "auto" }
        : { mode: "manual", low: blackWhite.low as number, high: blackWhite.high as number },
      stretch: stretch.kind === "gamma"
        ? { kind: "gamma", gamma: stretch.gamma as number }
        : { kind: stretch.kind as "linear" | "asinh" },
    }
  })
  const requestedDisplay: ServerColorDisplay = { r: layers[0], g: layers[1], b: layers[2] }
  const channels = value.channels.map((channel, index): DisplayChannelMetadata => {
    if (!record(channel) || channel.index !== index || channel.role !== (["blue", "green", "red"] as const)[index] ||
        (channel.center_band !== null && !integer(channel.center_band, 1, 102)) || !integer(channel.width, 1, 102) ||
        !Array.isArray(channel.bands) ||
        !finite(channel.wavelength_min_um) || !finite(channel.wavelength_max_um) ||
        channel.wavelength_min_um > channel.wavelength_max_um || !integer(channel.finite_count, 0, expected.width * expected.height) ||
        !nullableFinite(channel.default_black) || !nullableFinite(channel.default_white) ||
        !nullableFinite(channel.default_span)) return bad()
    const bands = channel.bands
    if (bands.length === 0 || bands.some((band, i) =>
        !integer(band, 1, 102) || (i > 0 && Number(bands[i - 1]) >= band))) return bad()
    const defaultBlack = channel.default_black === null ? null : Math.fround(channel.default_black)
    const defaultWhite = channel.default_white === null ? null : Math.fround(channel.default_white)
    const defaultSpan = channel.default_span === null
      ? defaultBlack === null || defaultWhite === null ? null : Math.fround(defaultWhite - defaultBlack)
      : Math.fround(channel.default_span)
    if (channel.finite_count === 0) {
      if (defaultBlack !== null || defaultWhite !== null || defaultSpan !== null) return bad()
    } else if (defaultBlack === null || defaultWhite === null || defaultSpan === null || !finite(defaultSpan) ||
        defaultSpan <= 0 || defaultWhite < defaultBlack || (defaultWhite === defaultBlack && channel.default_span === null)) return bad()
    return { ...channel, default_black: defaultBlack, default_white: defaultWhite, default_span: defaultSpan } as DisplayChannelMetadata
  })
  const preview = artifact(value.preview)
  const full = artifact(value.full)
  if (preview.width > 2048 || preview.height > 2048 || full.width !== expected.width || full.height !== expected.height ||
      preview.total_rows !== preview.height || full.total_rows !== full.height) return bad()
  return {
    job_id: value.job_id,
    revision: value.revision,
    current: value.current,
    width: value.width,
    height: value.height,
    display: requestedDisplay,
    channels,
    preview,
    full,
  }
}

function artifact(value: unknown): DisplayArtifact {
  if (!record(value) || !["not_requested", "queued", "rendering", "ready", "failed", "superseded", "evicted"].includes(String(value.state)) ||
      !integer(value.width, 1) || !integer(value.height, 1) || typeof value.url !== "string" ||
      !(value.bytes === null || integer(value.bytes, 0)) ||
      !(value.error === null || typeof value.error === "string") || !integer(value.completed_rows, 0) ||
      !integer(value.total_rows, 0) || value.completed_rows > value.total_rows ||
      (value.state === "ready" && (value.bytes === null || value.completed_rows !== value.total_rows))) {
    throw new Error("The server display artifact status is malformed.")
  }
  return value as DisplayArtifact
}

function validBlackWhite(value: Record<string, unknown>): boolean {
  if (value.mode === "auto") return Object.keys(value).length === 1
  return value.mode === "manual" && finite(value.low) && finite(value.high) && value.high > value.low &&
    Object.keys(value).length === 3
}

function validStretch(value: Record<string, unknown>): boolean {
  if (value.kind === "linear" || value.kind === "asinh") return Object.keys(value).length === 1
  return value.kind === "gamma" && finiteIn(value.gamma, 0.2, 3) && Object.keys(value).length === 2
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
}
function integer(value: unknown, min: number, max = Number.MAX_SAFE_INTEGER): value is number {
  return typeof value === "number" && Number.isSafeInteger(value) && value >= min && value <= max
}
function finite(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value)
}
function finiteIn(value: unknown, min: number, max: number): value is number {
  return finite(value) && value >= min && value <= max
}
function nullableFinite(value: unknown): value is number | null {
  return value === null || finite(value)
}
