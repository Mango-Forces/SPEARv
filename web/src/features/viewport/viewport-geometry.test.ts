// @ts-nocheck
import { describe, it } from "node:test"
import assert from "node:assert/strict"
import {
  MAX_ZOOM,
  MIN_ZOOM,
  VISIBLE_MARGIN,
  clampZoom,
  measureGeometry,
  panGeometry,
  rebaseDrag,
  zoomGeometry,
} from "./viewport-geometry.ts"

describe("viewport geometry", () => {
  it("fits and centers native dimensions on initial measure or native change", () => {
    const initial = measureGeometry(undefined, { w: 400, h: 200 }, 800, 600, false)
    assert.equal(initial.fitted, true)
    assert.equal(initial.geometry.baseScale, 2)
    assert.equal(initial.geometry.zoom, 1)
    assert.equal(initial.geometry.tx, 0)
    assert.equal(initial.geometry.ty, 100)

    const changed = measureGeometry(initial.geometry, { w: 200, h: 200 }, 800, 600, false)
    assert.equal(changed.fitted, true)
    assert.equal(changed.geometry.baseScale, 3)
    assert.equal(changed.geometry.tx, 100)
    assert.equal(changed.geometry.ty, 0)
  })

  it("pans smoothly from the drag anchor and clamps within visible margins", () => {
    const { geometry } = measureGeometry(undefined, { w: 400, h: 400 }, 800, 600, false)
    const drag = { pointerId: 1, x: 100, y: 100, tx: geometry.tx, ty: geometry.ty, lastX: 100, lastY: 100 }
    const panned = panGeometry(geometry, drag, 145, 80)
    assert.equal(panned.tx, geometry.tx + 45)
    assert.equal(panned.ty, geometry.ty - 20)

    const clampedRight = panGeometry(geometry, drag, 5000, 100)
    assert.equal(clampedRight.tx, 800 - VISIBLE_MARGIN)

    const clampedLeft = panGeometry(geometry, drag, -5000, 100)
    const displayedW = geometry.nativeW * geometry.baseScale * geometry.zoom
    assert.equal(clampedLeft.tx, VISIBLE_MARGIN - displayedW)
  })

  it("keeps the cursor native pixel anchored while zooming and clamps zoom bounds", () => {
    assert.equal(clampZoom(0.01), MIN_ZOOM)
    assert.equal(clampZoom(999), MAX_ZOOM)

    const { geometry } = measureGeometry(undefined, { w: 400, h: 400 }, 800, 800, false)
    const zoomed = zoomGeometry(geometry, 2, 300, 500)
    assert.equal(zoomed.zoom, 2)
    const scaleBefore = geometry.baseScale * geometry.zoom
    const scaleAfter = zoomed.baseScale * zoomed.zoom
    const nativeXBefore = (300 - geometry.tx) / scaleBefore
    const nativeYBefore = (500 - geometry.ty) / scaleBefore
    const nativeXAfter = (300 - zoomed.tx) / scaleAfter
    const nativeYAfter = (500 - zoomed.ty) / scaleAfter
    assert.ok(Math.abs(nativeXBefore - nativeXAfter) < 1e-9)
    assert.ok(Math.abs(nativeYBefore - nativeYAfter) < 1e-9)
  })

  it("preserves drag continuity across mid-drag wheel zoom without jumping back", () => {
    const { geometry } = measureGeometry(undefined, { w: 400, h: 400 }, 800, 800, false)
    let drag = { pointerId: 1, x: 200, y: 200, tx: geometry.tx, ty: geometry.ty, lastX: 200, lastY: 200 }

    drag = { ...drag, lastX: 250, lastY: 230 }
    const afterPan = panGeometry(geometry, drag, 250, 230)
    assert.equal(afterPan.tx, 50)
    assert.equal(afterPan.ty, 30)

    const afterWheel = zoomGeometry(afterPan, 2, 250, 230)
    drag = rebaseDrag(drag, afterWheel)

    drag = { ...drag, lastX: 265, lastY: 240 }
    const afterNextPan = panGeometry(afterWheel, drag, 265, 240)
    assert.equal(afterNextPan.tx, afterWheel.tx + 15)
    assert.equal(afterNextPan.ty, afterWheel.ty + 10)
  })

  it("preserves drag continuity across mid-drag viewport resize even after clamp", () => {
    const { geometry } = measureGeometry(undefined, { w: 400, h: 400 }, 800, 800, false)
    let drag = { pointerId: 1, x: 100, y: 100, tx: geometry.tx, ty: geometry.ty, lastX: 100, lastY: 100 }

    drag = { ...drag, lastX: 2000, lastY: 100 }
    const clampedPan = panGeometry(geometry, drag, 2000, 100)
    assert.equal(clampedPan.tx, 800 - VISIBLE_MARGIN)

    const resized = measureGeometry(clampedPan, { w: 400, h: 400 }, 1000, 800, false)
    assert.equal(resized.fitted, false)
    drag = rebaseDrag(drag, resized.geometry)

    drag = { ...drag, lastX: 1990, lastY: 110 }
    const afterResizePan = panGeometry(resized.geometry, drag, 1990, 110)
    assert.equal(afterResizePan.tx, resized.geometry.tx - 10)
    assert.equal(afterResizePan.ty, resized.geometry.ty + 10)
  })
})
