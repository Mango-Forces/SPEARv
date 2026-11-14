// Proves the marker, status and layout fixes against a real SPEARv server.
//
//   SPEARV_BASE_URL=http://127.0.0.1:8123 \
//   PLAYWRIGHT_PATH=/home/ubuntu/e2e-tools/node_modules/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/home/ubuntu/.cache/ms-playwright \
//   node web/e2e/markers-status.mjs
//
// Screenshots go to SPEARV_SCREENSHOT_DIR (default
// /tmp/spearv-verify/markers-status). The server must already be running with
// SPEARV_INDEX, SPEARV_GAIA and SPEARV_WATCH_SECS=0.

import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir } from "node:fs/promises"
import { closeDrawer, layerRow, openDrawer, toggleLayer } from "./lib/ui.mjs"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8123"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-verify/markers-status"
await mkdir(shots, { recursive: true })

const delta = new URLSearchParams([
  ["ra", "58.2783"],
  ["dec", "26.1398"],
  ["fov", "0.1"],
  ["dr", "qr2"],
  ["mode", "delta"],
  ["mask", "on"],
  ["pin", "25:1:v0:+"],
  ["pin", "25:1:v1:-"],
])
const composite = new URLSearchParams([
  ["ra", "56.75"],
  ["dec", "24.12"],
  ["fov", "0.02"],
  ["dr", "qr3"],
  ["mode", "composite"],
  ["mask", "on"],
  ["pin", "42:1"],
])
// Renders, but the Gaia store stops 3.3 degrees away.
const noGaia = new URLSearchParams([
  ["ra", "54.4515"],
  ["dec", "21.5424"],
  ["fov", "0.05"],
  ["dr", "qr2"],
  ["mode", "slider"],
  ["mask", "on"],
  ["pin", "42:1"],
])

function appUrl(params, catalogs) {
  const query = new URLSearchParams(params)
  if (catalogs) {
    query.set("gaia", "0.9")
    query.set("wise", "0.6")
  }
  return `/?${query.toString()}`
}

function renderUrl(params) {
  const query = new URLSearchParams(params)
  if (query.get("mode") === "delta") {
    query.delete("mode")
    return `/api/v1/delta.png?${query.toString()}`
  }
  return `/api/v1/render.png?${query.toString()}`
}

async function warm(params) {
  const response = await fetch(`${base}${renderUrl(params)}`)
  if (!response.ok) throw new Error(`warm ${renderUrl(params)}: HTTP ${response.status}`)
  await response.arrayBuffer()
}

console.log("warming the server cache")
await warm(delta)
await warm(composite)
await warm(noGaia)
// Warm both catalogs too: the WISE read is the slow part of the first load.
for (const params of [delta, composite]) {
  const query = new URLSearchParams(params)
  const search = `${query.get("ra")}&dec=${query.get("dec")}&fov=${query.get("fov")}`
  for (const source of ["gaia", "wise"]) {
    const response = await fetch(`${base}/api/v1/catalog/${source}?ra=${search}`)
    if (!response.ok && response.status !== 404) throw new Error(`warm ${source}: HTTP ${response.status}`)
    await response.arrayBuffer()
  }
}

const browser = await chromium.launch()

/** Opens the app at one viewport and waits for a real loaded image. */
async function open(params, viewport, options = {}) {
  const context = await browser.newContext({ viewport })
  const page = await context.newPage()
  const renders = []
  page.on("request", (request) => {
    // A mosaic start is the one request a picture makes, whatever its mode.
    if (/\/api\/v1\/mosaic\?/.test(request.url())) renders.push(request.url())
  })
  if (options.failWise) {
    await page.route("**/api/v1/catalog/wise*", (route) => route.abort("failed"))
  }
  const url = `${base}${appUrl(params, options.catalogs)}`
  await page.goto(url, { waitUntil: "domcontentloaded", timeout: 60000 })
  await page.waitForFunction(
    () => {
      const canvas = document.querySelector(".overlay-frame canvas")
      const status = document.querySelector(".status")?.textContent ?? ""
      return Boolean(canvas && canvas.width > 0 && status.startsWith("ready"))
    },
    null,
    { timeout: 120000 },
  )
  // Wait past several 500 ms status ticks: none may overwrite "ready".
  await page.waitForTimeout(1400)
  const status = await page.locator(".status").innerText()
  assert.match(status, /^ready/, `status did not stay ready: ${status}`)
  return { context, page, renders }
}

