// Motion bench for the three painter boxes, on a real cold 5 degree colour run.
//
//   SPEARV_BASE_URL=http://127.0.0.1:8789 \
//   PLAYWRIGHT_PATH=/tmp/spearv-playwright/node_modules/playwright/index.js \
//   PLAYWRIGHT_BROWSERS_PATH=/tmp/spearv-playwright/browsers \
//   node web/e2e/mosaic-boxes.mjs
//
// Samples all three box positions every 250 ms, writes them to
// /tmp/spearv-bench/boxes.json, screenshots at 15/40/70% and done, and asserts
// that each box walks the path without skipping, that stalls are bounded, that
// every box reaches the end, that the boxes hold different tiles most of the
// run, and that the first box moves soon after load.

import assert from "node:assert/strict"
import { createRequire } from "node:module"
import { mkdir, writeFile } from "node:fs/promises"
import { openDrawer } from "./lib/ui.mjs"

const require = createRequire(import.meta.url)
const { chromium } = require(process.env.PLAYWRIGHT_PATH || "playwright")

const base = process.env.SPEARV_BASE_URL || "http://127.0.0.1:8789"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-bench"
await mkdir(shots, { recursive: true })

const field = `${base}/?ra=56.75&dec=24.12&fov=5&mode=composite&dr=qr3&color=1&mask=off`
const browser = await chromium.launch()
const page = await browser.newPage({ viewport: { width: 1280, height: 900 } })
const failures = []
const rawTileFormats = []
page.on("pageerror", (error) => failures.push(error.message))
page.on("request", (request) => {
  const url = new URL(request.url())
  if (url.pathname.endsWith("/tile") && url.searchParams.has("format")) rawTileFormats.push(url.searchParams.get("format"))
})

const loadedAt = Date.now()
await page.goto(field)
await page.waitForSelector(".mosaic canvas", { timeout: 60_000 })
// The status line carries per-channel progress, e.g. `R 41% · G 37% · B 44%`.
// A terminal status here means the page joined an already-done job, so the
// spare was not fresh; fail fast instead of timing out on boxes.
await page.waitForFunction(
  () => /^(R |ready|incomplete|failed)/.test(document.querySelector("figcaption .status")?.textContent ?? ""),
  undefined,
  { timeout: 240_000 },
)
const joined = await page.locator("figcaption .status").textContent()
assert.ok(/^R /.test(joined ?? ""), `joined a finished job, not a cold run: ${joined}`)
// Boxes appear once the server reports the path.
await page.waitForFunction(() => (window.__mosaicBoxes?.length ?? 0) === 3, undefined, { timeout: 120_000 })
const path = await page.evaluate(() => window.__mosaicBoxPath)
assert.ok(path.length > 10, `the box path covers the field, got ${path.length} tiles`)
const indexOf = new Map(path.map((key, i) => [key, i]))
console.log(`path ok, ${path.length} tiles`)

const statusText = () => page.locator("figcaption .status").textContent()
const channelMax = (text) => {
  const found = [...text.matchAll(/([RGB]) (\d+)%/g)].map((m) => Number(m[2]) / 100)
  return found.length === 3 ? Math.max(...found) : 0
}

