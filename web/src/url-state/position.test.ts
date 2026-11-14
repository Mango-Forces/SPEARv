// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import { FOV_WARN_DEG, MAX_FOV, formatNumber, isLargeField, parseDec, parseFov, parseRa } from "./position.ts"

describe("sky position entry", () => {
  it("reads RA as decimal degrees or hours", () => {
    assert.equal(parseRa("20h 30m 32.0s"), 307.6333333333333)
    assert.equal(parseRa("20:30:32"), 307.6333333333333)
    assert.equal(parseRa("56.75"), 56.75)
    assert.equal(parseRa("24h"), undefined)
    assert.equal(parseRa(""), undefined)
  })

  it("reads Dec as decimal degrees or sexagesimal, keeping the sign", () => {
    assert.equal(parseDec("40° 17' 50.2\""), 40.29727777777778)
    assert.equal(parseDec("−40° 17′ 50.2″"), -40.29727777777778)
    assert.equal(parseDec("40d 17m 50.2s"), 40.29727777777778)
    assert.equal(parseDec("24.12"), 24.12)
    assert.equal(parseDec("90:00:01"), undefined)
  })

  it("caps the field of view at 30° per axis and warns above 10°", () => {
    assert.equal(MAX_FOV, 30)
    assert.equal(FOV_WARN_DEG, 10)
    assert.equal(parseFov("10"), 10)
    assert.equal(parseFov("30"), 30)
    assert.equal(parseFov("30.1"), undefined)
    assert.equal(parseFov("0"), undefined)
    assert.equal(isLargeField(10), false)
    assert.equal(isLargeField(10.0001), true)
  })

  it("formats decimals with six places and no trailing zeros", () => {
    assert.equal(formatNumber(56.75), "56.75")
    assert.equal(formatNumber(0.1 + 0.2), "0.3")
    assert.equal(formatNumber(-24.1234567), "-24.123457")
  })
})
