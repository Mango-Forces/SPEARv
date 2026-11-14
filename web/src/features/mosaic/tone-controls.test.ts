// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import { colorPointError, DEFAULT_DISPLAY, displayOf, resolveDisplay, toneBlurCommit, withDisplay, type BlackWhite } from "../../url-state/display.ts"
import { readView, viewSearch } from "../../url-state/view.ts"

const base = readView("?mode=specific")
const manual: BlackWhite = { mode: "manual", low: -4, high: 9 }
const withManualRed = { ...DEFAULT_DISPLAY, color: { ...DEFAULT_DISPLAY.color, r: { ...DEFAULT_DISPLAY.color.r, blackWhite: manual } } }

describe("ToneControls blur commit guard", () => {
  it("leaves untouched Auto points Auto even when their shown values have six digits", () => {
    assert.deepEqual(toneBlurCommit("3588.23", "5000.12", false), { kind: "ignore" })
    assert.equal(displayOf(base).color.r.blackWhite.mode, "auto")
  })
  it("commits and persists an intentional manual edit", () => {
    const result = toneBlurCommit("12.5", "40", true)
    assert.deepEqual(result, { kind: "manual", blackWhite: { mode: "manual", low: 12.5, high: 40 } })
    const display = { ...DEFAULT_DISPLAY, color: { ...DEFAULT_DISPLAY.color, r: { ...DEFAULT_DISPLAY.color.r, blackWhite: result.blackWhite } } }
    const url = viewSearch(withDisplay(base, display))
    assert.equal(new URLSearchParams(url).get("bw_r"), "12.5,40")
    assert.deepEqual(displayOf(readView(`?${url}`)).color.r.blackWhite, { mode: "manual", low: 12.5, high: 40 })
  })
  it("does nothing when an unchanged Manual field loses focus", () => {
    assert.deepEqual(toneBlurCommit("-4", "9", false), { kind: "ignore" })
    const url = viewSearch(withDisplay(base, withManualRed))
    assert.equal(new URLSearchParams(url).get("bw_r"), "-4,9")
    assert.deepEqual(displayOf(readView(`?${url}`)).color.r.blackWhite, manual)
  })
  it("rejects blank, nonfinite, equal and inverted points without committing", () => {
    for (const [black, white] of [["", "9"], ["1", ""], ["NaN", "9"], ["1", "Infinity"], ["9", "9"], ["9", "1"]]) {
      const result = toneBlurCommit(black, white, true)
      assert.equal(result.kind, "error", `${black}..${white}`)
      assert.match(result.message, /finite points/)
    }
  })
  it("returns to Auto while genuine manual URL state survives a new job", () => {
    const url = viewSearch(withDisplay(base, withManualRed))
    assert.equal(new URLSearchParams(url).get("bw_r"), "-4,9")
    const autoDisplay = { ...withManualRed, color: { ...withManualRed.color, r: { ...withManualRed.color.r, blackWhite: { mode: "auto" } } } }
    assert.equal(new URLSearchParams(viewSearch(withDisplay(base, autoDisplay))).has("bw_r"), false)

    const restored = displayOf(readView(`?${url}`))
    assert.deepEqual(restored.color.r.blackWhite, manual)
    const metadata = { source_recipe: { mode: "color" }, channels: [{ index: 2, default_black: 0, default_span: 1 }] }
    const resolved = resolveDisplay(restored, metadata)
    assert.deepEqual([resolved.layers[0].low, resolved.layers[0].span], [-4, 13])
  })
})

describe("Color manual point policy", () => {
  it("rejects the ranges the server cannot keep as f32", () => {
    for (const [low, high] of [[1e100, 1.1e100], [0, 1e39], [0, 1e-100], [-1e100, 0], [1, 1]]) {
      assert.ok(colorPointError(low, high), `${low}..${high}`)
    }
  })
  it("keeps the ranges the server can keep as f32", () => {
    for (const [low, high] of [[-4, 9], [0, 1e38], [0, 3.4e38], [-1e-45, 0]]) {
      assert.equal(colorPointError(low, high), undefined, `${low}..${high}`)
    }
  })
  it("blocks an invalid Color edit before it reaches the display state", () => {
    const rejected = toneBlurCommit("1e100", "1.1e100", true, colorPointError)
    assert.equal(rejected.kind, "error")
    assert.match(rejected.message, /32-bit/)
    assert.deepEqual(toneBlurCommit("200", "1000", true, colorPointError), {
      kind: "manual",
      blackWhite: { mode: "manual", low: 200, high: 1000 },
    })
  })
  it("leaves the Composite rule unchanged", () => {
    assert.deepEqual(toneBlurCommit("1e100", "1.1e100", true), {
      kind: "manual",
      blackWhite: { mode: "manual", low: 1e100, high: 1.1e100 },
    })
  })
  it("keeps a stored manual range the server rejects and still reports it", () => {
    const stored = { ...DEFAULT_DISPLAY, color: { ...DEFAULT_DISPLAY.color, r: { ...DEFAULT_DISPLAY.color.r, blackWhite: { mode: "manual", low: 1e100, high: 1.1e100 } } } }
    const url = viewSearch(withDisplay(base, stored))
    const restored = displayOf(readView(`?${url}`)).color.r.blackWhite
    assert.deepEqual(restored, { mode: "manual", low: 1e100, high: 1.1e100 })
    assert.ok(colorPointError(restored.low, restored.high))
  })
})
