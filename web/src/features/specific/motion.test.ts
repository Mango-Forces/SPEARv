// @ts-nocheck
import assert from "node:assert/strict"
import { describe, it } from "node:test"
import { detectSources, heading, PIXEL_ARCSEC, robustStats, searchMotion } from "./motion.ts"

/** Deterministic noise so the field is the same every run. */
function rng(seed: number) {
  let s = seed >>> 0
  return () => {
    s = (s * 1664525 + 1013904223) >>> 0
    return s / 2 ** 32 - 0.5
  }
}

function field(size: number, stars: { x: number; y: number; flux: number }[], seed: number, mask?: (x: number, y: number) => boolean) {
  const noise = rng(seed)
  const values = new Float32Array(size * size)
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let v = 1 + noise() * 0.1
      for (const star of stars) v += star.flux * Math.exp(-((x - star.x) ** 2 + (y - star.y) ** 2) / (2 * 1.1 ** 2))
      values[y * size + x] = mask?.(x, y) ? Number.NaN : v
    }
  }
  return { width: size, height: size, values }
}

const FIXED = [
  { x: 10.3, y: 12.6, flux: 5 },
  { x: 40.2, y: 9.1, flux: 3 },
  { x: 33.7, y: 44.4, flux: 8 },
  { x: 8.8, y: 48.2, flux: 4 },
  { x: 50.1, y: 30.5, flux: 6 },
]

describe("motion search", () => {
  it("robust stats ignore NaN and outliers", () => {
    const stats = robustStats(Float32Array.from([1, 1, 1, 1, 100, Number.NaN]))
    assert.equal(stats.median, 1)
    assert.equal(stats.sigma, 0)
  })

  it("detects each star once near its true centre", () => {
    const sources = detectSources(field(59, FIXED, 1))
    assert.equal(sources.length, FIXED.length)
    for (const star of FIXED) {
      assert.ok(sources.some((source) => Math.hypot(source.x - star.x, source.y - star.y) < 0.3))
    }
  })

  it("merges a masked saturated core into one source", () => {
    const star = { x: 29.4, y: 30.2, flux: 400 }
    const plane = field(59, [star], 2, (x, y) => Math.hypot(x - 29, y - 30) < 1.2)
    const sources = detectSources(plane)
    assert.equal(sources.length, 1)
    assert.ok(sources[0].saturated)
    assert.ok(Math.hypot(sources[0].x - star.x, sources[0].y - star.y) < 0.6)
  })

  it("finds the one mover and leaves fixed stars alone", () => {
    // 1.2 px per year to the right (west) and slightly up (north).
    const epochs = [0, 0.5, 1, 1.5, 2].map((years, at) => ({
      years,
      plane: field(59, [...FIXED, { x: 20 + 1.2 * years, y: 28 - 0.2 * years, flux: 4 }], 10 + at),
    }))
    const search = searchMotion(epochs)
    assert.equal(search.tracked, FIXED.length + 1)
    assert.equal(search.movers.length, 1)
    const mover = search.movers[0]
    assert.ok(Math.abs(mover.pmRa - -1.2 * PIXEL_ARCSEC) < 0.6, `pmRa ${mover.pmRa}`)
    assert.ok(Math.abs(mover.pmDec - 0.2 * PIXEL_ARCSEC) < 0.6, `pmDec ${mover.pmDec}`)
    assert.equal(heading(mover.pmRa, mover.pmDec), "W")
    assert.equal(mover.track.length, 5)
  })

  it("needs four epochs before it fits anything", () => {
    const epochs = [0, 1, 2].map((years) => ({ years, plane: field(59, [...FIXED, { x: 20 + 2 * years, y: 28, flux: 4 }], 3 + years) }))
    assert.deepEqual(searchMotion(epochs).movers, [])
  })

  it("one masked ring is one star even when its peaks sit far apart", () => {
    const star = { x: 29.5, y: 30.0, flux: 600 }
    const plane = field(59, [star], 4, (x, y) => Math.hypot(x - 29.5, y - 30) < 3.6)
    const sources = detectSources(plane)
    assert.equal(sources.length, 1)
    assert.ok(Math.hypot(sources[0].x - star.x, sources[0].y - star.y) < 0.6)
  })

  it("ignores faint tracks inside a much brighter star's halo", () => {
    const epochs = [0, 0.5, 1, 1.5, 2].map((years, at) => ({
      years,
      plane: field(59, [...FIXED, { x: 30, y: 30, flux: 2000 }, { x: 37 + 1.2 * years, y: 30, flux: 3 }], 20 + at),
    }))
    assert.ok(searchMotion(epochs).movers.every((mover) => Math.hypot(mover.x - 30, mover.y - 30) >= 10))
  })

  it("names compass headings with east positive", () => {
    assert.equal(heading(0, 1), "N")
    assert.equal(heading(1, 0), "E")
    assert.equal(heading(-1, 0), "W")
    assert.equal(heading(0, -1), "S")
  })
})