async function shot(page, name, fullPage) {
  await page.screenshot({ path: `${shots}/${name}.png`, fullPage })
  console.log(`saved ${name}.png`)
}

async function markerReport(page) {
  return page.evaluate(() => {
    const shape = (selector) => {
      const element = document.querySelector(selector)
      if (!element) return null
      const rect = element.getBoundingClientRect()
      return {
        tag: element.tagName,
        width: Number(rect.width.toFixed(2)),
        height: Number(rect.height.toFixed(2)),
        fill: getComputedStyle(element).fill,
      }
    }
    return {
      gaia: shape(".markers.gaia .marker-shape"),
      wise: shape(".markers.wise .marker-shape"),
      gaiaCount: document.querySelectorAll(".markers.gaia .marker").length,
      wiseCount: document.querySelectorAll(".markers.wise .marker").length,
    }
  })
}

async function layoutReport(page) {
  return page.evaluate(() => {
    const image = document.querySelector(".overlay-frame canvas").getBoundingClientRect()
    const row = document.querySelector(".frame-row").getBoundingClientRect()
    const viewer = document.querySelector(".viewer").getBoundingClientRect()
    return {
      image: { w: Number(image.width.toFixed(1)), h: Number(image.height.toFixed(1)) },
      row: { w: Number(row.width.toFixed(1)), h: Number(row.height.toFixed(1)) },
      viewer: { w: Number(viewer.width.toFixed(1)), h: Number(viewer.height.toFixed(1)) },
    }
  })
}

// 1280 x 900: one request per view, small screen-sized markers, ready status.
{
  const { context, page, renders } = await open(delta, { width: 1280, height: 900 }, { catalogs: true })
  assert.equal(renders.length, 1, `delta made ${renders.length} render requests: ${renders.join(", ")}`)
  const markers = await markerReport(page)
  console.log("delta 1280 markers", markers)
  assert.ok(markers.gaiaCount > 0 && markers.wiseCount > 0, "catalog markers missing")
  assert.equal(markers.gaia.tag, "circle", "Gaia marker must be a circle")
  assert.equal(markers.wise.tag, "polygon", "WISE marker must be a diamond")
  assert.equal(markers.gaia.fill, "none", "Gaia marker must be hollow")
  assert.equal(markers.wise.fill, "none", "WISE marker must be hollow")
  assert.ok(markers.gaia.width <= 14, `Gaia marker is ${markers.gaia.width}px wide`)
  assert.ok(markers.wise.width <= 14, `WISE marker is ${markers.wise.width}px wide`)
  const layout = await layoutReport(page)
  console.log("delta 1280 layout", layout)
  assert.ok(layout.image.h >= layout.row.h - 2, `image ${layout.image.h} does not fill row ${layout.row.h}`)
  await shot(page, "delta-1280-catalogs", false)

  // Marker visibility is URL state; changing it must not re-render the image.
  await toggleLayer(page, "Gaia")
  await page.waitForTimeout(600)
  assert.equal(renders.length, 1, `hiding Gaia re-rendered the image: ${renders.join(", ")}`)
  assert.equal(await page.locator(".markers.gaia").count(), 0, "Gaia layer stayed visible")
  await closeDrawer(page, "Layers")

  // A real view change makes exactly one new request, and the old response
  // cannot leave its clock or status behind.
  await page.getByRole("textbox", { name: /^RA$/i }).fill("58.3")
  await page.getByRole("button", { name: /^Show$/i }).click()
  for (let i = 0; i < 240 && renders.length < 2; i++) await page.waitForTimeout(250)
  assert.equal(renders.length, 2, `view change made ${renders.length - 1} new requests: ${renders.join(", ")}`)
  await page.waitForFunction(
    () => (document.querySelector(".status")?.textContent ?? "").startsWith("ready"),
    null,
    { timeout: 120000 },
  )
  await page.waitForTimeout(1400)
  assert.match(await page.locator(".status").innerText(), /^ready/)
  await context.close()
}

