// One concise UI contract for the rebuilt shell. Drives the real React
// components against deterministic API routes: no engine, S3, index or long
// science run.
//
//   SPEARV_BASE_URL=http://127.0.0.1:8091 \
//   PLAYWRIGHT_PATH=/home/ubuntu/e2e-tools/node_modules/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/home/ubuntu/.cache/ms-playwright \
//   node web/e2e/ui-contract.mjs
//
// Covers: one-line header and icon-only Share; closed drawers with
// close/reopen/Escape; no public movers; Share PNG/JPEG/WebP with real
// painted pixels; canonical share links; colour round-trip; Specific editor,
// bounds share roundtrip and sector persistence; Spectrum collapsed table
// with panel-bounded scrolling.

import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir, readFile } from "node:fs/promises"
import { join } from "node:path"
import zlib from "node:zlib"
import {
  assertNoPageOverflow,
  closeAllDrawers,
  closeDrawer,
  drawerIsOpen,
  findPanelButton,
  layerRow,
  mainCanvas,
  modeButton,
  openDrawer,
  toggleLayer,
  waitForCanvas,
  waitForDrawer,
} from "./lib/ui.mjs"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8080"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-verify/ui-contract"
await mkdir(shots, { recursive: true })

// ---------------------------------------------------------------------------
// Deterministic 64x64 solid tiles. The mosaic canvas is 128x128 with 64px
// tiles, and useMosaic paints each finished tile at its natural size with
// drawImage(bitmap, x*tile, y*tile). A 2x2 stub would leave the canvas
// almost entirely black, so the export would be a black frame. These tiles
// decode through the real createImageBitmap path and paint opaque colour.
// ---------------------------------------------------------------------------

const crcTable = (() => {
  const table = new Int32Array(256)
  for (let n = 0; n < 256; n++) {
    let c = n
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    table[n] = c
  }
  return table
})()

function crc32(buffer) {
  let crc = -1
  for (let i = 0; i < buffer.length; i++) crc = crcTable[(crc ^ buffer[i]) & 0xff] ^ (crc >>> 8)
  return (crc ^ -1) >>> 0
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4)
  length.writeUInt32BE(data.length, 0)
  const body = Buffer.concat([Buffer.from(type, "ascii"), data])
  const crc = Buffer.alloc(4)
  crc.writeUInt32BE(crc32(body), 0)
  return Buffer.concat([length, body, crc])
}

function encodeSolidPng(width, height, r, g, b) {
  const header = Buffer.alloc(13)
  header.writeUInt32BE(width, 0)
  header.writeUInt32BE(height, 4)
  header[8] = 8
  header[9] = 2
  header[10] = 0
  header[11] = 0
  header[12] = 0
  const stride = width * 3 + 1
  const raw = Buffer.alloc(stride * height)
  for (let y = 0; y < height; y++) {
    raw[y * stride] = 0
    for (let x = 0; x < width; x++) {
      raw[y * stride + 1 + x * 3] = r
      raw[y * stride + 1 + x * 3 + 1] = g
      raw[y * stride + 1 + x * 3 + 2] = b
    }
  }
  const compressed = zlib.deflateSync(raw)
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk("IHDR", header),
    pngChunk("IDAT", compressed),
    pngChunk("IEND", Buffer.alloc(0)),
  ])
}

// Solid warm red 64x64: clearly non-black, decodes on every browser.
const tilePng = encodeSolidPng(64, 64, 210, 40, 40)

// Color shows a bounded server preview and downloads the exact native server
// PNG. There is no scale16 raw layer and no local final canvas in this path.
const serverPreview = encodeSolidPng(32, 32, 210, 40, 40)
const serverFull = encodeSolidPng(128, 128, 210, 40, 40)
const displayChannels = ["blue", "green", "red"].map((role, index) => ({
  index, role, center_band: [6, 41, 91][index], width: 5, bands: [[4, 5, 6, 7, 8], [39, 40, 41, 42, 43], [89, 90, 91, 92, 93]][index],
  wavelength_min_um: 0.8 + index * 0.5, wavelength_max_um: 0.9 + index * 0.5, finite_count: 128 * 128,
  default_black: 0, default_white: 1, default_span: 1,
}))

function parsePng(buffer) {
  assert.equal(buffer[0], 0x89)
  assert.equal(buffer[1], 0x50)
  let offset = 8
  let width = 0
  let height = 0
  let bitDepth = 0
  let colorType = 0
  const idats = []
  while (offset + 8 <= buffer.length) {
    const length = buffer.readUInt32BE(offset)
    const type = buffer.subarray(offset + 4, offset + 8).toString("ascii")
    const data = buffer.subarray(offset + 8, offset + 8 + length)
    if (type === "IHDR") {
      width = data.readUInt32BE(0)
      height = data.readUInt32BE(4)
      bitDepth = data[8]
      colorType = data[9]
    } else if (type === "IDAT") {
      idats.push(data)
    } else if (type === "IEND") {
      break
    }
    offset += 12 + length
  }
  return { width, height, bitDepth, colorType, raw: zlib.inflateSync(Buffer.concat(idats)) }
}