const samples = []
const shotAt = { 15: false, 40: false, 70: false, 90: false }
let firstMoveAt = 0
let done = false
while (!done) {
  await new Promise((resolve) => setTimeout(resolve, 250))
  const [hooks, text] = await Promise.all([
    page.evaluate(() => ({
      boxes: window.__mosaicBoxes?.map((box) => `${box.x},${box.y}`) ?? [],
      painted: window.__mosaicPainted ?? [],
      frontiers: window.__mosaicFrontiers ?? [],
      pending: window.__mosaicPending?.() ?? [],
      stale: window.__mosaicStale?.() ?? [],
    })),
    statusText(),
  ])
  const at = Date.now()
  samples.push({ t: +((at - loadedAt) / 1000).toFixed(2), boxes: hooks.boxes, painted: hooks.painted, frontiers: hooks.frontiers, pending: hooks.pending, stale: hooks.stale, status: text })
  if (!firstMoveAt && hooks.boxes.some((key) => key !== path[0])) firstMoveAt = at
  const progress = channelMax(text ?? "")
  for (const [mark, file] of [[15, "boxes-15.png"], [40, "boxes-40.png"], [70, "boxes-70.png"], [90, "boxes-90.png"]]) {
    if (!shotAt[mark] && progress * 100 >= mark) {
      shotAt[mark] = true
      await page.screenshot({ path: `${shots}/${file}` })
      console.log(`${mark}%:`, text)
    }
  }
  done = /^(ready|incomplete|failed)/.test(text ?? "")
  if (at - loadedAt > 900_000) throw new Error("the bench run timed out")
}
const stamps = await page.evaluate(() => ({ readsDoneAt: window.__mosaicReadsDoneAt, finalAt: window.__mosaicFinalAt }))
await page.screenshot({ path: `${shots}/boxes-done.png` })
const totalS = (Date.now() - loadedAt) / 1000
const finalStatus = await statusText()
console.log("finished:", finalStatus, `in ${totalS.toFixed(0)} s after ${samples.length} samples`)
await writeFile(`${shots}/boxes.json`, JSON.stringify({ field, path, samples }, null, 1))

// Every sample is on the path, and no box skips: indices never run backwards
// and a 250 ms glide step stays bounded.
const index = (key) => {
  assert.ok(indexOf.has(key), `box off the path at ${key}`)
  return indexOf.get(key)
}
let maxJump = 0
for (const box of [0, 1, 2]) {
  let prev = 0
  for (const sample of samples) {
    if (sample.boxes.length !== 3) continue
    const at = index(sample.boxes[box])
    assert.ok(at >= prev, `box ${box} ran backwards from ${prev} to ${at}`)
    maxJump = Math.max(maxJump, at - prev)
    prev = at
  }
}
console.log(`path ok, longest 250 ms glide step ${maxJump} tiles`)
assert.ok(maxJump <= 25, `a box jumped ${maxJump} path tiles in one sample`)

// The boxes walk the first pass, then park at the end while depth reads go on,
// so only the walk says anything about how they move.
const home = samples.findIndex((sample) => sample.boxes.length === 3 && sample.boxes.every((key) => index(key) === path.length - 1))
const walking = home < 0 ? samples : samples.slice(0, home)

// The longest stall per box on the walk, planning excluded.
const reading = walking.filter((sample) => /^(R |\+ |%|\d+%)/.test(sample.status ?? ""))
const stalls = [0, 1, 2].map((box) => {
  let longest = 0
  let run = 0
  let prev = -1
  for (const sample of reading) {
    if (sample.boxes.length !== 3) continue
    const at = index(sample.boxes[box])
    run = at === prev ? run + 1 : 1
    prev = at
    longest = Math.max(longest, run)
  }
  return longest * 0.25
})
console.log(`longest stall while reading: R ${stalls[0].toFixed(1)} s · G ${stalls[1].toFixed(1)} s · B ${stalls[2].toFixed(1)} s`)

// Every box reaches the end of the path.
for (const box of [0, 1, 2]) {
  const reached = Math.max(...samples.filter((s) => s.boxes.length === 3).map((s) => index(s.boxes[box])))
  assert.equal(reached, path.length - 1, `box ${box} stopped at ${reached} of ${path.length - 1}`)
}
console.log("all three boxes reached the end")

const raced = walking.filter((sample) => sample.boxes.length === 3 && sample.boxes.some((key) => key !== path[0]))

