// @ts-nocheck
import assert from "node:assert/strict"
import { describe, it } from "node:test"
import { bandPins, detectorBands, frameSearch, stepReady, visitsInBounds } from "./frames.ts"

const VISITS = [
  { index: 0, mjd: 60800, mjd_end: 60815, exposures: 120 },
  { index: 1, mjd: 60980, mjd_end: 60995, exposures: 130 },
  { index: 2, mjd: 61160, mjd_end: 61170, exposures: 90 },
]

describe("epoch frames", () => {
  it("a pin expands to exactly the run it came from", () => {
    const expand = (pin: { center: number; width: number }) => {
      const start = pin.center - Math.floor((pin.width - 1) / 2)
      return Array.from({ length: pin.width }, (_, index) => start + index + 1)
    }
    for (const bands of [[1], [1, 2], detectorBands(1), detectorBands(6), Array.from({ length: 102 }, (_, i) => i + 1), [3, 4, 5, 6]]) {
      const pins = bandPins(bands)
      assert.equal(pins.length, 1)
      assert.deepEqual(expand(pins[0]), bands)
    }
    assert.deepEqual(bandPins([1, 2, 3, 44, 90, 91]), [{ center: 1, width: 3 }, { center: 43, width: 1 }, { center: 89, width: 2 }])
  })

  it("detectors split the 102 bands into six arrays of 17", () => {
    assert.deepEqual(detectorBands(1).slice(0, 2), [1, 2])
    assert.equal(detectorBands(6).at(-1), 102)
    assert.equal(detectorBands(3).length, 17)
  })

  it("the UTC window keeps overlapping visits only", () => {
    assert.equal(visitsInBounds(VISITS, { fromUtc: null, toUtc: null }).length, 3)
    // MJD 60990 is 2025-11-11.
    assert.deepEqual(visitsInBounds(VISITS, { fromUtc: "2025-11-11T00:00:00.000Z", toUtc: null }).map((visit) => visit.index), [1, 2])
    assert.deepEqual(visitsInBounds(VISITS, { fromUtc: null, toUtc: "2025-11-11T00:00:00.000Z" }).map((visit) => visit.index), [0, 1])
  })

  it("the frame query pins the visit once and keeps every band run", () => {
    const params = new URLSearchParams(frameSearch({ ra: 133.79513, dec: -7.24515, fovL: 0.5, fovH: 0.5, dr: "dr1", mask: true }, [1, 2, 3, 44], 4))
    assert.equal(params.get("mode"), "range")
    assert.deepEqual(params.getAll("pin"), ["1:3:v4:", "43:1"])
    assert.equal(params.get("ra"), "133.79513")
  })

  it("stepping wraps over the ready frames", () => {
    assert.equal(stepReady([0, 2, 3], 0, 1), 2)
    assert.equal(stepReady([0, 2, 3], 3, 1), 0)
    assert.equal(stepReady([0, 2, 3], 0, -1), 3)
    assert.equal(stepReady([], 1, 1), 1)
  })
})
