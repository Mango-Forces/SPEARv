// @ts-nocheck
import { beforeEach, test } from "node:test"
import assert from "node:assert/strict"
import { DEFAULT_DISPLAY } from "../../url-state/display.ts"
import { clearCachedPngs, ServerDisplayClient } from "./server-display-client.ts"
import { isColorImage } from "./color-export-mode.ts"

const PNG = png(1, 1)
const TIMINGS = { previewDebounceMs: 5, fullDebounceMs: 15, pollMs: 5 }

beforeEach(() => clearCachedPngs())

test("Color export never replaces Specific canvas export", () => {
  assert.equal(isColorImage({ color: true, mode: "specific" }), false)
  assert.equal(isColorImage({ color: true, mode: "composite" }), true)
  assert.equal(isColorImage({ color: true, mode: "color" }), true)
  assert.equal(isColorImage({ color: false, mode: "composite" }), false)
})

test("coalesces edits and opens export only for a verified current PNG Blob", async () => {
  const requests = []
  const server = fixture(requests)
  let resolveReady
  const ready = new Promise((resolve) => { resolveReady = resolve })
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => {
    snapshots.push(snapshot)
    if (snapshot.fullBlob) resolveReady(snapshot)
  }, server, TIMINGS)
  try {
    client.prepare()
    const edited = structuredClone(DEFAULT_DISPLAY)
    edited.color.r.gain = 1.5
    client.setDisplay(edited)
    const final = structuredClone(edited)
    final.color.g.tint = "#00ffaa"
    client.setDisplay(final)
    const snapshot = await ready
    assert.equal(snapshot.phase, "ready")
    assert.equal(snapshot.gate.exportReady, true)
    assert.equal(snapshot.fullBlob.type, "image/png")
    assert.deepEqual([...new Uint8Array(await snapshot.fullBlob.arrayBuffer())], [...PNG])
    assert.equal(requests.filter((request) => request.path.endsWith("/display-revisions") && request.method === "POST").length, 1)
    const body = requests.find((request) => request.path.endsWith("/display-revisions") && request.method === "POST").body
    assert.equal(body.r.gain, 1.5)
    assert.equal(body.g.tint, "#00ffaa")
    assert.deepEqual(body.r.black_white, { mode: "auto" })
    assert.equal("blackWhite" in body.r, false)
    assert.equal(requests.filter((request) => request.path.endsWith("/full.png")).length, 1)
    assert.equal(requests.filter((request) => request.path.endsWith("/display-revisions/d1/full") && request.method === "POST").length, 1)
    assert.equal(requests.some((request) => request.path.includes("scale16") || request.path.endsWith("/raw-metadata")), false)
    assert.ok(snapshots.some((item) => !item.gate.exportReady))
  } finally {
    client.dispose()
  }
})

test("ignores a late revision response after a newer display generation", async () => {
  let finishFirst
  let postCount = 0
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), async (input, init) => {
    const path = new URL(String(input), "http://fixture").pathname
    if (path.endsWith("/display-revisions") && init.method === "POST") {
      postCount++
      const display = JSON.parse(init.body)
      if (postCount === 1) return new Promise((resolve) => { finishFirst = () => resolve(Response.json(manifest(display, false, { revision: "d1" }))) })
      return Response.json(manifest(display, false, { revision: "d2" }), { status: 201 })
    }
    if (path.endsWith("/status")) return new Promise(() => {})
    throw new Error(`Unexpected request ${path}`)
  }, { previewDebounceMs: 1, fullDebounceMs: 10_000, pollMs: 10_000 })
  try {
    client.prepare()
    await until(() => finishFirst)
    const next = structuredClone(DEFAULT_DISPLAY)
    next.color.r.gain = 1.25
    client.setDisplay(next)
    await until(() => client.getSnapshot().settingsKey.includes("1.25") && client.getSnapshot().revision === "d2")
    finishFirst()
    await new Promise((resolve) => setTimeout(resolve, 1))
    assert.equal(client.getSnapshot().revision, "d2")
    assert.equal(client.getSnapshot().settingsKey.includes("1.25"), true)
  } finally {
    client.dispose()
  }
})

test("keeps the verified Blob through transient status loss", async () => {
  const options = { offline: false }
  let resolveReady
  const ready = new Promise((resolve) => { resolveReady = resolve })
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => {
    snapshots.push(snapshot)
    if (snapshot.fullBlob && !snapshot.error) resolveReady(snapshot)
  }, fixture([], options), TIMINGS)
  try {
    client.prepare()
    const downloaded = await ready
    options.offline = true
    // Once the PNG is cached, later status failures never reopen the gate or drop it.
    await new Promise((resolve) => setTimeout(resolve, 30))
    const after = client.getSnapshot()
    assert.equal(after.phase, "ready")
    assert.equal(after.fullBlob, downloaded.fullBlob)
    assert.equal(after.gate.exportReady, true)
    assert.equal(after.error, undefined)
  } finally {
    client.dispose()
  }
})