function pngHasNonBlack(parsed) {
  const { width, height, bitDepth, colorType, raw } = parsed
  assert.equal(bitDepth, 8, "exported PNG is not 8-bit")
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : null
  assert.ok(channels, `unexpected exported PNG color type ${colorType}`)
  const stride = width * channels
  let previous = Buffer.alloc(stride, 0)
  let found = false
  let offset = 0
  const paeth = (a, b, c) => {
    const p = a + b - c
    const pa = Math.abs(p - a)
    const pb = Math.abs(p - b)
    const pc = Math.abs(p - c)
    if (pa <= pb && pa <= pc) return a
    if (pb <= pc) return b
    return c
  }
  for (let y = 0; y < height; y++) {
    const filter = raw[offset]
    offset += 1
    const current = Buffer.alloc(stride)
    for (let i = 0; i < stride; i++) {
      const left = i >= channels ? current[i - channels] : 0
      const up = previous[i]
      const upperLeft = i >= channels ? previous[i - channels] : 0
      const value = raw[offset + i]
      let recon = value
      if (filter === 1) recon = (value + left) & 0xff
      else if (filter === 2) recon = (value + up) & 0xff
      else if (filter === 3) recon = (value + ((left + up) >> 1)) & 0xff
      else if (filter === 4) recon = (value + paeth(left, up, upperLeft)) & 0xff
      current[i] = recon
    }
    offset += stride
    for (let x = 0; x < width; x++) {
      const r = current[x * channels]
      const g = current[x * channels + 1]
      const b = current[x * channels + 2]
      if (r + g + b > 0) {
        found = true
        break
      }
    }
    if (found) break
    previous = current
  }
  return found
}

const mosaicInfo = { id: "contract", width: 128, height: 128, tile: 64, cols: 2, rows: 2, exposures: 4 }
// Shape mirrors components["schemas"]["MosaicStatus"]: order/tiles use
// {x,y}, frontiers/channels count per channel, settled tiles carry version
// and final. A done job with a full spiral order lands the final RGB tiles.
const mosaicStatus = {
  state: "done",
  seq: 7,
  error: null,
  active: [],
  order: [
    { x: 0, y: 0 },
    { x: 1, y: 0 },
    { x: 1, y: 1 },
    { x: 0, y: 1 },
  ],
  channels_total: [4, 4, 4],
  channels_done: [4, 4, 4],
  frontiers: [4, 4, 4],
  tiles: [
    { x: 0, y: 0, version: 1, final: true },
    { x: 1, y: 0, version: 1, final: true },
    { x: 1, y: 1, version: 1, final: true },
    { x: 0, y: 1, version: 1, final: true },
  ],
  tiles_total: 4,
  tiles_done: 4,
  bytes_planned: 1024,
  bytes_read: 1024,
  elapsed_ms: 1200,
  exposures_total: 4,
  exposures_done: 4,
  reads_failed: 0,
}

// A plain Composite job has one channel; its final PNG tiles paint the canvas.
const compositeStatus = {
  ...mosaicStatus,
  channels_total: [4],
  channels_done: [4],
  frontiers: [4],
}

const coverage = {
  release: "qr3",
  exposures: 12,
  bands: [
    { index: 0, detector: 1, subchan: 1, center_um: 0.75, wl_min_um: 0.74, wl_max_um: 0.76, exposures: 4 },
  ],
  visits: [{ index: 0, mjd: 60676, mjd_end: 60676.1, exposures: 4 }],
}

// Keys mirror components["schemas"]["SnippetRow"]: id/snippet_ref/band/mjd/
// utc/release/detector/obsid/version/wl_*_um plus a quality object.
const snippetRows = [1, 2, 3].map((index) => ({
  id: `contract-snippet-${index}`,
  snippet_ref: `ref-${index}`,
  band: index,
  mjd: 60676 + index / 24,
  utc: `2025-01-0${index}T00:00:00.000Z`,
  release: "qr3",
  detector: 1,
  obsid: `obs-${index}`,
  version: "1",
  wl_min_um: 0.75,
  wl_max_um: 0.8,
  quality: {},
}))

// Shape mirrors components["schemas"]["SpectrumBody"] with Point entries.
const validSpectrum = {
  ra: 56.75,
  dec: 24.12,
  x: 64,
  y: 64,
  release: "qr3",
  meaning: "fixture spectrum",
  pins: [],
  points: [
    { band: 0, wavelength_um: 0.75, flux: 1.2, error: 0.1, flags: [], invalid: false, samples: 3 },
    { band: 1, wavelength_um: 0.8, flux: 1.05, error: 0.15, flags: [], invalid: false, samples: 2 },
    { band: 2, wavelength_um: 0.85, flux: 0.9, error: 0.2, flags: ["ramp"], invalid: true, samples: 1 },
  ],
}
const invalidSpectrum = {
  ...validSpectrum,
  points: validSpectrum.points.map((point) => ({ ...point, flags: ["ramp"], invalid: true })),
}

const browser = await chromium.launch()
const context = await browser.newContext({
  viewport: { width: 1440, height: 960 },
  permissions: ["clipboard-read", "clipboard-write"],
})
const page = await context.newPage()

