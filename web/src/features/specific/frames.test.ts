// @ts-nocheck
import assert from "node:assert/strict"
import { describe, it } from "node:test"
import {
  detectorBands,
  engineBands,
  exposuresInBounds,
  frameBody,
  qualityFlags,
  stepReady,
  visitsInBounds,
} from "./frames.ts"

const VISITS = [
  { index: 0, mjd: 60800, mjd_end: 60815, exposures: 120 },
  { index: 1, mjd: 60980, mjd_end: 60995, exposures: 130 },
  { index: 2, mjd: 61160, mjd_end: 61170, exposures: 90 },
]

describe("epoch frames", () => {
  it("detectors split the 102 bands into six arrays of 17", () => {
    assert.deepEqual(detectorBands(1).slice(0, 2), [1, 2])
    assert.equal(detectorBands(6).at(-1), 102)
    assert.deepEqual(engineBands([1, 2, 17]), [0, 1, 16])
  })

  it("the UTC window keeps overlapping visits only", () => {
    assert.equal(visitsInBounds(VISITS, { fromUtc: null, toUtc: null }).length, 3)
    assert.deepEqual(
      visitsInBounds(VISITS, { fromUtc: "2025-11-11T00:00:00.000Z", toUtc: null }).map((visit) => visit.index),
      [1, 2],
    )
  })

  it("the frame body carries engine bands and either a visit or exposure ids", () => {
    const view = { ra: 133.79513, dec: -7.24515, fovL: 0.5, fovH: 0.5, dr: "dr1", mask: true }
    assert.deepEqual(frameBody(view, [1, 2, 3], { visit: 4 }).bands, [0, 1, 2])
    assert.equal(frameBody(view, [1], { visit: 4 }).visit, 4)
    assert.deepEqual(frameBody(view, [1], { exposures: ["a", "b"] }).exposures, ["a", "b"])
  })

  it("stepping wraps over the ready frames", () => {
    assert.equal(stepReady([0, 2, 3], 0, 1), 2)
    assert.equal(stepReady([0, 2, 3], 3, 1), 0)
    assert.equal(stepReady([], 1, 1), 1)
  })

  it("quality flags name only the dirty cards", () => {
    assert.deepEqual(qualityFlags({ dqa_l1: "Pass", dqa_l2: "Pass", aurora: false, n_streaks: 0, finast: 0 }), [])
    assert.deepEqual(qualityFlags({ dqa_l1: "Fail", aurora: true, n_streaks: 2 }), ["L1 Fail", "aurora", "2 streaks"])
  })

  it("exposure date filter uses the exposure MJD", () => {
    const rows = [
      { id: "a", obsid: "a", detector: 1, version: "v", mjd: 60800, visit: 0, wl_min_um: 1, wl_max_um: 2 },
      { id: "b", obsid: "b", detector: 1, version: "v", mjd: 61160, visit: 2, wl_min_um: 1, wl_max_um: 2 },
    ]
    assert.deepEqual(
      exposuresInBounds(rows, { fromUtc: "2025-11-11T00:00:00.000Z", toUtc: null }).map((row) => row.id),
      ["b"],
    )
  })
})
