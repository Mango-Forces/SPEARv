// Full-App Color browser regression over a tiny mocked server.
//
// Exercises the production App, useMosaic, useRawLayers, useColorDisplay,
// MosaicPicture, SharePanel, and display controls. All science and display
// routes are mocked in the browser; no engine or real science job is touched.
//
// Run from anywhere:
//   PLAYWRIGHT_PATH=/path/to/playwright/index.js node web/e2e/color-parent-state.mjs
//
// "playwright" must be resolvable, or PLAYWRIGHT_PATH must point at its
// index.js. The browser cache follows Playwright's normal
// PLAYWRIGHT_BROWSERS_PATH setting. Vite is started as this process's child on
// 127.0.0.1:8243 and stopped in the finally block.

import assert from "node:assert/strict"
import fs from "node:fs"
import os from "node:os"
import path from "node:path"
import { spawn } from "node:child_process"
import { fileURLToPath } from "node:url"
import { createRequire } from "node:module"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const webRoot = fileURLToPath(new URL("..", import.meta.url))
const viteBin = fileURLToPath(new URL("../node_modules/vite/bin/vite.js", import.meta.url))
const host = "127.0.0.1"
const port = 8243
const base = `http://${host}:${port}`
const PNG = Buffer.from("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGN4oKAAAAMEASFndL7nAAAAAElFTkSuQmCC", "base64")
const tempRoot = fs.mkdtempSync(path.join(os.tmpdir(), "spearv-color-browser-"))

let viteLog = ""
const vite = spawn(process.execPath, [viteBin, "--host", host, "--port", String(port), "--strictPort"], {
  cwd: webRoot,
  env: { ...process.env, BROWSER: "none" },
  stdio: ["ignore", "pipe", "pipe"],
})
vite.stdout.setEncoding("utf8")
vite.stderr.setEncoding("utf8")
vite.stdout.on("data", (chunk) => (viteLog += chunk))
vite.stderr.on("data", (chunk) => (viteLog += chunk))
const viteExited = new Promise((resolve) => vite.once("exit", resolve))

async function waitForVite() {
  const deadline = Date.now() + 30_000
  while (Date.now() < deadline) {
    if (vite.exitCode !== null) throw new Error(`Vite exited with ${vite.exitCode}\n${viteLog}`)
    try {
      const response = await fetch(base)
      if (response.ok) return
    } catch {
      // Vite is still starting.
    }
    await new Promise((resolve) => setTimeout(resolve, 100))
  }
  throw new Error(`Vite did not serve ${base}\n${viteLog}`)
}

async function stopVite() {
  if (vite.exitCode !== null) return
  vite.kill("SIGTERM")
  await Promise.race([viteExited, new Promise((resolve) => setTimeout(resolve, 5_000))])
  if (vite.exitCode === null) vite.kill("SIGKILL")
}

