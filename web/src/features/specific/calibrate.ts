/** Shared display math for calibrated Specific frames. Values are MJy/sr;
 * NaN is no data. Every frame of one run uses one stretch so brightness
 * changes across epochs are real, not per-frame autoscaling. */

export type FramePlane = {
  width: number
  height: number
  /** Top-left origin, row-major, little-endian f32 from `/api/v1/frame`. */
  values: Float32Array
}

export type Stretch = { low: number; high: number; span: number }

const NO_DATA_RGB: [number, number, number] = [24, 28, 36]
const DISPLAY_GAMMA = 0.7

export function percentile(sorted: Float32Array | number[], p: number): number {
  if (sorted.length === 0) return 0
  const i = Math.min(sorted.length - 1, Math.round((sorted.length - 1) * p))
  return sorted[i]!
}

/** One shared stretch over every finite sample of every frame. */
export function sharedStretch(planes: readonly FramePlane[]): Stretch {
  const samples: number[] = []
  for (const plane of planes) {
    for (const value of plane.values) {
      if (Number.isFinite(value)) samples.push(value)
    }
  }
  samples.sort((a, b) => a - b)
  if (samples.length === 0) return { low: 0, high: 1, span: 1 }
  const low = percentile(samples, 0.02)
  const high = percentile(samples, 0.995)
  const span = high > low ? high - low : 1
  return { low, high, span }
}

export function tone(value: number, stretch: Stretch): number {
  const t = Math.pow(Math.min(1, Math.max(0, (value - stretch.low) / stretch.span)), DISPLAY_GAMMA)
  return Math.round(t * 255)
}

/** Paint one plane through a shared stretch. Missing samples become no-data. */
export function paintPlane(plane: FramePlane, stretch: Stretch, out: ImageData): void {
  const rgb = out.data
  for (let i = 0, p = 0; i < plane.values.length; i++, p += 4) {
    const value = plane.values[i]!
    if (!Number.isFinite(value)) {
      rgb[p] = NO_DATA_RGB[0]
      rgb[p + 1] = NO_DATA_RGB[1]
      rgb[p + 2] = NO_DATA_RGB[2]
      rgb[p + 3] = 255
      continue
    }
    const grey = tone(value, stretch)
    rgb[p] = grey
    rgb[p + 1] = grey
    rgb[p + 2] = grey
    rgb[p + 3] = 255
  }
}

/** Calibrated difference painted as mid-grey zero. White is brighter in `a`. */
export function paintDifference(a: FramePlane, b: FramePlane, stretch: Stretch, out: ImageData, gain = 1.5): void {
  const rgb = out.data
  const half = stretch.span / 2
  for (let i = 0, p = 0; i < a.values.length; i++, p += 4) {
    const left = a.values[i]!
    const right = b.values[i]!
    if (!Number.isFinite(left) || !Number.isFinite(right)) {
      rgb[p] = NO_DATA_RGB[0]
      rgb[p + 1] = NO_DATA_RGB[1]
      rgb[p + 2] = NO_DATA_RGB[2]
      rgb[p + 3] = 255
      continue
    }
    const grey = Math.round(Math.min(255, Math.max(0, 128 + ((left - right) / half) * 128 * gain)))
    rgb[p] = grey
    rgb[p + 1] = grey
    rgb[p + 2] = grey
    rgb[p + 3] = 255
  }
}

/** Equal-weight mean of the planes that have a finite sample at each pixel. */
export function paintSum(planes: readonly FramePlane[], stretch: Stretch, out: ImageData): void {
  const rgb = out.data
  const n = planes[0]?.values.length ?? 0
  for (let i = 0, p = 0; i < n; i++, p += 4) {
    let total = 0
    let count = 0
    for (const plane of planes) {
      const value = plane.values[i]!
      if (!Number.isFinite(value)) continue
      total += value
      count += 1
    }
    if (count === 0) {
      rgb[p] = NO_DATA_RGB[0]
      rgb[p + 1] = NO_DATA_RGB[1]
      rgb[p + 2] = NO_DATA_RGB[2]
      rgb[p + 3] = 255
      continue
    }
    const grey = tone(total / count, stretch)
    rgb[p] = grey
    rgb[p + 1] = grey
    rgb[p + 2] = grey
    rgb[p + 3] = 255
  }
}

/** Fraction of finite samples in a 7-pixel aperture around the field centre. */
export function centreCoverage(plane: FramePlane, radius = 3): number {
  const cx = Math.floor(plane.width / 2)
  const cy = Math.floor(plane.height / 2)
  let total = 0
  let ok = 0
  for (let y = cy - radius; y <= cy + radius; y++) {
    if (y < 0 || y >= plane.height) continue
    for (let x = cx - radius; x <= cx + radius; x++) {
      if (x < 0 || x >= plane.width) continue
      total += 1
      if (Number.isFinite(plane.values[y * plane.width + x]!)) ok += 1
    }
  }
  return total === 0 ? 0 : ok / total
}
