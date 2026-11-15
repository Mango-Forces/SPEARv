/** Shared display math for calibrated Specific frames. Values are MJy/sr;
 * NaN is no data. Every frame of one run uses one stretch so brightness
 * changes across epochs are real, not per-frame autoscaling.
 *
 * Display options mirror WiseView: fixed min/max, linear or soft stretch, invert.
 * Motion checks: calibrated difference, half-opacity onion skin, abs-diff glow. */

export type FramePlane = {
  width: number
  height: number
  /** Top-left origin, row-major, little-endian f32 from `/api/v1/frame`. */
  values: Float32Array
}

export type Stretch = { low: number; high: number; span: number }

/** How physical values become grey. WiseView defaults are linear + invert. */
export type DisplayTone = {
  /** 1 = linear (WiseView); lower lifts the faint end. */
  gamma: number
  invert: boolean
}

export const TONE_SOFT: DisplayTone = { gamma: 0.7, invert: false }
export const TONE_WISEVIEW: DisplayTone = { gamma: 1, invert: true }

const NO_DATA_RGB: [number, number, number] = [24, 28, 36]
/** Lime accent for motion glow, matching the shell. */
const MOTION_RGB: [number, number, number] = [198, 255, 46]

export function percentile(sorted: Float32Array | number[], p: number): number {
  if (sorted.length === 0) return 0
  const i = Math.min(sorted.length - 1, Math.round((sorted.length - 1) * p))
  return sorted[i]!
}

/** One shared stretch over every finite sample of every frame. */
export function sharedStretch(planes: readonly FramePlane[], lowP = 0.02, highP = 0.995): Stretch {
  const samples: number[] = []
  for (const plane of planes) {
    for (const value of plane.values) {
      if (Number.isFinite(value)) samples.push(value)
    }
  }
  samples.sort((a, b) => a - b)
  if (samples.length === 0) return { low: 0, high: 1, span: 1 }
  const low = percentile(samples, lowP)
  const high = percentile(samples, highP)
  const span = high > low ? high - low : 1
  return { low, high, span }
}

/** Manual stretch bounds; falls back to auto when either end is missing. */
export function resolveStretch(auto: Stretch, low: number | null, high: number | null): Stretch {
  const lo = low === null || !Number.isFinite(low) ? auto.low : low
  const hi = high === null || !Number.isFinite(high) ? auto.high : high
  const span = hi > lo ? hi - lo : 1
  return { low: lo, high: hi, span }
}

export function tone(value: number, stretch: Stretch, display: DisplayTone = TONE_SOFT): number {
  let t = Math.min(1, Math.max(0, (value - stretch.low) / stretch.span))
  if (display.gamma !== 1) t = Math.pow(t, display.gamma)
  if (display.invert) t = 1 - t
  return Math.round(t * 255)
}

function writeGrey(rgb: Uint8ClampedArray, p: number, grey: number) {
  rgb[p] = grey
  rgb[p + 1] = grey
  rgb[p + 2] = grey
  rgb[p + 3] = 255
}

function writeNoData(rgb: Uint8ClampedArray, p: number) {
  rgb[p] = NO_DATA_RGB[0]
  rgb[p + 1] = NO_DATA_RGB[1]
  rgb[p + 2] = NO_DATA_RGB[2]
  rgb[p + 3] = 255
}

function writeRgb(rgb: Uint8ClampedArray, p: number, r: number, g: number, b: number) {
  rgb[p] = r
  rgb[p + 1] = g
  rgb[p + 2] = b
  rgb[p + 3] = 255
}

/** Paint one plane through a shared stretch. Missing samples become no-data. */
export function paintPlane(plane: FramePlane, stretch: Stretch, out: ImageData, display: DisplayTone = TONE_SOFT): void {
  const rgb = out.data
  for (let i = 0, p = 0; i < plane.values.length; i++, p += 4) {
    const value = plane.values[i]!
    if (!Number.isFinite(value)) {
      writeNoData(rgb, p)
      continue
    }
    writeGrey(rgb, p, tone(value, stretch, display))
  }
}

/**
 * Half-opacity onion skin: average of two toned frames.
 * Fixed sky stays put; a mover leaves a double image / smear your eye catches while blinking.
 */
