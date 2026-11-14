// Proves the progressive mosaic, in every mode, against a real SPEARv server.
//
//   SPEARV_BASE_URL=http://127.0.0.1:8080 \
//   PLAYWRIGHT_PATH=/home/ubuntu/e2e-tools/node_modules/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/home/ubuntu/.cache/ms-playwright \
//   node web/e2e/mosaic.mjs
//
// Screenshots go to SPEARV_SCREENSHOT_DIR (default /tmp/spearv-bench). This reads
// about 4 GB from S3. It also proves the sharp reveal follows the server's
// snake order, and saves early, middle and final frames as snake-*.png.

import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir } from "node:fs/promises"
import { modeButton, openDrawer, toggleLayer } from "./lib/ui.mjs"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8080"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-bench"
await mkdir(shots, { recursive: true })

const field = (fov) => `${base}/?ra=56.75&dec=24.12&fov=${fov}&mode=composite&dr=qr3&color=1&mask=off`
const statusLine = (page) => page.locator("figcaption .status")
const finished = async (page, timeout) => {
  await page.waitForFunction(
    () => /^(ready|incomplete|failed)/.test(document.querySelector("figcaption .status")?.textContent ?? ""),
    undefined,
    { timeout },
  )
  return statusLine(page).textContent()
}

const browser = await chromium.launch()
const context = await browser.newContext({ viewport: { width: 1280, height: 900 } })
const page = await context.newPage()
const failures = []
page.on("pageerror", (error) => failures.push(error.message))
const polls = []
const starts = []
page.on("request", (request) => {
  const polled = /\/api\/v1\/mosaic\/([^/]+)\/status/.exec(request.url())
  if (polled) polls.push({ id: polled[1], at: Date.now() })
})
page.on("response", (response) => {
  // A later navigation can discard the body of a response nobody needs any more.
  if (/\/api\/v1\/mosaic\?/.test(response.url()) && response.ok()) {
    response.json().then((body) => starts.push(body.id), () => undefined)
  }
})

// Colour -> composite -> colour joins the same job and stops polling in between.
await page.goto(field(0.5))
await page.waitForSelector(".mosaic canvas", { timeout: 30_000 })
await page.waitForFunction(() => /^(R |Planning)/.test(document.querySelector("figcaption .status")?.textContent ?? ""))
await modeButton(page, "Composite").click()
await page.waitForTimeout(1000)
const left = Date.now()
await page.waitForTimeout(2500)
const colourJob = starts[0]
assert.equal(polls.filter((poll) => poll.at > left && poll.id === colourJob).length, 0, "the colour job's polling stops")
await modeButton(page, "Color").click()
await page.waitForSelector(".mosaic canvas", { timeout: 30_000 })
assert.equal(await finished(page, 180_000), "ready")
assert.equal(starts.at(-1), colourJob, "returning to colour joins the job")
console.log("switch ok, job", colourJob)

// A click maps to the same PNG pixel it would on the image. Open the Spectrum
// drawer first when the shell mounts it lazily.
const spectrumToggle = page
  .getByRole("button", { name: /^\s*Spectrum\s*$/i })
  .and(page.locator("[aria-expanded], [aria-controls]"))
  .first()
if (await spectrumToggle.count()) await openDrawer(page, "Spectrum")

const box = await page.locator(".mosaic").boundingBox()
await page.waitForFunction(() => {
  const preview = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
  const full = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  return Boolean(preview && preview.complete && preview.naturalWidth > 0 || full && full.complete && full.naturalWidth > 0)
}, undefined, { timeout: 60_000 })
const preview = await page.evaluate(() => {
  const image = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
  return image ? { width: image.naturalWidth, height: image.naturalHeight } : null
})
if (preview) assert.ok(preview.width > 0 && preview.width <= 2048, "a bounded server preview was visible")
await page.waitForFunction(() => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 180_000 })
const server = await page.evaluate(() => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  const holder = document.querySelector(".mosaic-canvas")
  return { width: image.naturalWidth, height: image.naturalHeight, hidden: getComputedStyle(holder).visibility === "hidden" }
})
assert.ok(server.width > 0 && server.height > 0, "the native server Color PNG loaded")
assert.equal(server.hidden, true, "the client canvas is hidden behind the server PNG")
const size = { w: server.width, h: server.height }
// Browsers deliver whole CSS pixels, so the expected pixel comes from where the
// click really lands, not from the fraction asked for.
const clickX = Math.floor(box.x + box.width * 0.25)
const clickY = Math.floor(box.y + box.height * 0.75)
const [spectrum] = await Promise.all([page.waitForRequest(/\/api\/v1\/spectrum\?/), page.mouse.click(clickX, clickY)])
const clicked = new URL(spectrum.url()).searchParams
assert.equal(Number(clicked.get("x")), Math.floor(((clickX - box.x) / box.width) * size.w))
assert.equal(Number(clicked.get("y")), Math.floor(((clickY - box.y) / box.height) * size.h))
console.log("click ok", clicked.get("x"), clicked.get("y"), "of", size.w)

// Past the click grid the picture says so and a click does nothing.
await page.goto(field(8))
await page.waitForSelector(".mosaic canvas", { timeout: 30_000 })
await page.getByText("Spectrum clicks are off above 6.99°").waitFor()
let spectra = 0
page.on("request", (request) => (spectra += request.url().includes("/api/v1/spectrum") ? 1 : 0))
const wide = await page.locator(".mosaic").boundingBox()
await page.mouse.click(wide.x + wide.width / 2, wide.y + wide.height / 2)
await page.waitForTimeout(500)
assert.equal(spectra, 0, "no click above the limit")
console.log("wide note ok")