try {
const pageErrors = []
const requests = { mosaic: [], movers: [], spectrum: [], rawMetadata: [], rawTiles: [], displayPng: [] }
page.on("pageerror", (error) => pageErrors.push(error.message))

const json = (route, body, status = 200) =>
  route.fulfill({ status, contentType: "application/json", body: JSON.stringify(body) })
const png = (route) => route.fulfill({ status: 200, contentType: "image/png", body: tilePng })

const displayRevisions = []
const fullRequested = new Set()
let holdFullGaia = false
let releaseHeldFull
const heldFull = new Promise((resolve) => { releaseHeldFull = resolve })
const displayArtifact = (revision, kind, ready) => {
  const width = kind === "preview" ? 32 : 128
  const height = kind === "preview" ? 32 : 128
  const bytes = kind === "preview" ? serverPreview : serverFull
  return {
    state: ready ? "ready" : "not_requested",
    width, height,
    url: `/api/v1/mosaic/contract/display-revisions/${revision}/${kind}.png`,
    bytes: ready ? bytes.length : null,
    error: null,
    completed_rows: ready ? height : 0,
    total_rows: height,
  }
}
const displayManifest = (revision) => ({
  job_id: "contract",
  revision,
  current: true,
  width: 128,
  height: 128,
  display: displayRevisions[Number(revision.slice(1))],
  channels: displayChannels,
  preview: displayArtifact(revision, "preview", true),
  full: displayArtifact(revision, "full", fullRequested.has(revision)),
})

let spectrumBody = validSpectrum
let viewColor = false
await page.route("**/api/**", async (route) => {
  const request = route.request()
  const url = new URL(request.url())
  const path = url.pathname
  if (path === "/api/v1/releases") {
    return json(route, { default_release: "qr3", releases: [{ id: "qr2" }, { id: "qr3" }] })
  }
  if (path === "/api/v1/coverage") return json(route, coverage)
  if (path === "/api/v1/mosaic" && request.method() === "GET") {
    viewColor = url.searchParams.get("color") === "1"
    requests.mosaic.push(url)
    return json(route, mosaicInfo)
  }
  if (/^\/api\/v1\/mosaic\/[^/]+\/status$/.test(path)) return json(route, viewColor ? mosaicStatus : compositeStatus)
  if (/^\/api\/v1\/mosaic\/[^/]+\/raw-metadata$/.test(path)) {
    // Color never asks for raw layers. A Composite failure is optional and
    // must leave its completed server image downloadable.
    requests.rawMetadata.push(viewColor ? "color" : "composite")
    return json(route, { error: "raw layers unavailable" }, 404)
  }
  if (/^\/api\/v1\/mosaic\/[^/]+\/tile$/.test(path)) {
    const format = url.searchParams.get("format")
    if (format) requests.rawTiles.push(format)
    return png(route)
  }
  if (/^\/api\/v1\/mosaic\/[^/]+\/display-revisions$/.test(path) && request.method() === "POST") {
    const revision = `d${displayRevisions.length}`
    displayRevisions.push(JSON.parse(request.postData()))
    return json(route, displayManifest(revision), 201)
  }
  const display = /^\/api\/v1\/mosaic\/[^/]+\/display-revisions\/([^/]+)\/(status|full|preview\.png|full\.png)$/.exec(path)
  if (display) {
    const [, revision, kind] = display
    if (!displayRevisions[Number(revision.slice(1))]) return json(route, { error: "no revision" }, 404)
    if (kind === "status") return json(route, displayManifest(revision))
    if (kind === "full") {
      fullRequested.add(revision)
      return json(route, displayManifest(revision), 202)
    }
    requests.displayPng.push(`${kind}:${revision}`)
    const body = kind === "preview.png" ? serverPreview : serverFull
    if (kind === "full.png" && holdFullGaia && new URL(page.url()).searchParams.get("gaia") === "0.7") {
      holdFullGaia = false
      await heldFull
    }
    return route.fulfill({ status: 200, contentType: "image/png", headers: { "content-length": String(body.length) }, body })
  }
  if (/^\/api\/v1\/catalog\/[^/]+$/.test(path)) {
    return json(route, {
      source: path.endsWith("gaia") ? "gaia" : "wise",
      truncated: false,
      rows: [{ id: "contract-marker", x: 64, y: 64, mag: 12 }],
    })
  }
  if (path === "/api/v1/spectrum") {
    requests.spectrum.push(url)
    return json(route, spectrumBody)
  }
  if (path === "/api/v1/observations/query" && request.method() === "POST") {
    return json(route, { dataset_id: "contract-dataset", state: "done", revision: 1 })
  }
  if (/^\/api\/v1\/observations\/[^/]+$/.test(path) && request.method() === "GET") {
    return json(route, {
      rows: snippetRows,
      matches_so_far: snippetRows.length,
      pending_files: 0,
      failed_files: 0,
      revision: 1,
      state: "done",
    })
  }
  if (/^\/api\/v1\/observations\/[^/]+$/.test(path) && request.method() === "DELETE") {
    return route.fulfill({ status: 204 })
  }
  if (path === "/api/v1/snippets/retain" && request.method() === "POST") return route.fulfill({ status: 204 })
  if (path === "/api/v1/sequences" && request.method() === "POST") {
    return json(route, { error: "contract stubs do not build sequences" }, 400)
  }
  if (path.startsWith("/api/v1/movers")) {
    if (request.method() === "POST") requests.movers.push(url)
    return json(route, { error: "movers are not a public action" }, 404)
  }
  return json(route, { error: `unmocked ${request.method()} ${path}` }, 404)
})

async function until(predicate, message, timeout = 8000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    if (await predicate()) return
    await page.waitForTimeout(60)
  }
  assert.fail(message)
}

/** Text a person can see. Visually hidden accessible labels report no box. */
function visibleText(locator) {
  return locator.evaluate((element) => {
    const walker = document.createTreeWalker(element, NodeFilter.SHOW_TEXT)
    let text = ""
    for (let node = walker.nextNode(); node; node = walker.nextNode()) {
      const range = document.createRange()
      range.selectNodeContents(node)
      const rect = range.getBoundingClientRect()
      if (rect.width > 0 && rect.height > 0) text += node.textContent ?? ""
    }
    return text.trim()
  })
}

async function gotoView(params, options = {}) {
  const view = { ...params }
  if (view.fov !== undefined) {
    view.fov_l = view.fov
    view.fov_h = view.fov
    delete view.fov
  }
  await page.goto(`${base}/?${new URLSearchParams(view).toString()}`, {
    waitUntil: "domcontentloaded",
    timeout: 60000,
  })
  if (options.canvas !== false) await waitForCanvas(page)
  await closeAllDrawers(page)
}

async function clipboard() {
  return page.evaluate(() => navigator.clipboard.readText())
}

async function copyShareLink() {
  await openDrawer(page, "Share")
  await page.evaluate(() => navigator.clipboard.writeText("").catch(() => undefined))
  const copy = page.getByRole("button", { name: /^copy link$/i })
  await copy.waitFor()
  await copy.click()
  await until(async () => (await clipboard()).length > 0, "the Share panel did not copy a link")
  return new URL(await clipboard())
}

