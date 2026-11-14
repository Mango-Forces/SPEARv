// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import { formatUtcClock, formatUtcDate, formatUtcText, parseUtcText } from "./date.ts"

describe("fixed UTC date text", () => {
  it("accepts human minute, second, and millisecond precision", () => {
    assert.equal(parseUtcText("12 Nov 2026, 14:30 UTC"), "2026-11-12T14:30:00.000Z")
    assert.equal(parseUtcText("12 Nov 2026, 14:30:05 UTC"), "2026-11-12T14:30:05.000Z")
    assert.equal(parseUtcText("12 November 2026 14:30:05.123 Z"), "2026-11-12T14:30:05.123Z")
    assert.equal(parseUtcText(""), null)
  })

  it("accepts date-only ISO at the start of its UTC day", () => {
    assert.equal(parseUtcText("2026-06-01"), "2026-06-01T00:00:00.000Z")
    assert.equal(parseUtcText("2026-06-01", "end"), "2026-06-01T23:59:59.999Z")
  })

  it("keeps canonical ISO input byte-identical and pads older fractions", () => {
    assert.equal(parseUtcText("2026-11-12T14:30:05.123Z"), "2026-11-12T14:30:05.123Z")
    assert.equal(parseUtcText("2026-11-12T14:30:05.1Z"), "2026-11-12T14:30:05.100Z")
  })

  it("formats compact UTC text without dropping non-zero precision", () => {
    assert.equal(formatUtcText("2026-11-12T14:30:00.000Z"), "12 Nov 2026, 14:30 UTC")
    assert.equal(formatUtcText("2026-11-12T14:30:05.123Z"), "12 Nov 2026, 14:30:05.123 UTC")
    assert.equal(formatUtcDate("2026-11-12T14:30:05.123Z"), "12 Nov 2026")
    assert.equal(formatUtcClock("2026-11-12T14:30:05.123Z"), "14:30:05.123 UTC")
    assert.equal(formatUtcText("2026-11-12T14:30:05Z"), "12 Nov 2026, 14:30:05 UTC")
  })

  it("rejects ambiguous, non-UTC, and impossible values", () => {
    for (const value of ["11/12/2026", "12 Nov 2026", "14:30 UTC", "12 Nov 2026, 14:30", "12 Nov 2026, 14:30 +01:00", "31 Feb 2026, 14:30 UTC", "2026-02-30T14:30:00.000Z"]) {
      assert.throws(() => parseUtcText(value), value)
    }
  })
})
