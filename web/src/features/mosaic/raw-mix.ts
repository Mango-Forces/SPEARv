import type { ResolvedDisplay, Stretch } from "../../url-state/display.ts"
import type { RawChannel } from "./raw-protocol.ts"
export const NO_DATA = [24, 28, 36, 255] as const
export const SCALE_MAX = 65534
const f = Math.fround
const clamp = (value: number) => Math.max(0, Math.min(1, value))
export type RawSamples = Float32Array | Uint16Array
function stretch(value: number, setting: Stretch): number {
  if (setting.kind === "linear") return value
  if (setting.kind === "gamma") return f(Math.pow(value, f(setting.gamma)))
  return f(Math.asinh(value / 0.1) / Math.asinh(10))
}
function sample(values: RawSamples | undefined, channel: RawChannel | undefined, pixel: number): number {
  if (!values) return NaN
  if (values instanceof Float32Array) return values[pixel]
  const code = values[pixel]
  if (code === 65535 || channel?.scale_offset == null || channel.scale_span == null) return NaN
  return f(channel.scale_offset + f(f(code / SCALE_MAX) * channel.scale_span))
}
/** Tile-sized output only; retained layer views remain owned by the Worker. */
export function mixRawTile(channels: readonly RawSamples[], metadata: readonly RawChannel[], display: ResolvedDisplay, pixels: number): Uint8ClampedArray {
  const rgba = new Uint8ClampedArray(pixels * 4)
  for (let pixel = 0; pixel < pixels; pixel++) {
    let valid = false
    let r = 0, g = 0, b = 0
    for (const layer of display.layers) {
      const value = sample(channels[layer.channel], metadata[layer.channel], pixel)
      if (!layer.enabled || !Number.isFinite(value) || layer.low === null || layer.span === null) continue
      valid = true
      const t = clamp(f(f(value - layer.low) / layer.span))
      const intensity = f(f(stretch(t, layer.stretch) * f(layer.gain)) * f(display.lift))
      r = f(r + f(intensity * layer.tint[0]))
      g = f(g + f(intensity * layer.tint[1]))
      b = f(b + f(intensity * layer.tint[2]))
    }
    const offset = pixel * 4
    if (!valid) { rgba.set(NO_DATA, offset); continue }
    const byte = (gun: number) => Math.round(f(255 * (display.invert ? f(1 - clamp(gun)) : clamp(gun))))
    rgba[offset] = byte(r); rgba[offset + 1] = byte(g); rgba[offset + 2] = byte(b); rgba[offset + 3] = 255
  }
  return rgba
}
