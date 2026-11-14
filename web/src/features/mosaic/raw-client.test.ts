// @ts-nocheck
import { it } from "node:test"
import assert from "node:assert/strict"
import { DEFAULT_DISPLAY } from "../../url-state/display.ts"
import { RawClient } from "./raw-client.ts"

it("loads raw metadata and tiles with the global fetch receiver", async () => {
  const originalFetch = globalThis.fetch
  const receivers = []
  const urls = []
  const messages = []
  const metadata = {
    format_version: 1, job_id: "fixture", state: "done", width: 1, height: 1, tile_size: 256,
    dtype: "f32le", orientation: "png-top-row-first", nodata: "nan",
    source_recipe: { mode: "composite", mask: true, pins: [] },
    channels: [{ index: 0, role: "grey", center_band: null, width: 102, bands: [1, 2, 3],
      wavelength_min_um: 1, wavelength_max_um: 2, finite_count: 1, default_black: 0, default_white: 1, default_span: 1 }],
  }
  const worker = { onmessage: null, onerror: null, postMessage(message) { messages.push(message) }, terminate() {} }
  const canvas = { dataset: {} }
  try {
    globalThis.fetch = function (input) {
      receivers.push(this)
      const url = String(input)
      urls.push(url)
      if (url.endsWith("raw-metadata")) return Promise.resolve(Response.json(metadata))
      return Promise.resolve(new Response(new ArrayBuffer(4)))
    }
    const client = new RawClient(canvas, { id: "fixture", width: 1, height: 1 }, DEFAULT_DISPLAY, () => {}, undefined, () => worker)
    try {
      await client.prepare()
      assert.ok(receivers.every((receiver) => receiver === globalThis), "global fetch must receive globalThis as its receiver")
      assert.deepEqual(urls, [
        "/api/v1/mosaic/fixture/raw-metadata",
        "/api/v1/mosaic/fixture/tile?x=0&y=0&channel=0&format=f32",
      ])
      assert.deepEqual(messages.map((message) => message.type), ["init", "tile", "load-complete"])
      assert.equal(client.getSnapshot().state.phase, "loading")
    } finally {
      client.dispose()
    }
  } finally {
    globalThis.fetch = originalFetch
  }
})