function searchEntries(url) {
  return [...url.searchParams.entries()].sort(
    ([firstKey, firstValue], [secondKey, secondValue]) =>
      firstKey.localeCompare(secondKey) || firstValue.localeCompare(secondValue),
  )
}

function assertSameSearch(copied, current, label) {
  assert.equal(copied.origin, new URL(base).origin, `${label}: copied another origin`)
  assert.deepEqual(
    searchEntries(copied),
    searchEntries(new URL(current)),
    `${label}: the copied link does not carry the current view`,
  )
}

// The raw URL omits canonical defaults (dr, pins) that viewSearch always
// writes, so the first copy cannot match byte-for-byte. Supplied settings
// must survive and the canonical defaults must be sensible.
function assertShareCarriesSupplied(copied, supplied, label) {
  assert.equal(copied.origin, new URL(base).origin, `${label}: copied another origin`)
  for (const [key, value] of Object.entries(supplied)) {
    assert.equal(copied.searchParams.get(key), value, `${label}: supplied ${key} did not survive`)
  }
  assert.equal(copied.searchParams.get("dr"), "qr3", `${label}: canonical dr is not qr3`)
  assert.ok(
    copied.searchParams.getAll("pin").includes("42:5"),
    `${label}: canonical default pin 42:5 is missing`,
  )
}

async function revealColorConfig() {
  const red = page.getByLabel(/Red band cent(?:er|re)/i).first()
  if (!(await red.isVisible().catch(() => false))) {
    const leftToggle = page
      .getByRole("button", { name: /(show|open|expand).*(controls|settings|colou?r)/i })
      .and(page.locator("[aria-expanded]"))
      .first()
    if (await leftToggle.count()) await leftToggle.click()
  }
  await red.waitFor({ state: "visible" })
  return red
}

async function triggerExport(format) {
  const button = page.getByRole("button", { name: new RegExp(`^Download ${format}$`, "i") })
  await button.waitFor()
  assert.equal(await button.isEnabled(), true, `Download ${format} is not enabled with an image`)
  return button.click()
}

function signature(bytes) {
  if (bytes.subarray(0, 8).equals(Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]))) return "png"
  if (bytes[0] === 0xff && bytes[1] === 0xd8 && bytes[2] === 0xff) return "jpeg"
  if (bytes.subarray(0, 4).toString() === "RIFF" && bytes.subarray(8, 12).toString() === "WEBP") return "webp"
  return "unknown"
}

async function waitForPaintedCanvas(locator, timeout = 15000) {
  const deadline = Date.now() + timeout
  while (Date.now() < deadline) {
    const painted = await locator.evaluate((canvas) => {
      if (!(canvas instanceof HTMLCanvasElement)) return null
      if (canvas.width === 0 || canvas.height === 0) return null
      const context = canvas.getContext("2d")
      if (!context) return null
      const data = context.getImageData(0, 0, canvas.width, canvas.height).data
      let lit = false
      for (let i = 0; i < data.length; i += 4) {
        if (data[i] + data[i + 1] + data[i + 2] > 0) {
          lit = true
          break
        }
      }
      return { w: canvas.width, h: canvas.height, lit }
    })
    if (painted && painted.lit) return painted
    await page.waitForTimeout(150)
  }
  assert.fail("the mosaic canvas never painted a non-black pixel")
}

// 1. One-line desktop header, visible labels, modes and icon-only Share.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off" })
const header = page.locator("header").first()
await header.waitFor()
const headerBox = await header.boundingBox()
assert.ok(headerBox, "no header box")
assert.ok(headerBox.height <= 88, `desktop header is ${headerBox.height}px tall, not one line`)
const logo = header
  .locator('a[aria-label*="spearv" i], img[alt*="spearv" i], svg[aria-label*="spearv" i], a[href="/"], .brand, [data-logo]')
  .first()
await logo.waitFor()
const controls = [
  page.getByRole("textbox", { name: /^RA$/i }),
  page.getByRole("textbox", { name: /^Dec$/i }),
  page.getByRole("textbox", { name: /^FOV-L$/i }),
  page.getByRole("button", { name: /^Unlink FOV-L and FOV-H$/i }),
  page.getByRole("textbox", { name: /^FOV-H$/i }),
  page.getByRole("combobox", { name: /^Release$/i }),
  page.getByRole("button", { name: /^View$/i }),
  modeButton(page, "Color"),
  modeButton(page, "Composite"),
  modeButton(page, "Specific"),
]
for (const control of controls) {
  await control.waitFor({ state: "visible" })
  const box = await control.boundingBox()
  assert.ok(box, "a topbar control has no box")
  const centre = box.y + box.height / 2
  assert.ok(
    centre >= headerBox.y - 2 && centre <= headerBox.y + headerBox.height + 2,
    "a topbar control sits outside the one-line header",
  )
  assert.ok(box.x >= -1 && box.x + box.width <= 1441, "a topbar control sits outside the desktop viewport")
}
// The chain is local draft state: linked edits mirror, unlink preserves,
// relink copies L to H, and toggling alone never starts another mosaic.
const fovL = page.getByRole("textbox", { name: /^FOV-L$/i })
const fovH = page.getByRole("textbox", { name: /^FOV-H$/i })
const beforeChain = requests.mosaic.length
await page.getByRole("button", { name: /^Unlink FOV-L and FOV-H$/i }).click()
await fovL.fill("0.2")
assert.equal(await fovH.inputValue(), "0.1")
await page.getByRole("button", { name: /^Link FOV-L and FOV-H$/i }).click()
assert.equal(await fovH.inputValue(), "0.2")
await fovH.fill("0.3")
assert.equal(await fovL.inputValue(), "0.3")
await page.waitForTimeout(400)
assert.equal(requests.mosaic.length, beforeChain, "draft chain changes started a mosaic")
await page.getByRole("button", { name: /^View$/i }).click()
await until(() => requests.mosaic.slice(beforeChain).some((url) => url.searchParams.get("fov_l") === "0.3" && url.searchParams.get("fov_h") === "0.3"), "Show did not commit both FOV axes")
console.log("PASS FOV chain draft and rectangular commit")