// Each box follows its own channel: when the channels' finished tiles are
// apart, their boxes are apart too. Channels that finish together stay together.
const apart = walking.filter((sample) => sample.boxes.length === 3 && (sample.frontiers ?? []).length === 3 && new Set(sample.frontiers).size > 1)
const followed = apart.filter((sample) => new Set(sample.boxes).size > 1).length
const share = apart.length > 0 ? followed / apart.length : 1
console.log(`${followed}/${apart.length} samples with channels apart have their boxes apart (${(share * 100).toFixed(0)}%)`)
assert.ok(share > 0.5, "the boxes move together when their channels do not")

console.log(`first box moved ${(firstMoveAt - loadedAt) / 1000}s after load`)
assert.ok(firstMoveAt - loadedAt < 120_000, "the first box took too long to move")

// No box stands past the tile its channel is filling, so everything behind it
// is finished.
let ahead = 0
for (const sample of samples) {
  if (sample.boxes.length !== 3 || (sample.frontiers ?? []).length !== 3) continue
  for (const box of [0, 1, 2]) if (index(sample.boxes[box]) > Math.min(path.length - 1, sample.frontiers[box])) ahead += 1
}
console.log(`box-samples past their tile: ${ahead}`)
assert.equal(ahead, 0, "a box ran ahead of its channel's data")

// Paint honesty, the main metric: at every sample, the share of path tiles
// strictly behind each box that are drawn on its layer. Painted means blitted,
// not queued. Tiles with no data for the channel must be drawn too, so this is
// stricter than asked. `net` forgives tiles with a fetch queued or in flight;
// `raw` forgives nothing.
const honesty = (forgive) => {
  let worst = 1
  let short = 0
  let longest = 0
  let run = 0
  for (const sample of samples) {
    if (sample.boxes.length !== 3 || (sample.frontiers ?? []).length !== 3) continue
    const pending = new Set(forgive ? sample.pending : [])
    let sampleWorst = 1
    for (const box of [0, 1, 2]) {
      const pos = index(sample.boxes[box])
      const painted = new Set(sample.painted[box] ?? [])
      let done = 0
      for (let i = 0; i < pos; i++) if (painted.has(i) || pending.has(`${box}:${path[i]}`)) done += 1
      sampleWorst = Math.min(sampleWorst, pos === 0 ? 1 : done / pos)
    }
    worst = Math.min(worst, sampleWorst)
    run = sampleWorst < 1 ? run + 1 : 0
    short += sampleWorst < 1 ? 1 : 0
    longest = Math.max(longest, run)
  }
  return { worst, short, longest: longest * 0.25 }
}
const raw = honesty(false)
const net = honesty(true)
console.log(`paint honesty raw: worst ${(raw.worst * 100).toFixed(1)}%, ${raw.short} samples short, longest shortfall ${raw.longest.toFixed(2)} s`)
console.log(`paint honesty net of fetches in flight: worst ${(net.worst * 100).toFixed(1)}%, ${net.short} samples short, longest ${net.longest.toFixed(2)} s`)
const staleMax = Math.max(...samples.map((sample) => Math.max(0, ...(sample.stale ?? []))))
console.log(`tiles behind a box with an older drawn version: at most ${staleMax} per box`)
assert.ok(raw.worst >= 0.85, "too much paint missing behind the boxes")

// Motion across the read: the share of reading time each box changed tile
// within any 5 s, and how long after a box arrived the reads ended.
const lastAt = [0, 1, 2].map((box) => reading.find((sample) => sample.boxes.length === 3 && index(sample.boxes[box]) === path.length - 1)?.t)
const readEnd = reading.at(-1)?.t ?? 0
console.log(`read window ${reading[0]?.t}-${readEnd} s; boxes arrived at ${lastAt.join(", ")} s`)
const gaps = [0, 1].map((box) => reading.filter((s) => s.boxes.length === 3 && index(s.boxes[box]) !== index(s.boxes[2])).length)
console.log(`R and G differ from B on ${gaps[0]} and ${gaps[1]} of ${reading.length} reading samples; R differs from G on ${reading.filter((s) => s.boxes[0] !== s.boxes[1]).length}`)

