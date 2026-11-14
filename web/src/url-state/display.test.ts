// @ts-nocheck
import { describe, it, before, after } from "node:test"
import assert from "node:assert/strict"
import { readFileSync } from "node:fs"
import { runInNewContext } from "node:vm"
import { DEFAULT_DISPLAY, displayOf, resolveDisplay, withDisplay } from "./display.ts"
import { readView, viewSearch } from "./view.ts"
import { NO_DATA, mixRawTile } from "../features/mosaic/raw-mix.ts"
import { rawBytes, validateRawMetadata } from "../features/mosaic/raw-protocol.ts"
import { RawClient } from "../features/mosaic/raw-client.ts"
import { rangeCommit } from "../features/mosaic/range.ts"

const md = (width = 1, height = 1, color = false) => ({ format_version: color ? 2 : 1, job_id: "fixture", state: "done", width, height, tile_size: 256,
  dtype: color ? "u16le" : "f32le", orientation: "png-top-row-first", nodata: color ? "65535" : "nan", source_recipe: { mode: color ? "color" : "composite", mask: true, pins: [] },
  channels: Array.from({ length: color ? 3 : 1 }, (_, index) => ({ index, role: color ? ["blue", "green", "red"][index] : "grey", center_band: null, width: 102, bands: [1, 2, 3],
    wavelength_min_um: 1, wavelength_max_um: 2, finite_count: width * height, default_black: 0, default_white: 1, default_span: 1,
    scale_offset: color ? 0 : null, scale_span: color ? 1 : null })) })
const clone = () => structuredClone(DEFAULT_DISPLAY)
const base = readView("?mode=specific")