// Only Share is icon-only by request. Layers and Spectrum keep readable
// labels, each with an accessible name and an icon.
const shareButton = await findPanelButton(page, "Share")
await shareButton.waitFor({ state: "visible" })
assert.equal(await visibleText(shareButton), "", "Share must be an icon-only toolbar button")
assert.ok((await shareButton.locator("svg, img").count()) > 0, "Share needs an icon")
assert.equal(await drawerIsOpen(page, shareButton), false, "Share drawer must start closed")
for (const name of ["Layers", "Spectrum"]) {
  const button = await findPanelButton(page, name)
  await button.waitFor({ state: "visible" })
  const accessible = (await button.getAttribute("aria-label")) ?? (await button.textContent()) ?? ""
  assert.match(accessible, new RegExp(name, "i"), `${name} needs an accessible action name`)
  assert.ok((await button.locator("svg, img").count()) > 0, `${name} needs an icon`)
  assert.equal(await drawerIsOpen(page, button), false, `${name} drawer must start closed`)
}
console.log("PASS one-line desktop header, labels, modes and icon-only Share")

// 2. No removed public actions.
assert.equal(await page.getByRole("button", { name: /^(help|image|copy embed code|find movers|movers)$/i }).count(), 0)
assert.equal(await page.getByRole("link", { name: /^(help|image|copy embed code|find movers|movers)$/i }).count(), 0)
assert.equal(await page.locator(".embed-actions, #slot-movers, #slot-image").count(), 0)
assert.equal(await page.getByText(/find movers/i).count(), 0)
console.log("PASS no public Help, Image, embed-copy or movers actions")

// 3. Each right drawer closes, reopens and answers Escape.
for (const name of ["Share", "Layers", "Spectrum"]) {
  const button = await openDrawer(page, name)
  assert.equal(await drawerIsOpen(page, button), true, `${name} did not open`)
  await closeDrawer(page, name)
  assert.equal(await drawerIsOpen(page, button), false, `${name} did not close`)
  await openDrawer(page, name)
  await page.keyboard.press("Escape")
  await waitForDrawer(page, button, false)
  console.log(`PASS ${name} drawer close/reopen/Escape`)
}

// 4. A movers=on link exposes no movers UI and sends no movers request.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off", movers: "on" })
await page.waitForTimeout(600)
assert.equal(requests.movers.length, 0, `a movers request escaped: ${requests.movers.join(", ")}`)
assert.equal(await page.getByRole("button", { name: /movers/i }).count(), 0)
assert.equal(await page.getByRole("link", { name: /movers/i }).count(), 0)
assert.equal(await page.getByText(/find movers/i).count(), 0)
console.log("PASS movers=on link exposes no public movers action")

// 5. Share copies the current view and exports real PNG, JPEG and WebP files.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off" })
const firstCopy = await copyShareLink()
assertShareCarriesSupplied(
  firstCopy,
  { ra: "56.75", dec: "24.12", fov_l: "0.1", fov_h: "0.1", mode: "color", mask: "off" },
  "initial copy",
)
await closeDrawer(page, "Share")
await modeButton(page, "Composite").click()
await page.waitForFunction(() => new URL(location.href).searchParams.get("mode") === "composite")
await toggleLayer(page, "Gaia")
await page.waitForFunction(() => new URL(location.href).searchParams.has("gaia"))
await closeDrawer(page, "Layers")
const changedCopy = await copyShareLink()
assertSameSearch(changedCopy, page.url(), "copy after mode and layer changes")
assert.notDeepEqual(searchEntries(changedCopy), searchEntries(firstCopy))
// 5a. Color: the visible native server PNG, with the original cached Blob.
holdFullGaia = true
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off", gaia: "0.7" })
await openDrawer(page, "Share")
const colorPreview = page.locator('img.mosaic-server-image[alt^="Bounded Color preview"]').first()
await colorPreview.waitFor({ state: "visible", timeout: 15000 })
assert.deepEqual(await colorPreview.evaluate((img) => [img.naturalWidth, img.naturalHeight]), [32, 32], "the bounded Color preview is 32x32")
const colorDownload = page.getByRole("button", { name: "Download PNG", exact: true })
assert.equal(await colorDownload.isDisabled(), true, "Color download waits for the native full PNG")
await page.getByText(/Preparing the original full-resolution Color PNG/).waitFor({ timeout: 5000 })
releaseHeldFull()
const colorFull = page.locator('img[alt="Server-rendered full-resolution Color PNG."]')
await colorFull.waitFor({ state: "visible", timeout: 15000 })
assert.deepEqual(await colorFull.evaluate((img) => [img.naturalWidth, img.naturalHeight]), [128, 128], "the visible Color PNG is the native 128x128 server image")
await page.getByText(/The exact current full-resolution server PNG is ready to download/).waitFor({ timeout: 5000 })
assert.equal(await page.locator('input[name="share-export-format"]').count(), 1, "Color offers only the original PNG")
const displayPngsBefore = requests.displayPng.length
const [colorSaved] = await Promise.all([page.waitForEvent("download", { timeout: 30000 }), colorDownload.click()])
const colorPath = await colorSaved.path()
assert.ok(colorPath, "Color download has no file")
const colorBytes = await readFile(colorPath)
assert.match(colorSaved.suggestedFilename(), /\.png$/i, "Color filename is not .png")
assert.equal(signature(colorBytes), "png", "Color download is not a PNG")
assert.deepEqual(colorBytes, serverFull, "Color download is not the exact cached server PNG")
const colorParsed = parsePng(colorBytes)
assert.deepEqual([colorParsed.width, colorParsed.height], [128, 128], "Color download is not the native 128x128 server PNG")
assert.ok(pngHasNonBlack(colorParsed), "Color download is black")
await page.getByText("Saved the exact full-resolution server PNG.").waitFor({ timeout: 5000 })
assert.equal(requests.displayPng.length, displayPngsBefore, "the cached PNG download made no new artifact request")
assert.deepEqual(requests.rawMetadata.filter((mode) => mode === "color"), [], "Color must not request raw metadata")
assert.deepEqual(requests.rawTiles, [], "Color must not request scale16/f32 raw tiles")
console.log(`PASS Share Color original PNG: ${colorBytes.length} bytes, 128x128, cached`)
await page.screenshot({ path: join(shots, "share-color.png"), fullPage: true })