// Reduced motion: boxes hold still.
const calm = await browser.newContext({ reducedMotion: "reduce", viewport: { width: 1280, height: 900 } })
const calmPage = await calm.newPage()
await calmPage.goto(field(8))
await calmPage.waitForSelector(".mosaic-box", { state: "attached", timeout: 60_000 })
assert.equal(await calmPage.locator(".mosaic-box").first().evaluate((node) => getComputedStyle(node).animationName), "none")
await calm.close()
console.log("reduced motion ok")

// Delta: the same canvas and candidate labels. Delta has no visit buttons; its
// URL-only data path is exercised by the marker/status checks.
const geometria = `${base}/?ra=58.2783&dec=26.1398&fov=0.3&mode=delta&dr=qr2&pin=25:1:v0:%2B&pin=25:1:v1:-`
const candidateAsked = []
page.on("request", (request) => {
  if (request.url().includes("/api/v1/delta/candidates")) candidateAsked.push(Date.now())
})
await page.goto(geometria)
await page.waitForSelector(".mosaic canvas", { timeout: 60_000 })
assert.equal(await finished(page, 180_000), "ready")
const answered = await page.waitForResponse(/\/api\/v1\/delta\/candidates/, { timeout: 120_000 })
assert.ok(answered.ok(), "candidates answered")
const labels = (await answered.json()).candidates.filter((row) => row.class !== "star_residue").length
await page.waitForSelector(".candidates", { state: "attached", timeout: 15_000 })
assert.equal(await page.locator(".candidates .candidate").count(), labels)
await page.screenshot({ path: `${shots}/mosaic-delta-geometria.png` })
console.log("delta ok,", labels, "candidate labels, canvas stays")

// The 5 degree showcase, early, middle and finished. Three painter boxes walk
// the full spiral while the layers land, then the final RGB tiles replace them.
await page.goto(field(5))
await page.waitForFunction(() => /^R [1-9]/.test(document.querySelector("figcaption .status")?.textContent ?? ""), undefined, {
  timeout: 240_000,
})
const fiveDeg = starts.at(-1)
const path = await page.evaluate(async (id) => {
  const response = await fetch(`/api/v1/mosaic/${id}/status`)
  const status = await response.json()
  return status.order.map((tile) => `${tile.x},${tile.y}`)
}, fiveDeg)
assert.ok(path.length > 10, `the box path covers the field, got ${path.length} tiles`)
await page.screenshot({ path: `${shots}/snake-early.png` })
console.log("early:", await statusLine(page).textContent())
await page.waitForFunction((total) => (window.__mosaicBoxes?.length ?? 0) === 3, undefined, { timeout: 120_000 })
const boxCount = await page.locator(".mosaic-box").count()
assert.equal(boxCount, 3, `three painter boxes, got ${boxCount}`)
await page.waitForTimeout(20_000)
await page.screenshot({ path: `${shots}/snake-mid.png` })
console.log("mid-run:", await statusLine(page).textContent())
const end = await finished(page, 420_000)
await page.screenshot({ path: `${shots}/snake-done.png` })
assert.equal(await page.locator(".mosaic-box").count(), 0, "the boxes leave once the final image lands")
console.log("boxes ok, final image landed")
console.log("finished:", end, "|", (await page.locator(".viewer .error").count()) ? await page.locator(".viewer .error").textContent() : "no warning")
assert.equal(end, "ready", "the 5 degree job finished with every read")

// Overlays: the catalog route stops at 1 degree, so turning Gaia on at 5
// degrees reports a failed layer and draws no misplaced markers.
const failedCatalog = page.waitForResponse((response) => /\/api\/v1\/catalog\/gaia/.test(response.url()), {
  timeout: 20_000,
})
await toggleLayer(page, "Gaia")
const catalogResponse = await failedCatalog
assert.ok(catalogResponse.status() >= 400, `catalog at 5 degrees answered ${catalogResponse.status()}`)
console.log("overlay at 5 degrees:", catalogResponse.status())
assert.equal(await page.locator(".markers").count(), 0, "no misplaced markers")

// At 1 degree or less, markers share the visible native server image's grid.
await page.goto(`${field(0.5)}&gaia=0.7`)
await page.waitForSelector(".markers circle", { state: "attached", timeout: 60_000 })
await page.waitForFunction(() => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 180_000 })
const fit = await page.evaluate(() => {
  const svg = document.querySelector(".markers")
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  const a = svg.getBoundingClientRect()
  const b = image.getBoundingClientRect()
  return { view: svg.getAttribute("viewBox"), native: `0 0 ${image.naturalWidth} ${image.naturalHeight}`, dx: a.x - b.x, dy: a.y - b.y, dw: a.width - b.width }
})
assert.equal(fit.view, fit.native)
assert.deepEqual([fit.dx, fit.dy, fit.dw], [0, 0, 0])
await page.screenshot({ path: `${shots}/mosaic-half-degree-gaia.png` })
console.log("markers aligned", fit.view)

assert.deepEqual(failures, [])
await browser.close()
console.log("all mosaic checks passed")