test("a no-longer-current server revision does not invalidate the verified local Blob", async () => {
  const options = { current: true }
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), fixture([], options), TIMINGS)
  try {
    client.prepare()
    const ready = await waitFor(snapshots, (snapshot) => !!snapshot.fullBlob && !snapshot.error)
    options.current = false
    await new Promise((resolve) => setTimeout(resolve, 30))
    const after = client.getSnapshot()
    assert.equal(after.phase, "ready")
    assert.equal(after.fullBlob, ready.fullBlob)
    assert.equal(after.gate.exportReady, true)
    assert.equal(after.error, undefined)
  } finally {
    client.dispose()
  }
})

test("requestFull failure and retry keep a verified Blob and open gate", async () => {
  const options = { fullRequestError: false, holdStatus: false }
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), fixture([], options), TIMINGS)
  try {
    client.prepare()
    const ready = await waitFor(snapshots, (snapshot) => !!snapshot.fullBlob && !snapshot.error)
    options.holdStatus = true
    options.fullRequestError = true
    client.retry()
    const retrying = client.getSnapshot()
    assert.equal(retrying.fullBlob, ready.fullBlob)
    assert.equal(retrying.gate.exportReady, true)
    assert.equal(retrying.phase, "ready")
    assert.equal(retrying.error, undefined)
  } finally {
    client.dispose()
  }
})

test("evicted revision recovers by re-creating instead of dead-ending", async () => {
  let posts = 0
  let display
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), async (input, init = {}) => {
    const path = new URL(String(input), "http://fixture").pathname
    const method = init.method ?? "GET"
    if (path.endsWith("/display-revisions") && method === "POST") {
      posts++
      display = JSON.parse(init.body)
      return Response.json(manifest(display, true, { revision: `d${posts}` }), { status: 201 })
    }
    if (path.includes("/display-revisions/d") && path.endsWith("/status")) {
      if (posts === 1) return errorResponse(410, "gone")
      return Response.json(manifest(display, true, { revision: `d${posts}` }))
    }
    if (path.includes("/display-revisions/d") && path.endsWith("/full") && method === "POST") {
      return Response.json(manifest(display, true, { revision: `d${posts}` }), { status: 202 })
    }
    if (path.endsWith("/preview.png") || path.endsWith("/full.png")) return pngResponse(PNG)
    throw new Error(`Unexpected ${method} ${path}`)
  }, TIMINGS)
  try {
    client.prepare()
    const ready = await waitFor(snapshots, (snapshot) => !!snapshot.fullBlob && snapshot.phase === "ready")
    assert.equal(ready.gate.exportReady, true)
    assert.ok(posts >= 2)
  } finally {
    client.dispose()
  }
})

test("cached PNG survives a brand-new client for the same job and settings", async () => {
  const requests = []
  const first = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, fixture(requests), TIMINGS)
  try {
    first.prepare()
    await until(() => !!first.getSnapshot().fullBlob)
  } finally {
    first.dispose()
  }
  const before = requests.length
  const second = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, fixture(requests), TIMINGS)
  try {
    second.prepare()
    assert.equal(!!second.getSnapshot().fullBlob, true)
    assert.equal(second.getSnapshot().gate.exportReady, true)
    assert.equal(requests.length, before)
  } finally {
    second.dispose()
  }
})

test("a local settings change closes the gate and releases the cached Blob immediately", async () => {
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), fixture([]), TIMINGS)
  try {
    client.prepare()
    await waitFor(snapshots, (snapshot) => !!snapshot.fullBlob)
    const next = structuredClone(DEFAULT_DISPLAY)
    next.color.r.gain = 1.25
    client.setDisplay(next)
    const changed = client.getSnapshot()
    assert.equal(changed.fullBlob, undefined)
    assert.equal(changed.fullUrl, undefined)
    assert.equal(changed.previewUrl, undefined)
    assert.equal(changed.gate.exportReady, false)
    assert.equal(changed.settingsKey.includes("1.25"), true)
  } finally {
    client.dispose()
  }
})

test("a late full response cannot restore the gate after a settings change", async () => {
  let releaseFull
  const options = { holdFull: true }
  const server = fixture([], options, () => new Promise((resolve) => { releaseFull = () => resolve(pngResponse(PNG)) }))
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), server, { previewDebounceMs: 5, fullDebounceMs: 10, pollMs: 5 })
  try {
    client.prepare()
    await until(() => releaseFull)
    const next = structuredClone(DEFAULT_DISPLAY)
    next.color.r.gain = 1.25
    client.setDisplay(next)
    releaseFull()
    await new Promise((resolve) => setTimeout(resolve, 1))
    const current = client.getSnapshot()
    assert.equal(current.fullBlob, undefined)
    assert.equal(current.gate.exportReady, false)
    assert.equal(current.settingsKey.includes("1.25"), true)
  } finally {
    client.dispose()
  }
})