// 5b. Composite keeps the canvas PNG/JPEG/WebP export. Its optional raw
// request fails, so the still-complete server image must stay downloadable.
const colorCanvas = await page.locator(".mosaic canvas").elementHandle()
await closeDrawer(page, "Share")
await modeButton(page, "Composite").click()
await page.waitForFunction(() => new URL(location.href).searchParams.get("mode") === "composite")
await page.waitForFunction((previous) => document.querySelector(".mosaic canvas") !== previous, colorCanvas)
await colorCanvas?.dispose()
await openDrawer(page, "Share")
const exportCanvas = page.locator("canvas[data-export-image]").first()
await exportCanvas.waitFor({ state: "attached" })
const painted = await waitForPaintedCanvas(exportCanvas)
assert.deepEqual([painted.w, painted.h], [128, 128], `export canvas is ${painted.w}x${painted.h}, not 128x128`)
await until(() => requests.rawMetadata.includes("composite"), "the Composite raw request did not fail as expected", 5000)
for (const [format, expected] of [["PNG", "png"], ["JPEG", "jpeg"], ["WebP", "webp"]]) {
  const [download] = await Promise.all([
    page.waitForEvent("download", { timeout: 30000 }),
    triggerExport(format),
  ])
  const path = await download.path()
  assert.ok(path, `${format} download has no file`)
  const bytes = await readFile(path)
  assert.ok(bytes.length > 32, `${format} download is only ${bytes.length} bytes`)
  assert.match(
    download.suggestedFilename(),
    new RegExp(`\\.${expected === "jpeg" ? "jpe?g" : expected}$`, "i"),
    `${format} filename is ${download.suggestedFilename()}`,
  )
  assert.equal(signature(bytes), expected, `${format} download is not a ${format} image`)
  if (expected === "png") {
    const parsed = parsePng(bytes)
    assert.deepEqual([parsed.width, parsed.height], [128, 128], `exported PNG is ${parsed.width}x${parsed.height}, not 128x128`)
    assert.ok(pngHasNonBlack(parsed), "exported PNG is all black: fixture tiles never painted")
  }
  console.log(`PASS Share ${format} download: ${bytes.length} bytes, 128x128`)
}
assert.equal(requests.rawTiles.length, 0, "a failed raw request must not fetch a raw tile")
assert.equal(await page.getByRole("button", { name: "Download PNG", exact: true }).isEnabled(), true, "Composite stays downloadable after the optional raw failure")
await page.screenshot({ path: join(shots, "share-export.png"), fullPage: true })

// 6. The copied view survives mode and layer changes and a reload.
await page.goto(changedCopy.href, { waitUntil: "domcontentloaded" })
await waitForCanvas(page)
await until(
  async () => (await modeButton(page, "Composite").getAttribute("aria-pressed")) === "true",
  "the copied link did not restore Composite mode",
)
await openDrawer(page, "Layers")
const gaiaButton = layerRow(page, "Gaia").getByRole("button", { name: /show|hide/i }).first()
await until(
  async () => (await gaiaButton.getAttribute("aria-pressed")) === "true",
  "the copied link did not restore the Gaia layer",
)
assert.equal(new URL(page.url()).searchParams.get("gaia"), "0.7")
await closeDrawer(page, "Layers")
console.log("PASS copied link keeps mode and layer changes after reload")

// 7. Colour configuration round-trips through the URL owner.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off" })
const redBand = await revealColorConfig()
const redWidth = page.getByLabel(/Red band width/i).first()
await redWidth.waitFor({ state: "visible" })
await redBand.fill("12")
await redWidth.fill("4")
await until(
  () =>
    requests.mosaic.some(
      (url) => url.searchParams.get("color_r") === "11" && url.searchParams.get("color_r_width") === "4",
    ),
  "the changed red colour recipe never reached the mosaic request",
)
const colourUrl = page.url()
assert.ok(/colou?r/i.test(colourUrl), `the colour recipe is not in the URL: ${colourUrl}`)
const mosaicBeforeReload = requests.mosaic.length
await page.goto(colourUrl, { waitUntil: "domcontentloaded" })
await waitForCanvas(page)
await revealColorConfig()
assert.equal(await page.getByLabel(/Red band cent(?:er|re)/i).first().inputValue(), "12")
assert.equal(await page.getByLabel(/Red band width/i).first().inputValue(), "4")
await until(
  () =>
    requests.mosaic
      .slice(mosaicBeforeReload)
      .some((url) => url.searchParams.get("color_r") === "11" && url.searchParams.get("color_r_width") === "4"),
  "reloading the colour link did not request the saved recipe",
)
console.log("PASS colour channels round-trip through the URL and the mosaic request")

