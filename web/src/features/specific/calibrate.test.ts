// @ts-nocheck
import assert from "node:assert/strict"
import { describe, it } from "node:test"
import { centreCoverage, paintDifference, paintPlane, paintSum, sharedStretch, tone } from "./calibrate.ts"

function plane(width: number, height: number, fill: number | ((i: number) => number)) {
  const values = new Float32Array(width * height)
  for (let i = 0; i < values.length; i++) values[i] = typeof fill === "function" ? fill(i) : fill
  return { width, height, values }
}

describe("calibrated display", () => {
  it("one stretch covers every frame", () => {
    const a = plane(2, 2, (i) => (i === 0 ? Number.NaN : 1 + i))
    const b = plane(2, 2, 10)
    const stretch = sharedStretch([a, b])
    assert.ok(stretch.high > stretch.low)
    assert.equal(tone(stretch.low, stretch), 0)
    assert.equal(tone(stretch.high, stretch), 255)
  })

  it("paints no-data as the shared grey-blue", () => {
    const p = plane(1, 1, Number.NaN)
    const out = { data: new Uint8ClampedArray(4), width: 1, height: 1 } as ImageData
    paintPlane(p, { low: 0, high: 1, span: 1 }, out)
    assert.deepEqual([...out.data], [24, 28, 36, 255])
  })

  it("a difference of equal planes is mid-grey", () => {
    const p = plane(1, 1, 5)
    const out = { data: new Uint8ClampedArray(4), width: 1, height: 1 } as ImageData
    paintDifference(p, p, { low: 0, high: 10, span: 10 }, out, 1)
    assert.equal(out.data[0], 128)
  })

  it("the sum of two planes is their mean", () => {
    const a = plane(1, 1, 2)
    const b = plane(1, 1, 8)
    const stretch = { low: 0, high: 10, span: 10 }
    const out = { data: new Uint8ClampedArray(4), width: 1, height: 1 } as ImageData
    paintSum([a, b], stretch, out)
    assert.equal(out.data[0], tone(5, stretch))
  })

  it("centre coverage ignores NaN in the aperture", () => {
    const p = plane(7, 7, Number.NaN)
    p.values[3 * 7 + 3] = 1
    assert.ok(centreCoverage(p) > 0)
    assert.ok(centreCoverage(p) < 1)
  })
})