test("rejects a full PNG whose first chunk is not a valid RGB8 IHDR", async () => {
  const malformed = png(1, 1)
  malformed[12] = 0
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), fixture([], { fullPng: malformed }), TIMINGS)
  try {
    client.prepare()
    const failed = await waitFor(snapshots, (snapshot) => snapshot.phase === "failed")
    assert.equal(failed.gate.exportReady, false)
    assert.match(failed.error, /IHDR/)
  } finally {
    client.dispose()
  }
})

test("rejects a manifest whose echoed settings differ from the request", async () => {
  const requests = []
  const options = {
    echoDisplay: (display) => {
      const echoed = structuredClone(display)
      echoed.r.gain = 0.5
      return echoed
    },
  }
  const snapshots = []
  const client = new ServerDisplayClient({ jobId: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, (snapshot) => snapshots.push(snapshot), fixture(requests, options), TIMINGS)
  try {
    client.prepare()
    const failed = await waitFor(snapshots, (snapshot) => snapshot.phase === "failed")
    assert.equal(failed.gate.exportReady, false)
    assert.match(failed.error, /do not match this revision/)
    assert.equal(requests.some((request) => request.path.endsWith("/full.png")), false)
  } finally {
    client.dispose()
  }
})

function fixture(requests, options = {}, holdFullFactory) {
  let display
  let fullQueued = false
  return async (input, init = {}) => {
    const path = new URL(String(input), "http://fixture").pathname
    const request = { path, method: init.method ?? "GET", body: init.body ? JSON.parse(init.body) : undefined }
    requests.push(request)
    if (path.endsWith("/display-revisions") && request.method === "POST") {
      display = request.body
      return Response.json(manifest(display, false, options), { status: 201 })
    }
    if (path.endsWith("/display-revisions/d1/full") && request.method === "POST") {
      if (options.fullRequestError) return errorResponse(500, "full request failed")
      fullQueued = true
      return Response.json(manifest(display, true, options), { status: 202 })
    }
    if (path.endsWith("/display-revisions/d1/status")) {
      if (options.holdStatus) return new Promise(() => {})
      if (options.offline) return errorResponse(503, "offline")
      if (options.status) {
        const response = await options.status()
        if (response) return response
      }
      return Response.json(manifest(display, fullQueued, options))
    }
    if (path.endsWith("/display-revisions/d1/preview.png")) return pngResponse(options.previewPng ?? PNG)
    if (path.endsWith("/display-revisions/d1/full.png")) {
      if (holdFullFactory && options.holdFull) return holdFullFactory()
      return pngResponse(options.fullPng ?? PNG)
    }
    throw new Error(`Unexpected request ${path}`)
  }
}

function manifest(display, fullQueued, options = {}) {
  return {
    job_id: "fixture", revision: options.revision ?? "d1", current: options.current ?? true, width: 1, height: 1,
    display: options.echoDisplay ? options.echoDisplay(display) : display,
    channels: ["blue", "green", "red"].map((role, index) => ({
      index, role, center_band: 10 + index, width: 1, bands: [10 + index], wavelength_min_um: 1, wavelength_max_um: 2,
      finite_count: 1, default_black: 0, default_white: 1, default_span: 1,
    })),
    preview: artifact("ready", PNG.byteLength),
    full: artifact(fullQueued ? "ready" : "not_requested", fullQueued ? PNG.byteLength : null),
  }
}

function artifact(state, bytes) {
  return {
    state, width: 1, height: 1, url: "/api/v1/mosaic/fixture/display-revisions/d1/preview.png",
    bytes, error: null, completed_rows: state === "ready" ? 1 : 0, total_rows: 1,
  }
}

function png(width, height) {
  const bytes = new Uint8Array(57)
  bytes.set([137, 80, 78, 71, 13, 10, 26, 10], 0)
  const view = new DataView(bytes.buffer)
  view.setUint32(8, 13, false)
  bytes.set([73, 72, 68, 82], 12)
  view.setUint32(16, width, false)
  view.setUint32(20, height, false)
  bytes[24] = 8
  bytes[25] = 2
  bytes.set([73, 68, 65, 84], 37)
  bytes.set([73, 69, 78, 68], 49)
  bytes.set([174, 66, 96, 130], 53)
  return bytes
}

function pngResponse(bytes) {
  return new Response(bytes, { headers: { "content-type": "image/png", "content-length": String(bytes.byteLength) } })
}

function errorResponse(status, message) {
  return new Response(JSON.stringify({ error: message }), { status, headers: { "content-type": "application/json" } })
}

async function waitFor(snapshots, predicate) {
  const found = snapshots.find(predicate)
  if (found) return found
  return new Promise((resolve, reject) => {
    const expires = Date.now() + 2000
    const timer = setInterval(() => {
      const snapshot = snapshots.find(predicate)
      if (snapshot) {
        clearInterval(timer)
        resolve(snapshot)
      } else if (Date.now() >= expires) {
        clearInterval(timer)
        reject(new Error("event did not arrive"))
      }
    }, 1)
  })
}

async function until(predicate) {
  const expires = Date.now() + 1000
  while (Date.now() < expires) {
    if (predicate()) return
    await new Promise((resolve) => setTimeout(resolve, 1))
  }
  assert.ok(predicate(), "event did not arrive")
}