describe("display URL boundary", () => {
  it("default display adds zero parameters and stays optional", () => {
    assert.equal(viewSearch(withDisplay(base, DEFAULT_DISPLAY)), viewSearch(base))
    assert.equal(withDisplay(base, DEFAULT_DISPLAY).display, undefined)
    assert.equal(readView("?mode=specific").display, undefined)
    assert.ok(Object.isFrozen(DEFAULT_DISPLAY.color.r.stretch))
  })
  it("every Color and Composite field survives every mode, including Specific", () => {
    const display = clone()
    for (const [index, key] of ["r", "g", "b"].entries()) display.color[key] = { enabled: false, tint: ["#123abc", "#fedcba", "#876543"][index], gain: index * 0.5,
      blackWhite: { mode: "manual", low: -10 + index, high: 5 + index }, stretch: index === 0 ? { kind: "linear" } : index === 1 ? { kind: "asinh" } : { kind: "gamma", gamma: 2 } }
    display.composite = { gain: 1.5, blackWhite: { mode: "manual", low: -2, high: 4 }, stretch: { kind: "asinh" }, invert: true }
    for (const mode of ["specific", "composite", "slider", "range", "delta"]) {
      assert.deepEqual(displayOf(readView(`?${viewSearch(withDisplay({ ...base, mode }, display))}`)), display)
    }
    assert.deepEqual(displayOf(readView(`?${viewSearch(withDisplay({ ...base, mode: "composite", color: true }, display))}`)), display)
  })
  it("falls back per invalid field without science aliases", () => {
    const view = readView("?mode=color&color_r=80&tint_r=no&gain_r=no&bw_r=3,2&stretch_r=gamma:no&off=r,no&gain_g=1.5&stretch_g=gamma:9&bw_g=-2,3&bright_b=2&color_b_width=7")
    const display = displayOf(view)
    assert.deepEqual(display.color.r, DEFAULT_DISPLAY.color.r)
    assert.equal(display.color.g.gain, 1.5)
    assert.equal(display.color.g.stretch.gamma, 3)
    assert.deepEqual(display.color.g.blackWhite, { mode: "manual", low: -2, high: 3 })
    assert.deepEqual(display.color.b, DEFAULT_DISPLAY.color.b)
    assert.equal(view.colour.colorBands.r.pin, 80)
    assert.equal(view.colour.colorBands.b.width, 7)
    for (const param of ["bw_c=1,", "bw_c=1,1", "gain_c=", "stretch_c=gamma:", "invert_c=yes", "bright_c=2"]) assert.equal(readView(`?${param}`).display, undefined, param)
    const onlyDisplay = viewSearch(withDisplay(base, display))
    assert.equal(onlyDisplay.includes("color_r"), false)
    assert.equal(onlyDisplay.includes("bright"), false)
  })
  it("maps inclusive human ranges to API pins, Apply is the only commit", () => {
    assert.deepEqual(rangeCommit("1-8;44"), { mode: "range", pins: [{ center: 3, width: 8 }, { center: 43, width: 1 }] })
    assert.deepEqual(rangeCommit("44;1-8;4-6"), rangeCommit("1-8;44"))
    assert.deepEqual(rangeCommit(""), { mode: "composite" })
    assert.deepEqual(rangeCommit("all"), { mode: "composite" })
    assert.deepEqual(rangeCommit("1-102"), { mode: "composite" })
    assert.throws(() => rangeCommit("0"))
    assert.throws(() => rangeCommit("8-1"))
  })
})
describe("pure raw display math", () => {
  it("keeps NaN holes and disabled layers as no-data, gain zero as finite black", () => {
    const resolved = resolveDisplay(DEFAULT_DISPLAY, md())
    assert.deepEqual([...mixRawTile([Float32Array.of(NaN, Infinity)], [], resolved, 2)], [...NO_DATA, ...NO_DATA])
    assert.deepEqual([...mixRawTile([Float32Array.of(0.5)], [], { ...resolved, layers: resolved.layers.map((layer) => ({ ...layer, enabled: false })) }, 1)], NO_DATA)
    assert.deepEqual([...mixRawTile([Float32Array.of(0.5)], [], { ...resolved, layers: resolved.layers.map((layer) => ({ ...layer, gain: 0 })) }, 1)], [0, 0, 0, 255])
    const empty = md(); empty.channels[0] = { ...empty.channels[0], finite_count: 0, default_black: null, default_white: null, default_span: null }
    assert.deepEqual([...mixRawTile([Float32Array.of(1)], [], resolveDisplay(DEFAULT_DISPLAY, empty), 1)], NO_DATA)
  })
  it("maps wire 2/1/0 to pure R/G/B and tints/gains without changing scalars", () => {
    const display = clone(); for (const key of ["r", "g", "b"]) display.color[key].stretch = { kind: "linear" }
    const raw = [Float32Array.of(0.25), Float32Array.of(0.5), Float32Array.of(1)]
    assert.deepEqual([...mixRawTile(raw, [], resolveDisplay(display, md(1, 1, true)), 1)], [255, 128, 64, 255])
    display.color.r = { ...display.color.r, tint: "#00ffff", gain: 0.5 }; display.color.g.enabled = false; display.color.b.enabled = false
    assert.deepEqual([...mixRawTile(raw, [], resolveDisplay(display, md(1, 1, true)), 1)], [0, 128, 128, 255])
    assert.equal(raw[2][0], 1)
  })
  it("mixes normalized 16-bit layers locally, keeping no-data separate from finite black", () => {
    const metadata = md(2, 1, true)
    metadata.channels.forEach((channel) => { channel.scale_offset = -2; channel.scale_span = 4; channel.default_black = -2; channel.default_white = 2; channel.default_span = 4 })
    const display = clone(); for (const key of ["r", "g", "b"]) display.color[key].stretch = { kind: "linear" }
    const layers = [Uint16Array.of(16384, 65535), Uint16Array.of(32767, 65535), Uint16Array.of(65534, 65535)]
    assert.deepEqual([...mixRawTile(layers, metadata.channels, resolveDisplay(display, metadata), 2)], [255, 128, 64, 255, ...NO_DATA])
    display.color.r = { ...display.color.r, tint: "#00ffff", gain: 0.5 }; display.color.g.enabled = false; display.color.b.enabled = false
    assert.deepEqual([...mixRawTile(layers, metadata.channels, resolveDisplay(display, metadata), 2)], [0, 128, 128, 255, ...NO_DATA])
    assert.equal(layers[2][0], 65534)
  })
  it("linear/gamma/asinh are monotonic, clamp guns; invert does not recolor no-data", () => {
    for (const setting of [{ kind: "linear" }, { kind: "gamma", gamma: 0.7 }, { kind: "asinh" }]) {
      const display = clone(); display.composite.stretch = setting
      const output = mixRawTile([Float32Array.of(-1, 0, 0.25, 0.5, 1, 2)], [], resolveDisplay(display, md(6)), 6)
      const guns = [0, 4, 8, 12, 16, 20].map((index) => output[index])
      assert.deepEqual([...guns].sort((a, b) => a - b), guns)
      assert.equal(guns[0], 0); assert.equal(guns.at(-1), 255)
    }
    const display = clone(); display.composite.stretch = { kind: "linear" }; display.composite.invert = true
    assert.deepEqual([...mixRawTile([Float32Array.of(0, 1, NaN)], [], resolveDisplay(display, md(3)), 3)], [255, 255, 255, 255, 0, 0, 0, 255, ...NO_DATA])
  })
  it("Auto uses authoritative span even for large constant black points", () => {
    const metadata = md(); metadata.channels[0].default_black = 1e20; metadata.channels[0].default_white = 1e20; metadata.channels[0].default_span = 1
    assert.equal(resolveDisplay(DEFAULT_DISPLAY, metadata).layers[0].span, 1)
    assert.deepEqual([...mixRawTile([Float32Array.of(1e20)], [], resolveDisplay(DEFAULT_DISPLAY, validateRawMetadata(metadata, { id: "fixture", width: 1, height: 1 })), 1)], [0, 0, 0, 255])
  })
})
describe("raw metadata validation", () => {
  it("validates version/dimensions/order/span/no-data and exact estimates without ceiling", () => {
    const expected = { id: "fixture", width: 1, height: 1 }
    assert.equal(validateRawMetadata(md(), expected).channels[0].default_span, 1)
    const { default_span, ...withoutSpan } = md().channels[0]
    const spanless = md(); spanless.channels[0] = withoutSpan as any
    assert.equal(validateRawMetadata(spanless, expected).channels[0].default_span, 1)
    const collapsed = md(); collapsed.channels[0].default_black = 1e20; collapsed.channels[0].default_white = 1e20; delete collapsed.channels[0].default_span
    assert.throws(() => validateRawMetadata(collapsed, expected))
    for (const patch of [{ format_version: 2 }, { job_id: "old" }, { state: "reading" }, { width: 2 }, { tile_size: 128 }, { dtype: "f64" }, { orientation: "flipped" }, { nodata: "zero" }, { channels: [] }]) assert.throws(() => validateRawMetadata({ ...md(), ...patch }, expected))
    for (const patch of [{ default_span: 0 }, { finite_count: 2 }, { finite_count: 0 }, { role: "red" }, { default_black: null }, { bands: [0] }]) {
      const metadata = md(); Object.assign(metadata.channels[0], patch); assert.throws(() => validateRawMetadata(metadata, expected))
    }
    assert.equal(rawBytes(md(5854, 5854, true)), 205615896)
    const color = md(1, 1, true)
    assert.equal(validateRawMetadata(color, { ...expected, channels: 3 }).channels[2].scale_span, 1)
    assert.throws(() => validateRawMetadata({ ...color, dtype: "f32le", format_version: 1, nodata: "nan" }, expected))
    for (const patch of [{ scale_span: 0 }, { scale_offset: null }, { scale_span: Infinity }]) {
      const broken = structuredClone(color); Object.assign(broken.channels[0], patch)
      assert.throws(() => validateRawMetadata(broken, expected))
    }
  })
})

