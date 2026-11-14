import type { DisplayState, ResolvedDisplay } from "../../url-state/display.ts"
export type { ResolvedDisplay } from "../../url-state/display.ts"
export type RawChannel = {
  index: 0 | 1 | 2
  role: "blue" | "green" | "red" | "grey"
  center_band: number | null
  width: number
  bands: number[]
  wavelength_min_um: number
  wavelength_max_um: number
  finite_count: number
  default_black: number | null
  default_white: number | null
  default_span: number | null
  /** Physical value at normalized code zero; Color layers only. */
  scale_offset: number | null
  /** Physical width covered by normalized codes 0..65534; Color layers only. */
  scale_span: number | null
}
export type RawMetadata = {
  format_version: 1 | 2; job_id: string; state: "done"; width: number; height: number; tile_size: 256;
  dtype: "f32le" | "u16le"; orientation: "png-top-row-first"; nodata: "nan" | "65535";
  source_recipe: { mode: "color" | "composite" | "range"; mask: boolean; pins: { center_band: number; width: number; visit?: number | null; sign?: string | null }[] };
  channels: RawChannel[];
}
export type RawSessionState = { phase: "idle" } | { phase: "awaiting-opt-in"; estimateBytes: number }
  | { phase: "loading"; loadedTiles: number; totalTiles: number } | { phase: "ready" } | { phase: "unavailable"; reason: string }
export type RenderGate = { exportReady: boolean; remapBusy: boolean; completedRevision: number; pendingRevision: number | null; label: string }
export type RawErrorCode = "http-404" | "http-410" | "http-409" | "bad-tile" | "bad-metadata" | "worker"
export type ToRawWorker = { type: "init"; session: number; metadata: RawMetadata }
  | { type: "tile"; session: number; channel: number; tileIndex: number; tx: number; ty: number; width: number; height: number; buffer: ArrayBuffer }
  | { type: "load-complete"; session: number } | { type: "render"; session: number; revision: number; display: ResolvedDisplay }
  | { type: "dispose"; session: number }
export type FromRawWorker = { type: "load-progress"; session: number; loadedTiles: number; totalTiles: number }
  | { type: "load-ready"; session: number } | { type: "render-started" | "render-complete"; session: number; revision: number }
  | { type: "tile-rgba"; session: number; revision: number; tx: number; ty: number; width: number; height: number; buffer: ArrayBuffer }
  | { type: "error"; session: number; code: RawErrorCode; message: string }
export type DisplayControlsProps = { display: DisplayState; resolved: ResolvedDisplay; metadata: RawMetadata | null; session: RawSessionState; gate: RenderGate;
  onDisplayChange(next: DisplayState): void; onStartRaw(): void; onCancelRaw(): void; disabledReason?: string }
function record(value: unknown): value is Record<string, unknown> { return value !== null && typeof value === "object" && !Array.isArray(value) }
function integer(value: unknown, low: number, high: number): value is number { return typeof value === "number" && Number.isSafeInteger(value) && value >= low && value <= high }
function finite(value: unknown): value is number { return typeof value === "number" && Number.isFinite(value) }
/** The only unknown-JSON boundary. Internal rendering consumes this validated contract. */
export function validateRawMetadata(value: unknown, expected: { id: string; width: number; height: number; channels?: number }): RawMetadata {
  const bad = () => { throw new Error("Raw metadata does not match this completed mosaic.") }
  if (!record(value) || value.state !== "done" || value.job_id !== expected.id ||
      !integer(value.width, 1, Number.MAX_SAFE_INTEGER) || !integer(value.height, 1, Number.MAX_SAFE_INTEGER) || value.width !== expected.width || value.height !== expected.height ||
      value.tile_size !== 256 || value.orientation !== "png-top-row-first") return bad()
  const recipe = value.source_recipe
  if (!record(recipe) || !["color", "composite", "range"].includes(String(recipe.mode)) || typeof recipe.mask !== "boolean" || !Array.isArray(recipe.pins) ||
    recipe.pins.some((pin) => !record(pin) || !integer(pin.center_band, 1, 102) || !integer(pin.width, 1, 102) || (pin.visit != null && !integer(pin.visit, 0, 65535)) || (pin.sign != null && pin.sign !== "+" && pin.sign !== "-"))) return bad()
  const color = recipe.mode === "color"
  if (color ? value.format_version !== 2 || value.dtype !== "u16le" || value.nodata !== "65535"
    : value.format_version !== 1 || value.dtype !== "f32le" || value.nodata !== "nan") return bad()
  const count = color ? 3 : 1
  const pixels = value.width * value.height
  const bytes = value.dtype === "u16le" ? 2 : 4
  if (!Array.isArray(value.channels) || value.channels.length !== count || (expected.channels !== undefined && expected.channels !== count) || !Number.isSafeInteger(pixels * count * bytes)) return bad()
  const channels: RawChannel[] = []
  value.channels.forEach((channel, index) => {
    if (!record(channel) || channel.index !== index || channel.role !== (count === 1 ? "grey" : ["blue", "green", "red"][index]) ||
      (channel.center_band !== null && !integer(channel.center_band, 1, 102)) || !integer(channel.width, 1, 102) || !Array.isArray(channel.bands) || channel.bands.length === 0 ||
      channel.bands.some((band, i, bands) => !integer(band, 1, 102) || (i > 0 && Number(bands[i - 1]) >= band)) ||
      !finite(channel.wavelength_min_um) || !finite(channel.wavelength_max_um) || channel.wavelength_min_um > channel.wavelength_max_um ||
      !integer(channel.finite_count, 0, pixels)) return bad()
    if (channel.finite_count === 0) {
      if (channel.default_black != null || channel.default_white != null || channel.default_span != null || channel.scale_offset != null || channel.scale_span != null) return bad()
      channels.push({ ...(channel as RawChannel), default_black: null, default_white: null, default_span: null, scale_offset: null, scale_span: null })
      return
    }
    if (!finite(channel.default_black) || !finite(channel.default_white)) return bad()
    const black = Math.fround(channel.default_black)
    const white = Math.fround(channel.default_white)
    const derived = Math.fround(white - black)
    if (channel.default_span != null && (!finite(channel.default_span) || channel.default_span <= 0)) return bad()
    const span = channel.default_span == null ? derived : Math.fround(channel.default_span)
    if (!finite(span) || span <= 0 || white < black || (white === black && channel.default_span == null)) return bad()
    const scale = color
      ? finite(channel.scale_offset) && finite(channel.scale_span) && channel.scale_span > 0
        ? { scale_offset: Math.fround(channel.scale_offset), scale_span: Math.fround(channel.scale_span) }
        : bad()
      : { scale_offset: null, scale_span: null }
    if (!color && (channel.scale_offset != null || channel.scale_span != null)) return bad()
    channels.push({ ...(channel as RawChannel), default_black: black, default_white: white, default_span: span, ...scale })
  })
  const metadata = value as RawMetadata
  return { ...metadata, source_recipe: { ...metadata.source_recipe, pins: metadata.source_recipe.pins.map((pin) => ({ ...pin, visit: pin.visit ?? null, sign: pin.sign ?? null })) }, channels }
}
export function rawBytes(metadata: RawMetadata): number { return metadata.width * metadata.height * metadata.channels.length * (metadata.dtype === "u16le" ? 2 : 4) }
export const AUTO_RAW_PIXELS = 2048 * 2048