// 8. Specific editor is on the left, closes and reopens, and its table fits.
const firstObservation = page.waitForRequest(
  (request) =>
    request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/observations/query",
)
await gotoView(
  {
    ra: 56.75,
    dec: 24.12,
    fov: 0.1,
    mode: "specific",
    from_utc: "2025-01-01T00:00:00.000Z",
    to_utc: "2025-01-02T00:00:00.000Z",
    releases: "qr3,qr2",
    bands: "1-3;66",
  },
  { canvas: false },
)
const boundsBody = (await firstObservation).postDataJSON()
assert.deepEqual(boundsBody.region, { ra: 56.75, dec: 24.12, fov: 0.1 })
assert.equal(boundsBody.from_utc, "2025-01-01T00:00:00.000Z")
assert.equal(boundsBody.to_utc, "2025-01-02T00:00:00.000Z")
assert.deepEqual(boundsBody.releases, ["qr2", "qr3"])
assert.equal(boundsBody.bands, "1;2;3;66")
assert.equal(await page.getByLabel(/^(UTC start|start UTC)$/i).inputValue(), "2025-01-01T00:00")
assert.equal(await page.getByLabel(/^(UTC finish|finish UTC|UTC end|end UTC)$/i).inputValue(), "2025-01-02T00:00")
assert.equal(await page.getByLabel("Releases", { exact: true }).inputValue(), "both")
assert.equal(await page.getByLabel(/Bands \(1.102\)/i).inputValue(), "1-3;66")
const specificRoot = page.locator('.specific-workspace, main[aria-label*="Specific" i]').first()
const editor = page
  .locator('.specific-sidebar, aside[aria-label*="sector editor" i], aside[aria-label*="specific" i]')
  .first()
await editor.waitFor({ state: "visible" })
const editorBox = await editor.boundingBox()
assert.ok(editorBox, "the Specific editor has no box")
assert.ok(editorBox.x < 720, `Specific editor starts at x=${editorBox.x}, not on the left`)
assert.equal(
  await editor.getByRole("button", { name: /copy.*(location|bounds|link)/i }).count(),
  0,
  "the removed Specific local copy action is still public",
)
await page.locator(".snippet-scroll tbody tr").first().waitFor()
const tableFit = await page.evaluate(() => {
  const scroller = document.querySelector(".snippet-scroll")
  const table = scroller?.querySelector("table")
  if (!scroller || !table) return null
  const s = scroller.getBoundingClientRect()
  const t = table.getBoundingClientRect()
  return { scroll: scroller.scrollWidth, client: scroller.clientWidth, table: t.width, left: t.left - s.left }
})
assert.ok(tableFit, "the Specific snippet table is missing")
// The snippet table is 2D data but must fit its sidebar column exactly.
assert.ok(tableFit.scroll <= tableFit.client + 1, `snippet scroller overflows: ${JSON.stringify(tableFit)}`)
assert.ok(tableFit.table <= tableFit.client + 1, `snippet table is wider than its column: ${JSON.stringify(tableFit)}`)
assert.ok(tableFit.left >= -1, "the snippet table starts outside its scroller")
await assertNoPageOverflow(page)
const reloadedObservation = page.waitForRequest(
  (request) =>
    request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/observations/query",
)
await page.reload({ waitUntil: "domcontentloaded" })
assert.deepEqual((await reloadedObservation).postDataJSON(), boundsBody, "reload lost the URL bounds")
assert.equal(await page.getByLabel(/^(UTC start|start UTC)$/i).inputValue(), "2025-01-01T00:00")
assert.equal(await page.getByLabel(/Bands \(1.102\)/i).inputValue(), "1-3;66")
await page.locator(".snippet-scroll tbody tr").first().waitFor()
const hideEditor = specificRoot
  .getByRole("button", { name: /(hide|close|collapse).*(editor|sector|control|panel)/i })
  .first()
await hideEditor.waitFor({ state: "visible" })
await hideEditor.click()
await until(() => editor.isVisible().then((visible) => !visible), "the Specific editor did not close")
const showEditor = specificRoot
  .getByRole("button", { name: /(show|open|expand).*(editor|sector|control|panel)/i })
  .first()
await showEditor.waitFor({ state: "visible" })
await showEditor.click()
await editor.waitFor({ state: "visible" })
await assertNoPageOverflow(page)
await page.screenshot({ path: join(shots, "specific-editor.png"), fullPage: true })
console.log("PASS Specific editor on the left, close/reopen and fitted snippet table")

// 9. Default bounds can be edited, shared and reloaded.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "specific" }, { canvas: false })
await page.locator(".snippet-scroll tbody tr").first().waitFor()
assert.equal(await page.getByLabel(/^(UTC start|start UTC)$/i).inputValue(), "")
assert.equal(await page.getByLabel(/Bands \(1.102\)/i).inputValue(), "1-102")
const editedObservation = page.waitForRequest(
  (request) =>
    request.method() === "POST" && new URL(request.url()).pathname === "/api/v1/observations/query",
)
await page.getByLabel(/^(UTC start|start UTC)$/i).fill("2025-01-01T00:00")
await page.getByLabel(/^(UTC finish|finish UTC|UTC end|end UTC)$/i).fill("2025-01-02T00:00")
await page.getByLabel(/Bands \(1.102\)/i).fill("1-3;66")
await page.getByRole("button", { name: /^apply bounds$/i }).click()
const editedBody = (await editedObservation).postDataJSON()
assert.equal(editedBody.from_utc, "2025-01-01T00:00:00.000Z")
assert.equal(editedBody.to_utc, "2025-01-02T00:00:00.000Z")
assert.equal(editedBody.bands, "1;2;3;66")
await until(
  () => new URL(page.url()).searchParams.get("from_utc") === "2025-01-01T00:00:00.000Z",
  "edited bounds never reached the URL",
)
const boundsCopy = await copyShareLink()
assert.equal(boundsCopy.searchParams.get("from_utc"), "2025-01-01T00:00:00.000Z")
assert.equal(boundsCopy.searchParams.get("to_utc"), "2025-01-02T00:00:00.000Z")
assert.equal(boundsCopy.searchParams.get("bands"), "1-3;66")
await closeDrawer(page, "Share")
await page.goto(boundsCopy.href, { waitUntil: "domcontentloaded" })
await page.locator(".snippet-scroll tbody tr").first().waitFor()
assert.equal(await page.getByLabel(/^(UTC start|start UTC)$/i).inputValue(), "2025-01-01T00:00")
assert.equal(await page.getByLabel(/Bands \(1.102\)/i).inputValue(), "1-3;66")
await assertNoPageOverflow(page)
console.log("PASS default bounds edit to shared link round-trip")