function fakePort() {
  const port = { onmessage: null, onerror: null, messages: [], detached: [], terminated: false,
    postMessage(message, transfer = []) { this.messages.push(structuredClone(message, { transfer })); if (message.type === "tile") this.detached.push(message.buffer.byteLength) },
    terminate() { this.terminated = true }, emit(message) { this.onmessage?.({ data: message }) } }
  return port
}
function fakeCanvas() { const painted = []; return { width: 1024, height: 1, dataset: {}, painted, getContext() { return { putImageData(...args) { painted.push(args) } } } } }
async function until(predicate) { for (let i = 0; i < 200; i++) { if (predicate()) return; await Promise.resolve() } assert.ok(predicate(), "event did not arrive") }
const originalImageData = globalThis.ImageData
before(() => { globalThis.ImageData = class { constructor(data, width, height) { this.data = data; this.width = width; this.height = height } } })
after(() => { if (originalImageData === undefined) delete globalThis.ImageData; else globalThis.ImageData = originalImageData })

describe("loader and completed revision gate", () => {
  it("caps at four fetches, transfers edge bytes, display edits issue zero HTTP, drops old revisions", async () => {
    const metadata = md(1025, 1); const port = fakePort(); const canvas = fakeCanvas(); let requests = 0, active = 0, max = 0; const release = []
    const fetcher = async (url, { signal }) => {
      requests++
      if (url.endsWith("raw-metadata")) return Response.json(metadata)
      active++; max = Math.max(max, active)
      await new Promise((resolve) => release.push(resolve)); active--
      assert.equal(signal.aborted, false)
      const tx = Number(new URL(url, "http://fixture").searchParams.get("x"))
      return new Response(new Uint8Array((tx === 4 ? 1 : 256) * 4))
    }
    const client = new RawClient(canvas, { id: "fixture", width: 1025, height: 1 }, DEFAULT_DISPLAY, () => {}, fetcher, () => port)
    const done = client.prepare()
    await until(() => release.length === 4); assert.equal(max, 4)
    release.splice(0).forEach((resolve) => resolve()); await until(() => release.length === 1); release.splice(0).forEach((resolve) => resolve()); await done
    assert.deepEqual(port.detached, [0, 0, 0, 0, 0])
    assert.equal(canvas.painted.length, 0)
    assert.equal(port.messages.at(-1).type, "load-complete")
    const session = port.messages[0].session
    port.emit({ type: "load-ready", session })
    assert.equal(client.getSnapshot().gate.exportReady, false)
    const first = port.messages.at(-1).revision
    const before = requests; client.setDisplay(clone()); const latest = port.messages.at(-1).revision
    assert.equal(requests, before)
    port.emit({ type: "tile-rgba", session, revision: first, tx: 0, ty: 0, width: 1, height: 1, buffer: new ArrayBuffer(4) })
    port.emit({ type: "render-complete", session, revision: first })
    assert.equal(canvas.painted.length, 0); assert.equal(client.getSnapshot().gate.exportReady, false)
    port.emit({ type: "tile-rgba", session, revision: latest, tx: 4, ty: 0, width: 1, height: 1, buffer: new ArrayBuffer(4) })
    assert.equal(canvas.painted.length, 1); assert.equal(canvas.painted[0][1], 1024); assert.equal(canvas.width, 1024)
    assert.equal(client.getSnapshot().gate.exportReady, false)
    port.emit({ type: "render-complete", session, revision: latest })
    assert.deepEqual(client.getSnapshot().gate, { exportReady: true, remapBusy: false, completedRevision: latest, pendingRevision: null, label: "Local mix ready" })
    client.dispose()
  })
  it("automatically delivers all three compact Color layers even above 2048², with no server-image export", async () => {
    const metadata = md(2049, 2048, true), port = fakePort(), canvas = fakeCanvas(), urls = []
    const client = new RawClient(canvas, { id: "fixture", width: 2049, height: 2048, channels: 3 }, DEFAULT_DISPLAY, () => {}, async (url) => {
      urls.push(url)
      if (url.endsWith("raw-metadata")) return Response.json(metadata)
      const x = Number(new URL(url, "http://fixture").searchParams.get("x"))
      return new Response(new Uint16Array((x === 8 ? 1 : 256) * 256).fill(32767))
    }, () => port)
    assert.equal(client.getSnapshot().gate.exportReady, false)
    await client.prepare()
    assert.equal(client.getSnapshot().state.phase, "loading")
    assert.equal(client.getSnapshot().gate.exportReady, false)
    assert.equal(urls.length, 1 + 9 * 8 * 3)
    assert.ok(urls.slice(1).every((url) => url.endsWith("format=scale16")))
    assert.deepEqual(port.messages.filter((message) => message.type === "tile").slice(0, 3).map((message) => message.channel), [0, 1, 2])
    assert.equal(port.messages.filter((message) => message.type === "tile").at(-1).buffer.byteLength, 512)
    const session = port.messages[0].session
    port.emit({ type: "load-ready", session })
    const requests = urls.length; client.setDisplay(clone())
    assert.equal(urls.length, requests)
    port.emit({ type: "render-complete", session, revision: port.messages.at(-1).revision })
    assert.equal(client.getSnapshot().gate.exportReady, true)
    client.dispose()
  })
  it("requires informed opt-in above 2048²; cancellation and old generations never paint", async () => {
    const metadata = md(5854, 5854); const canvas = fakeCanvas(); const port = fakePort(); let requests = 0, signal
    const client = new RawClient(canvas, { id: "fixture", width: 5854, height: 5854, channels: 1 }, DEFAULT_DISPLAY, () => {}, async (url, options) => {
      requests++; signal = options.signal
      if (url.endsWith("raw-metadata")) return Response.json(metadata)
      return new Promise((resolve, reject) => options.signal.addEventListener("abort", () => reject(new Error("aborted")), { once: true }))
    }, () => port)
    await client.prepare(); assert.equal(requests, 1); assert.equal(client.getSnapshot().state.estimateBytes, 137077264)
    assert.equal(client.getSnapshot().gate.exportReady, true)
    // 23×23 tiles is above 400, so the loader opens 12 fetch lanes.
    client.start(); await until(() => requests === 13); assert.equal(client.getSnapshot().gate.exportReady, false); const session = port.messages[0].session
    client.cancel(); assert.equal(signal.aborted, true); assert.equal(port.terminated, true)
    // Nothing was painted, so the server image is still on screen and downloadable.
    assert.equal(client.getSnapshot().gate.exportReady, true)
    port.emit({ type: "load-ready", session }); port.emit({ type: "render-complete", session, revision: 1 })
    assert.equal(canvas.painted.length, 0); assert.equal(client.getSnapshot().state.phase, "idle")
    client.dispose()
  })
  it("a 404 in the middle of a load disposes the partial store and paints nothing", async () => {
    const canvas = fakeCanvas(), port = fakePort(); let requests = 0
    const client = new RawClient(canvas, { id: "fixture", width: 1025, height: 1 }, DEFAULT_DISPLAY, () => {}, async (url) => {
      requests++
      if (url.endsWith("raw-metadata")) return Response.json(md(1025, 1))
      if (requests <= 5) return new Response(new ArrayBuffer(256 * 4))
      return new Response("gone", { status: 404 })
    }, () => port)
    await client.prepare()
    assert.equal(client.getSnapshot().state.phase, "unavailable")
    assert.match(client.getSnapshot().state.reason, /Raw layers unavailable/)
    assert.equal(client.getSnapshot().gate.exportReady, true)
    assert.equal(canvas.painted.length, 0); assert.equal(port.terminated, true)
    client.dispose()
  })
  it("invalid lengths, HTTP 404/409/410, and bad metadata keep the server image", async () => {
    for (const status of [200, 404, 409, 410]) {
      const canvas = fakeCanvas(), port = fakePort()
      const client = new RawClient(canvas, { id: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, async (url) => url.endsWith("raw-metadata") ? Response.json(md()) : new Response(new Uint8Array(3), { status }), () => port)
      await client.prepare()
      assert.equal(client.getSnapshot().state.phase, "unavailable")
      assert.equal(client.getSnapshot().gate.completedRevision, 0); assert.equal(client.getSnapshot().gate.exportReady, true)
      assert.match(client.getSnapshot().state.reason, /server image is unchanged/)
      assert.equal(port.terminated, true); assert.equal(canvas.painted.length, 0)
      client.dispose()
    }
    const client = new RawClient(fakeCanvas(), { id: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, async () => Response.json({ ...md(), dtype: "bad" }), () => { throw new Error("must not start worker") })
    await client.prepare(); assert.equal(client.getSnapshot().state.phase, "unavailable"); client.dispose()
  })
  it("keeps a completed Composite server image downloadable when optional raw setup fails", async () => {
    const expected = { id: "fixture", width: 1, height: 1 }
    // Constructor baseline: a non-Color client starts export-open; a Color client stays closed.
    const idle = new RawClient(fakeCanvas(), expected, DEFAULT_DISPLAY, () => {}, async () => { throw new Error("must not fetch") }, () => { throw new Error("must not start worker") })
    assert.deepEqual(idle.getSnapshot().gate, { exportReady: true, remapBusy: false, completedRevision: 0, pendingRevision: null, label: "Finishing server image…" })
    idle.dispose()
    const color = new RawClient(fakeCanvas(), { ...expected, channels: 3 }, DEFAULT_DISPLAY, () => {}, async () => { throw new Error("must not fetch") }, () => { throw new Error("must not start worker") })
    assert.equal(color.getSnapshot().gate.exportReady, false)
    color.dispose()

    // Raw metadata missing or invalid is optional: the server image must stay downloadable.
    for (const [name, fetcher] of [
      ["raw metadata 404", async () => new Response("gone", { status: 404 })],
      ["invalid raw metadata", async () => Response.json({ ...md(), dtype: "bad" })],
    ] as const) {
      let worker = false
      const client = new RawClient(fakeCanvas(), expected, DEFAULT_DISPLAY, () => {}, fetcher, () => { worker = true; return fakePort() })
      await client.prepare()
      assert.equal(client.getSnapshot().state.phase, "unavailable", name)
      assert.equal(client.getSnapshot().gate.exportReady, true, name)
      assert.match(client.getSnapshot().state.reason, /server image is unchanged/, name)
      assert.equal(worker, false, `${name} must fail before starting the worker`)
      client.dispose()
    }

    // Browser Worker startup failure happens before the load closes the gate.
    const fetcher = async (url) => url.endsWith("raw-metadata") ? Response.json(md()) : new Response(new ArrayBuffer(4))
    const unsupported = new RawClient(fakeCanvas(), expected, DEFAULT_DISPLAY, () => {}, fetcher, () => { throw new Error("No Worker") })
    await unsupported.prepare()
    assert.equal(unsupported.getSnapshot().state.phase, "unavailable")
    assert.equal(unsupported.getSnapshot().gate.exportReady, true)
    assert.match(unsupported.getSnapshot().state.reason, /server image is unchanged/)
    unsupported.dispose()
  })
  it("worker startup fails safely; interruption mid-remap keeps export closed through retry", async () => {
    const fetcher = async (url) => url.endsWith("raw-metadata") ? Response.json(md()) : new Response(new ArrayBuffer(4))
    const unsupported = new RawClient(fakeCanvas(), { id: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, fetcher, () => { throw new Error("No Worker") })
    await unsupported.prepare(); assert.equal(unsupported.getSnapshot().gate.exportReady, true); unsupported.dispose()
    const port = fakePort(), canvas = fakeCanvas()
    const client = new RawClient(canvas, { id: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, fetcher, () => port)
    await client.prepare(); const session = port.messages[0].session
    port.emit({ type: "load-ready", session }); port.onerror()
    assert.equal(client.getSnapshot().gate.exportReady, false); assert.match(client.getSnapshot().state.reason, /Export waits/)
    client.cancel(); assert.equal(client.getSnapshot().gate.exportReady, false); client.dispose()
  })
})

// raw-worker.ts yields to the event loop after every 12 tiles (YIELD_EVERY).
// One more tile than that puts exactly one tile boundary on each side of a yield.
const WORKER_YIELD_EVERY = 12
const WORKER_TILES = WORKER_YIELD_EVERY + 1

/** The worker as a classic script, as the browser runs it. RAW_WORKER_TEST_FILE
 * points at a prebuilt bundle; otherwise Vite's esbuild bundles the source. */
async function workerSource() {
  if (process.env.RAW_WORKER_TEST_FILE) return readFileSync(process.env.RAW_WORKER_TEST_FILE, "utf8")
  const { build } = await import("esbuild")
  const result = await build({ entryPoints: [new URL("../features/mosaic/raw-worker.ts", import.meta.url).pathname], bundle: true, format: "iife", platform: "browser", write: false, logLevel: "silent" })
  return result.outputFiles[0].text
}

it("the real Worker supersedes and disposes at tile boundaries, transferring only tile patches", async () => {
  const source = await workerSource()
  const sent = [], turns = []
  const scope = { onmessage: null, postMessage(message, transfer = []) { sent.push(structuredClone(message, { transfer })); if (message.type === "tile-rgba") assert.equal(message.buffer.byteLength, 0) } }
  runInNewContext(source, { self: scope, setTimeout(callback) { turns.push(callback) }, Float32Array, Uint16Array, Uint8ClampedArray, Math, Map, Promise })
  const send = (message) => scope.onmessage({ data: message })
  const rendered = (revision) => sent.filter((message) => message.type === "tile-rgba" && message.revision === revision).length
  const completed = (revision) => sent.some((message) => message.type === "render-complete" && message.revision === revision)
  const metadata = md(256 * WORKER_TILES, 1)
  send({ type: "init", session: 1, metadata })
  for (let tx = 0; tx < WORKER_TILES; tx++) send({ type: "tile", session: 1, tileIndex: tx, tx, ty: 0, width: 256, height: 1, channel: 0, buffer: new Float32Array(256).fill(0.5).buffer })
  assert.equal(sent.at(-1).loadedTiles, WORKER_TILES)
  send({ type: "load-complete", session: 1 })
  const display = resolveDisplay(DEFAULT_DISPLAY, metadata)
  send({ type: "render", session: 1, revision: 1, display })
  assert.equal(rendered(1), WORKER_YIELD_EVERY)
  assert.equal(turns.length, 1)
  // A newer revision waits for the current tile boundary, then replaces revision 1.
  send({ type: "render", session: 1, revision: 2, display })
  assert.equal(rendered(2), 0)
  turns.shift()(); await until(() => rendered(2) === WORKER_YIELD_EVERY)
  assert.equal(rendered(1), WORKER_YIELD_EVERY)
  assert.equal(completed(1), false)
  turns.shift()(); await until(() => completed(2))
  assert.equal(rendered(2), WORKER_TILES)
  assert.equal(completed(1), false)
  send({ type: "render", session: 1, revision: 3, display }); send({ type: "dispose", session: 1 }); turns.shift()(); await Promise.resolve()
  assert.equal(completed(3), false)
  const count = sent.length; send({ type: "render", session: 1, revision: 4, display }); assert.equal(sent.length, count)
})
