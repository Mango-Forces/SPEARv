// @ts-nocheck
import assert from "node:assert/strict"
import { describe, it } from "node:test"
import {
  centreCoverage,
  paintDifference,
  paintMotionGlow,
  paintOnion,
  paintPlane,
  paintSum,
  resolveStretch,
  sharedStretch,
  tone,
  TONE_SOFT,
  TONE_WISEVIEW,
} from "./calibrate.ts"

function plane(width: number, height: number, fill: number | ((i: number) => number)) {
  const values = new Float32Array(width * height)
  for (let i = 0; i < values.length; i++) values[i] = typeof fill === "function" ? fill(i) : fill
  return { width, height, values }
}

function image(n = 1) {
  return { data: new Uint8ClampedArray(n * 4), width: n, height: 1 }
}

describe("calibrated display", () => {
  it("one stretch covers every frame", () => {
    const a = plane(2, 2, (i) => (i === 0 ? Number.NaN : 1 + i))
    const b = plane(2, 2, 10)
    const stretch = sharedStretch([a, b])
    assert.ok(stretch.high > stretch.low)
    assert.equal(tone(stretch.low, stretch, TONE_SOFT), 0)
    assert.equal(tone(stretch.high, stretch, TONE_SOFT), 255)
  })

  it("WiseView tone is linear and inverted", () => {
    const stretch = { low: 0, high: 10, span: 10 }
    assert.equal(tone(0, stretch, TONE_WISEVIEW), 255)
    assert.equal(tone(10, stretch, TONE_WISEVIEW), 0)
    assert.equal(tone(5, stretch, TONE_WISEVIEW), 128)
  })

  it("manual stretch overrides the auto ends", () => {
    const auto = { low: 0, high: 10, span: 10 }
    assert.deepEqual(resolveStretch(auto, 2, 8), { low: 2, high: 8, span: 6 })
    assert.deepEqual(resolveStretch(auto, null, 8), { low: 0, high: 8, span: 8 })
  })

  it("paints no-data as the shared grey-blue", () => {
    const p = plane(1, 1, Number.NaN)
    const out = image()
    paintPlane(p, { low: 0, high: 1, span: 1 }, out)
    assert.deepEqual([...out.data], [24, 28, 36, 255])
  })

  it("onion skin averages two equal frames to the same grey", () => {
    const p = plane(1, 1, 5)
    const stretch = { low: 0, high: 10, span: 10 }
    const out = image()
    paintOnion(p, p, stretch, out, TONE_SOFT, 0.5)
    assert.equal(out.data[0], tone(5, stretch, TONE_SOFT))
  })

  it("a difference of equal planes is mid-grey", () => {
    const p = plane(1, 1, 5)
    const out = image()
    paintDifference(p, p, { low: 0, high: 10, span: 10 }, out, TONE_SOFT, 1)
    assert.equal(out.data[0], 128)
  })

  it("motion glow stays grey when frames match and lights up when they differ", () => {
    const a = plane(1, 1, 5)
    const b = plane(1, 1, 5)
    const stretch = { low: 0, high: 10, span: 10 }
    const same = image()
    paintMotionGlow(a, b, stretch, same, TONE_SOFT, 0.08, 2)
    assert.equal(same.data[0], same.data[1])
    const moved = plane(1, 1, 9)
    const glow = image()
    paintMotionGlow(a, moved, stretch, glow, TONE_SOFT, 0.08, 2)
    assert.ok(glow.data[1]! > glow.data[0]!, "lime channel rises on motion")
  })

  it("the sum of two planes is their mean", () => {
    const a = plane(1, 1, 2)
    const b = plane(1, 1, 8)
    const stretch = { low: 0, high: 10, span: 10 }
    const out = image()
    paintSum([a, b], stretch, out, TONE_SOFT)
    assert.equal(out.data[0], tone(5, stretch, TONE_SOFT))
  })

  it("centre coverage ignores NaN in the aperture", () => {
    const p = plane(7, 7, Number.NaN)
    p.values[3 * 7 + 3] = 1
    assert.ok(centreCoverage(p) > 0)
    assert.ok(centreCoverage(p) < 1)
  })
})