{
  const { context, page, renders } = await open(delta, { width: 1280, height: 900 }, { catalogs: false })
  assert.equal(renders.length, 1, `plain delta made ${renders.length} render requests`)
  assert.equal(await page.locator(".markers").count(), 0, "plain delta drew markers")
  await shot(page, "delta-1280-plain", false)
  await context.close()
}

{
  const { context, page, renders } = await open(composite, { width: 1280, height: 900 }, { catalogs: true })
  assert.equal(renders.length, 1, `composite made ${renders.length} render requests`)
  const markers = await markerReport(page)
  console.log("composite 1280 markers", markers)
  assert.ok(markers.gaiaCount > 0 || markers.wiseCount > 0, "composite markers missing")
  await shot(page, "composite-1280-catalogs", false)
  await context.close()
}

{
  const { context, page, renders } = await open(composite, { width: 1280, height: 900 }, { catalogs: false })
  assert.equal(renders.length, 1, `plain composite made ${renders.length} render requests`)
  await shot(page, "composite-1280-plain", false)
  await context.close()
}

// 390 wide: the picture keeps the full width of its column. The new shell owns
// control collapsing through the right drawers, so this regression only checks
// the picture geometry that the drawer change must not disturb.
for (const [name, params, catalogs] of [
  ["delta-390-catalogs", delta, true],
  ["delta-390-plain", delta, false],
  ["composite-390-catalogs", composite, true],
  ["composite-390-plain", composite, false],
]) {
  const { context, page, renders } = await open(params, { width: 390, height: 844 }, { catalogs })
  assert.equal(renders.length, 1, `${name} made ${renders.length} render requests`)
  const layout = await layoutReport(page)
  console.log(`${name} layout`, layout)
  assert.ok(layout.image.w >= layout.row.w - 2, `image ${layout.image.w} does not fill row ${layout.row.w}`)
  assert.ok(layout.image.w >= 300, `phone image is only ${layout.image.w}px wide`)
  await shot(page, name, true)
  await context.close()
}

// A real 404 from the Gaia store is an empty layer, not a failed request.
{
  const { context, page, renders } = await open(noGaia, { width: 1280, height: 900 }, { catalogs: true })
  assert.equal(renders.length, 1, `no-Gaia view made ${renders.length} render requests`)
  await openDrawer(page, "Layers")
  const status = page.getByText(/no Gaia data here/i).first()
  await status.waitFor()
  assert.equal(await page.locator(".markers.gaia").count(), 0, "empty Gaia layer drew markers")
  assert.equal(await layerRow(page, "Gaia").locator(".overlay-failure").count(), 0)
  console.log("Gaia 404 reads:", await status.innerText())
  await shot(page, "gaia-no-data-1280", false)
  await context.close()
}

// A real 400 from the render route shows a failed status with the server text.
{
  const context = await browser.newContext({ viewport: { width: 1280, height: 900 } })
  const page = await context.newPage()
  const bad = new URLSearchParams(composite)
  bad.set("fov", "11")
  await page.goto(`${base}/?${bad.toString()}`, { waitUntil: "domcontentloaded" })
  await page.waitForFunction(() => (document.querySelector(".status")?.textContent ?? "").startsWith("failed"), null, {
    timeout: 60000,
  })
  const error = await page.locator(".error").innerText()
  assert.match(error, /field of view/i)
  await shot(page, "render-failure-1280", false)
  console.log("render failure reads:", error)
  await context.close()
}

// A failed catalog is reported in its own row while the image stays ready.
{
  const { context, page, renders } = await open(delta, { width: 1280, height: 900 }, { catalogs: true, failWise: true })
  assert.equal(renders.length, 1, `failed-catalog view made ${renders.length} render requests`)
  await openDrawer(page, "Layers")
  const failure = page.getByText(/WISE markers could not load/i).first()
  await failure.waitFor()
  console.log("catalog failure reads:", await failure.innerText())
  await shot(page, "catalog-failure-1280", false)
  await context.close()
}

await browser.close()
console.log("all marker, status and layout checks passed")