async function main() {
  await waitForVite()
  const browser = await chromium.launch({ headless: true })
  const context = await browser.newContext({ acceptDownloads: true, viewport: { width: 1440, height: 1000 } })
  const page = await context.newPage()
  const errors = []
  page.on("pageerror", (error) => errors.push(error.message))
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") errors.push(message.text())
  })

  const state = {
    currentFalse: false,
    offline: false,
    holdScience: false,
    releaseScience: null,
    revisions: new Map(),
    latest: null,
    nextRevision: 0,
    requests: [],
    displayStatusRequests: 0,
    displayPosts: 0,
  }

  const json = (route, body, status = 200) => route.fulfill({
    status,
    contentType: "application/json",
    body: JSON.stringify(body),
  })
  const channels = ["blue", "green", "red"].map((role, index) => ({
    index, role, center_band: 12 + index, width: 1, bands: [12 + index],
    wavelength_min_um: 1, wavelength_max_um: 2, finite_count: 1,
    default_black: 0, default_white: 1, default_span: 1,
    scale_offset: 0, scale_span: 1,
  }))
  const manifest = (revision) => {
    const entry = state.revisions.get(revision)
    if (!entry) return null
    const latest = revision === state.latest
    const fullReady = entry.fullRequested
    const artifact = (kind, ready) => ({
      state: ready ? "ready" : "not_requested",
      width: 1, height: 1,
      url: `/api/v1/mosaic/m1/display-revisions/${revision}/${kind}.png`,
      bytes: ready ? PNG.length : null,
      error: null,
      completed_rows: ready ? 1 : 0,
      total_rows: 1,
    })
    return {
      job_id: "m1", revision, current: latest && !state.currentFalse, width: 1, height: 1,
      display: entry.display,
      channels,
      preview: artifact("preview", true),
      full: artifact("full", fullReady),
    }
  }
  const statusBody = () => ({
    state: "done", active: [], order: [{ x: 0, y: 0 }],
    channels_total: [1, 1, 1], channels_done: [1, 1, 1], frontiers: [1, 1, 1],
    tiles: [], seq: 1, tiles_done: 1, tiles_total: 1, reads_failed: 0,
    bytes_planned: 1, bytes_read: 1, elapsed_ms: 1,
    exposures_done: 1, exposures_total: 1,
  })

  await context.route("**/api/**", async (route) => {
    const request = route.request()
    const url = new URL(request.url())
    const pathname = url.pathname
    state.requests.push({ method: request.method(), path: pathname })
    if (pathname === "/api/v1/releases") return json(route, { releases: [{ id: "qr2" }] })
    if (pathname === "/api/v1/coverage") return json(route, { visits: [], exposures: 0, bands: [] })
    if (pathname === "/api/v1/mosaic" && request.method() === "GET") {
      if (state.holdScience) await new Promise((resolve) => { state.releaseScience = resolve })
      return json(route, { id: "m1", width: 1, height: 1, cols: 1, rows: 1, tile: 256, exposures: 1 })
    }
    if (pathname === "/api/v1/mosaic/m1/status") return json(route, statusBody())
    if (pathname === "/api/v1/mosaic/m1/display-revisions" && request.method() === "POST") {
      state.displayPosts++
      const revision = `d${state.nextRevision++}`
      const display = JSON.parse(request.postData())
      state.revisions.set(revision, { display, fullRequested: false })
      state.latest = revision
      state.currentFalse = false
      return json(route, manifest(revision), 201)
    }
    const display = pathname.match(/^\/api\/v1\/mosaic\/m1\/display-revisions\/(d\d+)\/(status|full|preview\.png|full\.png)$/)
    if (display) {
      const [, revision, kind] = display
      const entry = state.revisions.get(revision)
      if (!entry) return json(route, { error: "no revision" }, 404)
      if (kind === "status") {
        state.displayStatusRequests++
        if (state.offline) return json(route, { error: "offline" }, 503)
        return json(route, manifest(revision))
      }
      if (kind === "full") {
        entry.fullRequested = true
        return json(route, manifest(revision), 202)
      }
      return route.fulfill({ status: 200, contentType: "image/png", headers: { "content-length": String(PNG.length) }, body: PNG })
    }
    return json(route, { error: `unexpected ${request.method()} ${pathname}` }, 404)
  })

  const waitUntil = async (predicate, timeout = 10000, label = "condition") => {
    const expires = Date.now() + timeout
    let last
    while (Date.now() < expires) {
      try {
        last = await predicate()
        if (last) return
      } catch (error) {
        last = error
      }
      await page.waitForTimeout(25)
    }
    throw new Error(`timed out: ${label} (last=${String(last)})`)
  }

  await page.goto(`${base}/?ra=56.75&dec=24.12&fov_l=1&fov_h=1&mode=color&mask=off`, { waitUntil: "domcontentloaded" })
  const shareTab = page.getByRole("button", { name: "Expand Share" })
  await shareTab.waitFor({ timeout: 10000 })
  await shareTab.click()
  const share = page.getByRole("dialog", { name: "Share" })
  await share.waitFor({ timeout: 5000 })
  const download = share.getByRole("button", { name: "Download PNG" })
  await waitUntil(() => download.isEnabled(), 15000, "Color download enabled")
  await share.getByText(/The exact current full-resolution server PNG is ready to download/).waitFor({ timeout: 5000 })
  await page.locator('img[alt="Server-rendered full-resolution Color PNG."]').waitFor({ timeout: 5000 })
  assert.equal(await page.locator('img[alt="Server-rendered full-resolution Color PNG."]').evaluate((img) => img.naturalWidth), 1)
  assert.equal(state.displayPosts, 1)

  const firstDownload = path.join(tempRoot, "download-ready.png")
  const [readyDownload] = await Promise.all([page.waitForEvent("download"), download.click()])
  await readyDownload.saveAs(firstDownload)
  assert.deepEqual(fs.readFileSync(firstDownload), PNG)
  await share.getByText("Saved the exact full-resolution server PNG.").waitFor({ timeout: 5000 })

  const pollsBeforeSettle = state.displayStatusRequests
  await waitUntil(() => state.displayStatusRequests >= pollsBeforeSettle + 2, 5000, "two settled display polls")
  assert.equal(state.displayPosts, 1)
  assert.ok(!errors.some((text) => /Maximum update depth|Too many re-renders/.test(text)), errors.join("\n"))

  state.offline = true
  await share.getByText(/offline/).waitFor({ timeout: 5000 })
  assert.equal(await download.isEnabled(), true)
  const requestsBeforeCachedDownload = state.requests.length
  const cachedDownload = path.join(tempRoot, "download-cached.png")
  const [cached] = await Promise.all([page.waitForEvent("download"), download.click()])
  await cached.saveAs(cachedDownload)
  assert.deepEqual(fs.readFileSync(cachedDownload), PNG)
  assert.equal(state.requests.length, requestsBeforeCachedDownload)

  state.offline = false
  await waitUntil(async () => !(await share.innerText()).includes("offline"), 5000, "server back online")

  const mosaicStartsBeforeEdit = state.requests.filter((request) => request.path === "/api/v1/mosaic").length
  const gain = page.getByLabel("Red gain value")
  await gain.fill("1.25")
  await waitUntil(async () => !(await download.isEnabled()), 2000, "edit closes export gate")
  await waitUntil(() => download.isEnabled(), 15000, "new revision export enabled")
  assert.equal(state.displayPosts, 2)
  assert.equal(state.requests.filter((request) => request.path === "/api/v1/mosaic").length, mosaicStartsBeforeEdit)

  state.currentFalse = true
  await share.getByText(/no longer lists this revision as current/).waitFor({ timeout: 5000 })
  assert.equal(await download.isEnabled(), true)

  state.holdScience = true
  await page.locator('input[name="ra"]').fill("57.75")
  await page.getByRole("button", { name: "View", exact: true }).click()
  await waitUntil(async () => !(await download.isEnabled()), 2000, "view change closes export gate")
  const viewGateText = await share.innerText()
  assert.match(viewGateText, /Waiting for the current Color display|Color settings are queued|Preparing the original full-resolution Color PNG/)
  if (state.releaseScience) state.releaseScience()

  assert.ok(!state.requests.some((request) => /scale16|raw-metadata/.test(request.path)), "Color must not request scale16/raw metadata")
  assert.ok(!errors.some((text) => /Maximum update depth|Too many re-renders/.test(text)), errors.join("\n"))

  await page.getByRole("button", { name: "Composite", exact: true }).click()
  await waitUntil(async () => await page.locator('input[name="share-export-format"]').count() === 3, 5000, "Composite keeps three export formats")
  await page.getByRole("button", { name: "Specific", exact: true }).click()
  await waitUntil(async () => await page.locator('input[name="share-export-format"]').count() === 3, 5000, "Specific keeps three export formats")

  const unexpectedErrors = errors.filter((text) => !/503 \(Service Unavailable\)/.test(text))
  assert.deepEqual(unexpectedErrors, [])
  console.log(JSON.stringify({
    ok: true,
    displayPosts: state.displayPosts,
    displayStatusRequests: state.displayStatusRequests,
    mosaicStarts: state.requests.filter((request) => request.path === "/api/v1/mosaic").length,
    requests: state.requests.map((request) => `${request.method} ${request.path}`),
    expectedOfflineError: errors.find((text) => /503 \(Service Unavailable\)/.test(text)) || null,
    unexpectedErrors,
  }, null, 2))

  await context.close()
  await browser.close()
}

let failed = false
try {
  await main()
} catch (error) {
  failed = true
  console.error(error.stack || error)
} finally {
  await stopVite()
  fs.rmSync(tempRoot, { recursive: true, force: true })
}
process.exitCode = failed ? 1 : 0