export function paintOnion(
  a: FramePlane,
  b: FramePlane,
  stretch: Stretch,
  out: ImageData,
  display: DisplayTone = TONE_SOFT,
  mix = 0.5,
): void {
  const rgb = out.data
  const w = Math.min(1, Math.max(0, mix))
  for (let i = 0, p = 0; i < a.values.length; i++, p += 4) {
    const left = a.values[i]!
    const right = b.values[i]!
    const leftOk = Number.isFinite(left)
    const rightOk = Number.isFinite(right)
    if (!leftOk && !rightOk) {
      writeNoData(rgb, p)
      continue
    }
    if (!leftOk) {
      writeGrey(rgb, p, tone(right, stretch, display))
      continue
    }
    if (!rightOk) {
      writeGrey(rgb, p, tone(left, stretch, display))
      continue
    }
    const ga = tone(left, stretch, display)
    const gb = tone(right, stretch, display)
    writeGrey(rgb, p, Math.round(ga * (1 - w) + gb * w))
  }
}

/**
 * Swipe compare: `a` left of the divider, `b` right of it, on one stretch and
 * one sky grid, so catalog markers and clicks stay aligned across the split.
 * `split` is the divider position as a fraction of the width.
 */
export function paintSwipe(
  a: FramePlane,
  b: FramePlane,
  stretch: Stretch,
  out: ImageData,
  display: DisplayTone = TONE_SOFT,
  split = 0.5,
): void {
  const rgb = out.data
  const edge = Math.round(Math.min(1, Math.max(0, split)) * a.width)
  for (let i = 0, p = 0; i < a.values.length; i++, p += 4) {
    const x = i % a.width
    if (x === edge && edge > 0 && edge < a.width) {
      writeRgb(rgb, p, MOTION_RGB[0], MOTION_RGB[1], MOTION_RGB[2])
      continue
    }
    const value = (x < edge ? a : b).values[i]!
    if (!Number.isFinite(value)) {
      writeNoData(rgb, p)
      continue
    }
    writeGrey(rgb, p, tone(value, stretch, display))
  }
}

/** Calibrated difference painted as mid-grey zero. With invert off, white is
 * brighter in `a`; invert flips that, matching WiseView difference blinks. */
export function paintDifference(
  a: FramePlane,
  b: FramePlane,
  stretch: Stretch,
  out: ImageData,
  display: DisplayTone = TONE_SOFT,
  gain = 1.5,
): void {
  const rgb = out.data
  const half = stretch.span / 2
  for (let i = 0, p = 0; i < a.values.length; i++, p += 4) {
    const left = a.values[i]!
    const right = b.values[i]!
    if (!Number.isFinite(left) || !Number.isFinite(right)) {
      writeNoData(rgb, p)
      continue
    }
    let t = 0.5 + ((left - right) / half) * 0.5 * gain
    t = Math.min(1, Math.max(0, t))
    if (display.invert) t = 1 - t
    writeGrey(rgb, p, Math.round(t * 255))
  }
}

/**
 * Motion checker: greyscale base from epoch A, lime glow where |A−B| exceeds
 * a fraction of the shared stretch. Fixed sky is quiet; movers light up.
 */
export function paintMotionGlow(
  a: FramePlane,
  b: FramePlane,
  stretch: Stretch,
  out: ImageData,
  display: DisplayTone = TONE_SOFT,
  /** Threshold as a fraction of stretch.span (0.05 ≈ 5% of the display range). */
  thresholdFrac = 0.08,
  gain = 2,
): void {
  const rgb = out.data
  const threshold = stretch.span * thresholdFrac
  for (let i = 0, p = 0; i < a.values.length; i++, p += 4) {
    const left = a.values[i]!
    const right = b.values[i]!
    if (!Number.isFinite(left) || !Number.isFinite(right)) {
      writeNoData(rgb, p)
      continue
    }
    const grey = tone(left, stretch, display)
    const delta = Math.abs(left - right)
    if (delta < threshold) {
      writeGrey(rgb, p, grey)
      continue
    }
    const strength = Math.min(1, ((delta - threshold) / stretch.span) * gain)
    writeRgb(
      rgb,
      p,
      Math.round(grey * (1 - strength) + MOTION_RGB[0] * strength),
      Math.round(grey * (1 - strength) + MOTION_RGB[1] * strength),
      Math.round(grey * (1 - strength) + MOTION_RGB[2] * strength),
    )
  }
}

/** Equal-weight mean of the planes that have a finite sample at each pixel. */
export function paintSum(planes: readonly FramePlane[], stretch: Stretch, out: ImageData, display: DisplayTone = TONE_SOFT): void {
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
      writeNoData(rgb, p)
      continue
    }
    writeGrey(rgb, p, tone(total / count, stretch, display))
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
