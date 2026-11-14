import { mixRawTile, type RawSamples } from "./raw-mix"
import type { FromRawWorker, RawMetadata, ToRawWorker } from "./raw-protocol"
type Tile = { tx: number; ty: number; width: number; height: number; channels: RawSamples[] }
/** Minimal worker scope type keeps DOM and worker builds independent. */
const scope = self as unknown as { onmessage: ((event: MessageEvent<ToRawWorker>) => void) | null; postMessage(message: FromRawWorker, transfer?: Transferable[]): void }
let session = -1
let metadata: RawMetadata | null = null
let tiles = new Map<number, Tile>()
let loaded = 0
let ready = false
let latest: Extract<ToRawWorker, { type: "render" }> | null = null
let running = false
const send = (message: FromRawWorker, transfer?: Transferable[]) => scope.postMessage(message, transfer)
/** Yield to the event loop every N tiles so tone edits can interrupt without
 * paying one timer hop per 256×256 tile (≈2k hops at 20° was the bulk of "final compute"). */
const YIELD_EVERY = 12
async function render() {
  if (running) return
  running = true
  try {
    while (latest && ready) {
      const request = latest
      send({ type: "render-started", session: request.session, revision: request.revision })
      let sinceYield = 0
      for (const tile of tiles.values()) {
        if (latest !== request || session !== request.session || !ready) break
        const rgba = mixRawTile(tile.channels, metadata?.channels ?? [], request.display, tile.width * tile.height)
        const buffer = rgba.buffer as ArrayBuffer
        send({ type: "tile-rgba", session: request.session, revision: request.revision, tx: tile.tx, ty: tile.ty, width: tile.width, height: tile.height, buffer }, [buffer])
        sinceYield++
        if (sinceYield >= YIELD_EVERY) {
          sinceYield = 0
          await new Promise<void>((resolve) => setTimeout(resolve, 0))
        }
      }
      if (latest === request && session === request.session && ready) {
        send({ type: "render-complete", session, revision: request.revision })
        latest = null
      }
    }
  } catch (error) {
    latest = null
    send({ type: "error", session, code: "worker", message: error instanceof Error ? error.message : "Local rendering failed." })
  } finally { running = false }
}
scope.onmessage = (event) => {
  const message = event.data
  if (message.type === "init") {
    session = message.session; metadata = message.metadata; tiles = new Map(); loaded = 0; ready = false; latest = null
    return
  }
  if (message.session !== session) return
  if (message.type === "dispose") { tiles.clear(); metadata = null; ready = false; latest = null; return }
  if (message.type === "tile" && metadata && !ready) {
    const tile = tiles.get(message.tileIndex) ?? { tx: message.tx, ty: message.ty, width: message.width, height: message.height, channels: [] }
    tile.channels[message.channel] = metadata.dtype === "u16le" ? new Uint16Array(message.buffer) : new Float32Array(message.buffer)
    tiles.set(message.tileIndex, tile)
    if (metadata.channels.every((channel) => tile.channels[channel.index])) {
      loaded++
      send({ type: "load-progress", session, loadedTiles: loaded, totalTiles: Math.ceil(metadata.width / 256) * Math.ceil(metadata.height / 256) })
    }
    return
  }
  if (message.type === "load-complete" && metadata) {
    if (loaded !== Math.ceil(metadata.width / 256) * Math.ceil(metadata.height / 256)) {
      send({ type: "error", session, code: "bad-tile", message: "Raw layer download is incomplete." }); return
    }
    ready = true; send({ type: "load-ready", session }); return
  }
  if (message.type === "render" && ready) { latest = message; void render() }
}
