// Proves the mosaic's final pixels match /api/v1/render.png for a range view,
// tile for tile. Needs a SPEARv API on SPEARV_API_URL (default
// http://127.0.0.1:8787) and /tmp/spearv-venv/bin/python with PIL and numpy.
//
//   node web/e2e/mosaic-pixels.mjs

import assert from "node:assert/strict"
import { execFileSync } from "node:child_process"
import { mkdir, writeFile } from "node:fs/promises"

const api = process.env.SPEARV_API_URL || "http://127.0.0.1:8787"
const shots = process.env.SPEARV_SCREENSHOT_DIR || "/tmp/spearv-bench"
const dir = `${shots}/pixels`
await mkdir(dir, { recursive: true })

const view = "ra=56.75&dec=24.12&fov=0.3&mode=range&dr=qr3&mask=on&pin=40:1"
const started = await (await fetch(`${api}/api/v1/mosaic?${view}`)).json()
assert.ok(started.id, `no mosaic id: ${JSON.stringify(started)}`)
console.log("mosaic", started.id, `${started.width}x${started.height}`, `${started.cols}x${started.rows}`)

let since = 0
let status
for (let i = 0; i < 300; i++) {
  status = await (await fetch(`${api}/api/v1/mosaic/${started.id}/status?since=${since}`)).json()
  since = status.seq
  if (status.state !== "planning" && status.state !== "reading") break
  await new Promise((resolve) => setTimeout(resolve, 1000))
}
assert.equal(status.state, "done", `mosaic ended ${status.state}: ${status.error}`)
console.log(`done: ${status.exposures_done}/${status.exposures_total} exposures, ${status.reads_failed} failed`)

for (let y = 0; y < started.rows; y++) {
  for (let x = 0; x < started.cols; x++) {
    const tile = await (await fetch(`${api}/api/v1/mosaic/${started.id}/tile?x=${x}&y=${y}&v=1`)).arrayBuffer()
    await writeFile(`${dir}/tile-${x}-${y}.png`, Buffer.from(tile))
  }
}
const render = await (await fetch(`${api}/api/v1/render.png?${view}`)).arrayBuffer()
assert.ok(render.byteLength > 1000, "render.png is empty")
await writeFile(`${dir}/render.png`, Buffer.from(render))

const report = execFileSync(
  "/tmp/spearv-venv/bin/python",
  [
    "-c",
    `import sys
from PIL import Image
import numpy as np
args = sys.argv[1:]
d = args[0]
w, h, t, cols, rows = (int(v) for v in args[1:6])
canvas = np.zeros((h, w, 3), np.uint8)
for y in range(rows):
    for x in range(cols):
        tile = np.asarray(Image.open(f"{d}/tile-{x}-{y}.png").convert("RGB"))
        th, tw = tile.shape[:2]
        canvas[y * t:y * t + th, x * t:x * t + tw] = tile
render = np.asarray(Image.open(f"{d}/render.png").convert("RGB"))
assert render.shape == canvas.shape, f"{render.shape} != {canvas.shape}"
diff = np.abs(canvas.astype(int) - render.astype(int))
print(f"max channel diff: {diff.max()}, mean: {diff.mean():.4f}, differing pixels: {(diff.sum(axis=2) > 0).sum()}")
assert diff.max() == 0, "mosaic pixels differ from render.png"
print("mosaic pixels match render.png exactly")`,
    dir,
    String(started.width),
    String(started.height),
    String(started.tile),
    String(started.cols),
    String(started.rows),
  ],
  { encoding: "utf8" },
)
console.log(report.trim())
