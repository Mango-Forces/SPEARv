// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import { formatBands, parseBands, parseBounds } from "./bounds.ts"

describe("band filters", () => {
  it("parses and compresses semicolon-delimited ranges", () => {
    assert.deepEqual(parseBands("1-8;44"), [1, 2, 3, 4, 5, 6, 7, 8, 44])
    assert.equal(formatBands([44, 8, 7, 6, 5, 4, 3, 2, 1]), "1-8;44")
    assert.deepEqual(parseBands("44"), [44])
  })

  it("keeps band validation errors actionable", () => {
    assert.throws(() => parseBands("1–8;44"), /1-8;44/)
    assert.throws(() => parseBands("0"), /1-8;44/)
    assert.throws(() => parseBands("103"), /1-8;44/)
  })

  it("preserves UTC milliseconds and open ends in applied bounds", () => {
    assert.deepEqual(parseBounds({ fromUtc: "12 Nov 2026, 14:30 UTC", toUtc: "", releases: ["qr2", "qr3", "dr1"], bands: "1-8;44" }), {
      fromUtc: "2026-11-12T14:30:00.000Z", toUtc: null, releases: ["dr1", "qr2", "qr3"], bands: [1, 2, 3, 4, 5, 6, 7, 8, 44],
    })
  })
})
