// Finished-job refresh regression against a real engine. No engine restart.
// SPEARV_BASE_URL=http://127.0.0.1:8194 PLAYWRIGHT_PATH=... node web/e2e/mosaic-refresh.mjs
// Optional SPEARV_BASELINE_URL records a second deployment's Color view with
// scale16 tiles held, for side-by-side comparison.
import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir, writeFile } from "node:fs/promises"
import { closeDrawer, openDrawer } from "./lib/ui.mjs"

const { chromium } = createRequire(import.meta.url)(process.env.PLAYWRIGHT_PATH || "playwright")
const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8194"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/opencode/spearv-mosaic-progressive"
const field = (origin, color) => `${origin}/?ra=56.75&dec=24.12&fov_l=0.9&fov_h=0.9&mode=composite&dr=qr3&mask=on${color ? "&color=1" : ""}`
await mkdir(shots, { recursive: true })
const browser = await chromium.launch()
const report = { base, tests: [], errors: [] }
const pixelSummary = () => {
  const canvas = document.querySelector(".mosaic canvas")
  if (!canvas) return null
  const data = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data
  let lit = 0, sum = 0
  for (let index = 0; index < data.length; index += 4) {
    if (data[index] || data[index + 1] || data[index + 2]) lit++
    sum += data[index] + data[index + 1] + data[index + 2]
  }
  return { lit, sum, pixels: data.length / 4, revision: Number(canvas.dataset.completedRevision || 0), busy: canvas.dataset.remapBusy === "true" }
}
const canvasReady = (page) => page.waitForFunction(() => {
  const canvas = document.querySelector(".mosaic canvas")
  return Number(canvas?.dataset.completedRevision) > 0 && canvas?.dataset.remapBusy === "false"
}, undefined, { timeout: 180_000 })
const colorFull = (page) => page.waitForFunction(() => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 180_000 })
const colorPreview = (page) => page.waitForFunction(() => {
  const image = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 180_000 })
const ready = (page, color = false) => color ? colorFull(page) : canvasReady(page)
const serverPng = (page) => page.evaluate(async () => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  if (!image) return null
  const response = await fetch(image.src)
  const buffer = new Uint8Array(await response.arrayBuffer())
  const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength)
  return {
    src: image.src,
    naturalWidth: image.naturalWidth,
    naturalHeight: image.naturalHeight,
    width: view.getUint32(16),
    height: view.getUint32(20),
    bytes: buffer.byteLength,
    signature: [...buffer.slice(0, 8)],
  }
})
const final = (page) => page.waitForFunction(() => /^(ready|incomplete)/.test(document.querySelector("figcaption .status")?.textContent || ""), undefined, { timeout: 180_000 })
const deferred = () => {
  let resolve
  const promise = new Promise((done) => { resolve = done })
  return { promise, resolve }
}