// 10. Sectors persist across mode toggles.
await gotoView(
  {
    ra: 56.75,
    dec: 24.12,
    fov: 0.1,
    mode: "specific",
    from_utc: "2025-01-01T00:00:00.000Z",
    to_utc: "2025-01-02T00:00:00.000Z",
    releases: "qr3,qr2",
    bands: "1-3;66",
  },
  { canvas: false },
)
await page.locator(".snippet-scroll tbody tr").first().waitFor()
await page.getByRole("button", { name: /^new sector$/i }).click()
const sectorNode = page.locator(".sector-node").first()
await sectorNode.waitFor()
const sectorName = (await sectorNode.textContent())?.trim() ?? ""
assert.ok(/sector 1/i.test(sectorName), `new sector did not appear: ${sectorName}`)
const joinButton = page.locator(".snippet-scroll tbody tr").first().getByRole("button", { name: /join band/i })
await joinButton.waitFor({ state: "visible" })
await joinButton.click()
await page.locator(".sector-members li").first().waitFor()
await modeButton(page, "Composite").click()
await waitForCanvas(page)
await modeButton(page, "Specific").click()
await page.locator(".snippet-scroll tbody tr").first().waitFor()
assert.ok((await page.locator(".sector-node").count()) >= 1, "sector did not survive mode toggles")
assert.ok((await page.locator(".sector-members li").count()) >= 1, "sector member did not survive mode toggles")
await assertNoPageOverflow(page)
console.log("PASS Specific sector persists across mode toggles")

// 11. No export without a rendered image.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "specific" }, { canvas: false })
await openDrawer(page, "Share")
for (const format of ["PNG", "JPEG", "WebP"]) {
  const button = page.getByRole("button", { name: new RegExp(`^Download ${format}$`, "i") })
  await button.waitFor()
  assert.equal(await button.isEnabled(), false, `Download ${format} is enabled with no image`)
}
await page.getByText(/no rendered image yet/i).first().waitFor()
await closeDrawer(page, "Share")
console.log("PASS empty image leaves exports disabled")

// 12. Spectrum table lives in a bounded panel scroller, not the page.
await gotoView({ ra: 56.75, dec: 24.12, fov: 0.1, mode: "color", mask: "off" })
await openDrawer(page, "Spectrum")
const canvas = mainCanvas(page)
const box = await canvas.boundingBox()
assert.ok(box, "no canvas box for the spectrum click")
const [spectrumRequest] = await Promise.all([
  page.waitForRequest(/\/api\/v1\/spectrum\?/),
  page.mouse.click(box.x + box.width * 0.5, box.y + box.height * 0.5),
])
assert.ok(spectrumRequest.url().includes("/api/v1/spectrum"))
await page.locator(".spectrum-panel-chart, #slot-spectrum svg").first().waitFor()
const measurements = page.getByText(/^Band measurements\b/i).first()
await measurements.waitFor()
const details = measurements.locator("xpath=ancestor::details[1]")
assert.equal(await details.evaluate((element) => element.open), false, "Band measurements must start collapsed")
await measurements.click()
assert.equal(await details.evaluate((element) => element.open), true, "Band measurements did not expand")
const spectrumWrap = page.locator(".spectrum-panel-table-wrap").first()
await spectrumWrap.waitFor({ state: "visible" })
assert.ok((await spectrumWrap.locator("tbody tr").count()) > 0, "the expanded table has no rows")
const wrapBox = await spectrumWrap.boundingBox()
const panelBox = await page.locator(".spectrum-panel, #slot-spectrum").first().boundingBox()
assert.ok(wrapBox && panelBox, "no spectrum panel box")
// The 2D table may scroll inside its own bounded container; the container
// itself must fit the panel and never push the page sideways.
assert.ok(wrapBox.width <= panelBox.width + 1, `spectrum container is wider than its panel: ${wrapBox.width} > ${panelBox.width}`)
await assertNoPageOverflow(page)
console.log("PASS Spectrum Band measurements starts collapsed, expands and stays in-panel")

spectrumBody = invalidSpectrum
const [invalidRequest] = await Promise.all([
  page.waitForRequest(/\/api\/v1\/spectrum\?/),
  page.mouse.click(box.x + box.width * 0.55, box.y + box.height * 0.55),
])
assert.ok(invalidRequest.url().includes("/api/v1/spectrum"))
await page.getByText(/No valid measurements/i).first().waitFor()
assert.ok(requests.spectrum.length >= 2, "invalid-only click did not reach the spectrum route")
await assertNoPageOverflow(page)
await page.screenshot({ path: join(shots, "spectrum-invalid.png"), fullPage: true })
console.log("PASS Spectrum invalid-only data says No valid measurements")

assert.deepEqual(pageErrors, [])
console.log("all UI contract checks passed")
} finally {
  await context.close().catch(() => undefined)
  await browser.close().catch(() => undefined)
}