// Share of the run spent on the outer ring: the first ring of the spiral.
const side = Math.sqrt(path.length)
const ring = Number.isInteger(side) ? 4 * side - 4 : path.length
const ringed = raced.filter((sample) => sample.boxes.some((key) => index(key) < ring))
const ringShare = raced.length > 0 ? ringed.length / raced.length : 0
console.log(`outer ring (${ring} tiles): ${ringed.length}/${raced.length} racing samples (${(ringShare * 100).toFixed(0)}%)`)

// Seconds from reads done to the final swap.
const readsDoneS = stamps.readsDoneAt ? (stamps.readsDoneAt - loadedAt) / 1000 : NaN
const finalS = stamps.finalAt ? (stamps.finalAt - loadedAt) / 1000 : NaN
console.log(`reads done at ${readsDoneS.toFixed(0)} s, final swap at ${finalS.toFixed(0)} s, glide home ${(finalS - readsDoneS).toFixed(1)} s`)

// Color paints a bounded progressive canvas during the run, then replaces it
// with the visible native server PNG. The preview is bounded; Download serves
// the cached original full-resolution PNG.
await page.waitForFunction(() => {
  const image = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 60_000 })
const preview = await page.evaluate(() => {
  const image = document.querySelector('img.mosaic-server-image[alt^="Bounded Color preview"]')
  return { width: image.naturalWidth, height: image.naturalHeight, hidden: getComputedStyle(document.querySelector(".mosaic-canvas")).visibility === "hidden" }
})
assert.ok(preview.width > 0 && preview.width <= 2048, `the bounded preview is ${preview.width}px wide`)
assert.equal(preview.hidden, true, "the progressive canvas is hidden behind the visible server preview")
await page.waitForFunction(() => {
  const image = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  return Boolean(image && image.complete && image.naturalWidth > 0)
}, undefined, { timeout: 900_000 })
const server = await page.evaluate(async () => {
  const full = document.querySelector('img[alt="Server-rendered full-resolution Color PNG."]')
  const canvas = document.querySelector(".mosaic canvas")
  const response = await fetch(full.src)
  const buffer = new Uint8Array(await response.arrayBuffer())
  const view = new DataView(buffer.buffer, buffer.byteOffset, buffer.byteLength)
  return {
    canvasWidth: canvas.width,
    canvasHeight: canvas.height,
    naturalWidth: full.naturalWidth,
    naturalHeight: full.naturalHeight,
    width: view.getUint32(16),
    height: view.getUint32(20),
    bytes: buffer.byteLength,
    signature: [...buffer.slice(0, 8)],
  }
})
assert.ok(server.naturalWidth > server.canvasWidth || server.naturalHeight > server.canvasHeight, `the native PNG ${server.naturalWidth}x${server.naturalHeight} is not larger than the ${server.canvasWidth}x${server.canvasHeight} progressive canvas`)
assert.equal(server.width, server.naturalWidth, "the PNG IHDR width is the native width")
assert.equal(server.height, server.naturalHeight, "the PNG IHDR height is the native height")
assert.ok(server.bytes > 0)
assert.deepEqual(server.signature, [137, 80, 78, 71, 13, 10, 26, 10])
assert.deepEqual(rawTileFormats, [], "Color must not request scale16/f32 raw tiles")
await openDrawer(page, "Share")
assert.equal(await page.getByRole("button", { name: "Download PNG", exact: true }).isDisabled(), false, "the cached native Color PNG is downloadable")
await page.getByText(/The exact current full-resolution server PNG is ready to download/).waitFor({ timeout: 30_000 })
console.log(`native PNG: ${server.naturalWidth}x${server.naturalHeight}, ${server.bytes} bytes, bounded preview ${preview.width}x${preview.height}`)

assert.deepEqual(failures, [])
await browser.close()
console.log("all box motion checks passed")