async function verify(color) {
  const label = color ? "color" : "composite"
  const context = await browser.newContext({ viewport: { width: 1440, height: 900 } })
  const page = await context.newPage()
  page.on("pageerror", (error) => report.errors.push(`${label}: ${error.message}`))
  await page.addInitScript(() => {
    window.__refreshDraws = []
    window.__refreshFrames = []
    const draw = CanvasRenderingContext2D.prototype.drawImage
    CanvasRenderingContext2D.prototype.drawImage = function (...args) {
      const result = draw.apply(this, args)
      if (this.canvas.matches(".mosaic canvas")) window.__refreshDraws.push({ x: args[1], y: args[2], at: performance.now() })
      return result
    }
    const tick = () => {
      const canvas = document.querySelector(".mosaic canvas")
      if (canvas && window.__refreshFrames.length < 12_000) {
        const data = canvas.getContext("2d").getImageData(0, 0, canvas.width, canvas.height).data
        let lit = 0
        for (let index = 0; index < data.length; index += 64) if (data[index] || data[index + 1] || data[index + 2]) lit++
        window.__refreshFrames.push({ at: performance.now(), lit, revision: Number(canvas.dataset.completedRevision || 0) })
      }
      requestAnimationFrame(tick)
    }
    requestAnimationFrame(tick)
  })
  await page.goto(field(base, color), { waitUntil: "domcontentloaded" })
  await ready(page, color)
  const id = await page.evaluate(() => window.__mosaicId)
  const status = await (await page.request.get(`${base}/api/v1/mosaic/${id}/status?since=0`)).json()
  assert.equal(status.state, "done")
  assert.equal(status.reads_failed, 0)
  const last = status.order.at(-1)

  if (color && process.env.SPEARV_BASELINE_URL) {
    const baseline = await context.newPage()
    const held = deferred()
    let rawHeld = false
    await baseline.route(/\/api\/v1\/mosaic\/[^/]+\/tile\?/, async (route) => {
      if (new URL(route.request().url()).searchParams.get("format") === "scale16") { rawHeld = true; await held.promise }
      await route.continue()
    })
    await baseline.goto(field(process.env.SPEARV_BASELINE_URL, true), { waitUntil: "domcontentloaded" })
    await final(baseline)
    await baseline.waitForFunction(() => !!document.querySelector(".raw-load")?.textContent.includes("Loading three color layers"))
    assert.ok(rawHeld)
    const before = await baseline.evaluate(pixelSummary)
    assert.equal(before.lit, 0, "baseline done-join is black until scale16 arrives")
    await baseline.screenshot({ path: `${shots}/before-color-raw-held.png` })
    report.tests.push({ label: "baseline-color-done-join", id: await baseline.evaluate(() => window.__mosaicId), before })
    held.resolve()
    await baseline.close()
  }

  for (const variant of ["spiral", "grid-fallback", "planning-done", "reduced-motion"]) {
    await page.emulateMedia({ reducedMotion: variant === "reduced-motion" ? "reduce" : "no-preference" })
    const png = deferred(), raw = deferred(), rawSeen = deferred(), full = deferred()
    let rawHeld = 0, fullHeld = 0, firstStatus, statusCount = 0, requests = []
    await page.route(/\/api\/v1\/mosaic\/[^/]+\/status\?/, async (route) => {
      const response = await route.fetch()
      const body = await response.json()
      firstStatus ??= body.state
      statusCount++
      if (variant === "grid-fallback" || variant === "planning-done") body.order = []
      if (variant === "planning-done" && statusCount === 1) { body.state = "planning"; body.order = [] }
      await route.fulfill({ response, json: body })
    })
    await page.route(/\/api\/v1\/mosaic\/[^/]+\/tile\?/, async (route) => {
      const params = new URL(route.request().url()).searchParams
      requests.push(Object.fromEntries(params))
      if (params.get("format") && !color) {
        rawHeld++
        rawSeen.resolve()
        await raw.promise
      } else if (!color && !params.get("format") && variant === "spiral" && params.get("x") === String(last.x) && params.get("y") === String(last.y)) await png.promise
      await route.continue()
    })
    if (color) {
      await page.route(/\/api\/v1\/mosaic\/[^/]+\/display-revisions\/[^/]+\/full\.png$/, async (route) => {
        fullHeld++
        await full.promise
        await route.continue()
      })
    }
    const cdp = await context.newCDPSession(page)
    await cdp.send("Network.clearBrowserCache")
    await page.reload({ waitUntil: "domcontentloaded" })
    await page.waitForSelector(".mosaic canvas")
    if (variant === "spiral" && !color) {
      await page.waitForFunction(() => window.__refreshDraws.length >= 6, undefined, { timeout: 60_000 })
      const partial = await page.evaluate(pixelSummary)
      assert.ok(partial.lit > 0 && partial.lit < partial.pixels, "partial final tiles are visible before the held last tile")
      await page.screenshot({ path: `${shots}/${label}-refresh-fill.png` })
      png.resolve()
    }
    await final(page)
    assert.equal(await page.evaluate(() => window.__mosaicId), id, "refresh rejoins the same in-memory job")
    assert.equal(firstStatus, "done", "refresh does not start a cold science rebuild")
    const waiting = await page.evaluate(pixelSummary)
    if (color) {
      await colorPreview(page)
      assert.equal(await page.evaluate(() => getComputedStyle(document.querySelector(".mosaic-canvas")).visibility), "hidden", "the fallback canvas is hidden behind the visible server preview")
      const preview = await page.evaluate(() => {
        const image = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
        return { width: image.naturalWidth, height: image.naturalHeight }
      })
      assert.ok(preview.width > 0 && preview.width <= 2048, "a bounded server preview is visible")
      await openDrawer(page, "Share")
      const download = page.getByRole("button", { name: "Download PNG", exact: true })
      assert.equal(await download.isDisabled(), true, "Color download waits for the native full PNG")
      await page.getByText(/Preparing the original full-resolution Color PNG/).waitFor({ timeout: 30_000 })
      await page.screenshot({ path: `${shots}/${label}-${variant}-preview.png` })
      full.resolve()
      await ready(page, true)
      assert.ok(fullHeld > 0, "the native full PNG was requested")
      assert.equal(await download.isDisabled(), false, "the native full PNG enables download")
      await page.getByText(/The exact current full-resolution server PNG is ready to download/).waitFor({ timeout: 30_000 })
      const native = await serverPng(page)
      assert.ok(native, "the native full Color PNG is visible")
      assert.equal(native.width, native.naturalWidth, "the full PNG is native, not the bounded preview")
      assert.equal(native.height, native.naturalHeight)
      assert.ok(native.bytes > 0)
      assert.deepEqual(native.signature, [137, 80, 78, 71, 13, 10, 26, 10])
      report.tests.push({ label: `${label}-${variant}-native`, width: native.width, height: native.height, bytes: native.bytes })
    } else {
      assert.ok(waiting.lit > 0, "final PNGs remain visible during raw loading")
      await rawSeen.promise
      assert.ok(rawHeld > 0)
      assert.equal(waiting.revision, 0, "local mix has not painted yet")
      await openDrawer(page, "Share")
      assert.ok(await page.getByRole("button", { name: "Download PNG", exact: true }).isDisabled(), "Composite export remains gated while raw layers load")
      await page.screenshot({ path: `${shots}/${label}-${variant}-raw-held.png` })
      raw.resolve()
      await ready(page)
      assert.equal(await page.getByRole("button", { name: "Download PNG", exact: true }).isDisabled(), false)
    }
    const draws = await page.evaluate(() => window.__refreshDraws)
    const frames = await page.evaluate(() => window.__refreshFrames)
    if (!color) {
      const expected = variant === "grid-fallback" || variant === "planning-done"
        ? Array.from({ length: status.order.length }, (_, index) => ({ x: index % 3, y: Math.floor(index / 3) }))
        : status.order
      assert.deepEqual(draws.map(({ x, y }) => ({ x: x / 256, y: y / 256 })), expected, "finished tiles paint in the reported path (or grid fallback) order")
      if (variant !== "reduced-motion") assert.ok(draws.at(-1).at > draws[0].at + 30, "fast fill spans visible animation frames")
      const firstLit = frames.findIndex((frame) => frame.lit > 0)
      assert.ok(firstLit >= 0)
      assert.equal(frames.slice(firstLit).some((frame) => frame.lit === 0), false, "no all-black frame after the first painted pixels, including raw handoff")
    }
    await page.screenshot({ path: `${shots}/${label}-${variant}-ready.png` })
    report.tests.push({ label: `${label}-${variant}`, id, firstStatus, statusCount, waiting, ready: await page.evaluate(pixelSummary), draws, frames, tileRequests: requests })
    await cdp.detach()
    await page.unrouteAll({ behavior: "wait" })
  }
  if (color) {
    const posts = []
    const starts = []
    page.on("request", (request) => {
      const url = new URL(request.url())
      if (request.method() === "POST" && url.pathname.endsWith("/display-revisions")) posts.push(url)
      if (request.method() === "GET" && url.pathname === "/api/v1/mosaic") starts.push(url)
    })
    await closeDrawer(page, "Share")
    await openDrawer(page, "Color controls")
    const beforeSrc = await page.evaluate(() => document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')?.src)
    assert.ok(beforeSrc, "the current server PNG is visible before the edit")
    const beforePosts = posts.length, beforeStarts = starts.length
    await page.getByRole("spinbutton", { name: "Red gain value", exact: true }).fill("0.25")
    await page.waitForFunction((previous) => {
      const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
      return Boolean(image && image.complete && image.naturalWidth > 0 && image.src !== previous)
    }, beforeSrc, { timeout: 180_000 })
    assert.ok(posts.length > beforePosts, "a Color control edit creates a server display revision")
    assert.equal(starts.length, beforeStarts, "a Color control edit does not restart the science job")
    const changed = await serverPng(page)
    assert.ok(changed && changed.bytes > 0)
    assert.equal(changed.width, changed.naturalWidth)
    assert.equal(changed.height, changed.naturalHeight)
    await closeDrawer(page, "Color controls")
    await openDrawer(page, "Share")
    await page.getByText(/The exact current full-resolution server PNG is ready to download/).waitFor({ timeout: 60_000 })
    assert.equal(await page.getByRole("button", { name: "Download PNG", exact: true }).isDisabled(), false, "the new server PNG is ready to download")
    report.tests.push({ label: "color-server-tone", posts: posts.length, starts: starts.length, width: changed.width, height: changed.height, bytes: changed.bytes })
  }
  await context.close()
}

try {
  await verify(true)
  await verify(false)
  assert.deepEqual(report.errors, [])
  console.log("Finished Color server-PNG and Composite raw refresh, grid fallback, planning-to-done, reduced motion, export and server tone checks passed.")
} finally {
  await writeFile(`${shots}/refresh-evidence.json`, JSON.stringify(report, null, 2))
  await browser.close()
}
