import { resolveDisplay, type DisplayState } from "../../url-state/display.ts"
import { AUTO_RAW_PIXELS, rawBytes, validateRawMetadata, type FromRawWorker, type RawErrorCode, type RawMetadata, type RawSessionState, type RenderGate, type ToRawWorker } from "./raw-protocol.ts"
export type RawSnapshot = { state: RawSessionState; metadata: RawMetadata | null; gate: RenderGate }
export const serverGate = (final: boolean): RenderGate => ({ exportReady: final, remapBusy: false, completedRevision: 0, pendingRevision: null, label: final ? "Server image" : "Finishing server image…" })
type WorkerPort = { postMessage(message: ToRawWorker, transfer?: Transferable[]): void; terminate(): void;
  onmessage: ((event: MessageEvent<FromRawWorker>) => void) | null; onerror: ((event: ErrorEvent) => void) | null }
class RawFailure extends Error { constructor(readonly code: RawErrorCode, message: string) { super(message) } }
let nextSession = 0

/** Composite raw layers are mixed in the CPU worker; Color uses server display revisions. */
export class RawClient {
  private ac = new AbortController()
  private worker: WorkerPort | null = null
  private session = ++nextSession
  private revision = 0
  private display: DisplayState
  private dead = false
  private snapshot: RawSnapshot = { state: { phase: "idle" }, metadata: null, gate: serverGate(false) }
  constructor(private canvas: HTMLCanvasElement, private expected: { id: string; width: number; height: number; channels?: number },
    display: DisplayState, private changed: (snapshot: RawSnapshot) => void,
    private fetcher: typeof fetch = (input, init) => globalThis.fetch(input, init),
    private createWorker: () => WorkerPort = () => new Worker(new URL("./raw-worker.ts", import.meta.url), { type: "module" })) {
    this.display = display
    // A completed Composite server image is downloadable until local layers
    // actually replace it. Color never constructs RawClient (server display
    // revisions own it); `channels === 3` covers direct three-channel callers.
    const local = this.expected.channels === 3
    this.snapshot = { ...this.snapshot, gate: { ...serverGate(!local), label: local ? "Waiting for the three color layers…" : "Finishing server image…" } }
  }
  getSnapshot(): RawSnapshot { return this.snapshot }
  private publish(patch: Partial<RawSnapshot>) {
    if (this.dead) return
    this.snapshot = { ...this.snapshot, ...patch }
    this.canvas.dataset.completedRevision = String(this.snapshot.gate.completedRevision)
    this.canvas.dataset.remapBusy = String(this.snapshot.gate.remapBusy)
    this.changed(this.snapshot)
  }
  private stop() {
    this.ac.abort()
    this.worker?.postMessage({ type: "dispose", session: this.session })
    this.worker?.terminate()
    this.worker = null
    this.session = ++nextSession
  }
  dispose() { this.dead = true; this.stop() }
  cancel() {
    if (this.snapshot.state.phase === "ready") return
    this.stop()
    this.publish({ state: { phase: "idle" }, ...this.serverImageGate() })
  }
  retry() { this.stop(); void this.prepare() }
  async prepare() {
    if (this.dead) return
    this.ac = new AbortController()
    const session = this.session
    this.publish({ state: { phase: "loading", loadedTiles: 0, totalTiles: 0 } })
    try {
      const response = await this.response(`/api/v1/mosaic/${encodeURIComponent(this.expected.id)}/raw-metadata`, this.ac.signal)
      let metadata: RawMetadata
      try { metadata = validateRawMetadata(await response.json(), this.expected) }
      catch { throw new RawFailure("bad-metadata", "Raw metadata does not match this completed mosaic.") }
      if (this.dead || this.ac.signal.aborted || session !== this.session) return
      this.publish({ metadata, ...(metadata.source_recipe.mode === "color"
        ? { gate: { ...serverGate(false), label: "Loading three color layers — export waits for them" } }
        : {}) })
      if (metadata.source_recipe.mode !== "color" && metadata.width * metadata.height > AUTO_RAW_PIXELS) {
        this.publish({ state: { phase: "awaiting-opt-in", estimateBytes: rawBytes(metadata) } }); return
      }
      await this.load(metadata, session)
    } catch (error) { this.fail(error, session) }
  }
  start() {
    if (this.dead) return
    const metadata = this.snapshot.metadata
    if (metadata && (this.snapshot.state.phase === "awaiting-opt-in" || this.snapshot.state.phase === "idle")) {
      this.ac = new AbortController()
      const session = this.session
      void this.load(metadata, session).catch((error: unknown) => this.fail(error, session))
      return
    }
    if (this.snapshot.state.phase === "unavailable" || this.snapshot.state.phase === "idle") this.retry()
  }
  private async response(url: string, signal: AbortSignal): Promise<Response> {
    const response = await this.fetcher(url, { signal, cache: "no-store" })
    if (!response.ok) {
      const code: RawErrorCode = response.status === 404 ? "http-404" : response.status === 410 ? "http-410" : response.status === 409 ? "http-409" : "worker"
      throw new RawFailure(code, `Raw layer request failed (${response.status}).`)
    }
    return response
  }
  private async load(metadata: RawMetadata, session: number) {
    const signal = this.ac.signal
    const cols = Math.ceil(metadata.width / 256), rows = Math.ceil(metadata.height / 256)
    const count = cols * rows * metadata.channels.length
    const worker = this.createWorker()
    this.worker = worker
    worker.onmessage = (event) => this.receive(event.data)
    worker.onerror = () => this.fail(new RawFailure("worker", "Local editing is unavailable in this browser."), session)
    worker.postMessage({ type: "init", session, metadata })
    this.publish({ state: { phase: "loading", loadedTiles: 0, totalTiles: cols * rows },
      gate: { ...this.snapshot.gate, exportReady: false, label: "Loading raw layers — export waits" } })
    let cursor = 0
    const fetchNext = async () => {
      while (cursor < count && !signal.aborted && !this.dead && session === this.session) {
        const index = cursor++
        const tileIndex = Math.floor(index / metadata.channels.length)
        const channel = index % metadata.channels.length, tx = tileIndex % cols, ty = Math.floor(tileIndex / cols)
        const width = Math.min(256, metadata.width - tx * 256), height = Math.min(256, metadata.height - ty * 256)
        const format = metadata.dtype === "u16le" ? "scale16" : "f32"
        const bytes = metadata.dtype === "u16le" ? 2 : 4
        const response = await this.response(`/api/v1/mosaic/${encodeURIComponent(metadata.job_id)}/tile?x=${tx}&y=${ty}&channel=${channel}&format=${format}`, signal)
        const buffer = await response.arrayBuffer()
        if (signal.aborted || this.dead || session !== this.session) return
        if (buffer.byteLength !== width * height * bytes) throw new RawFailure("bad-tile", "A raw tile has the wrong byte length.")
        this.worker?.postMessage({ type: "tile", session, channel, tileIndex, tx, ty, width, height, buffer }, [buffer])
      }
    }
    const tileCount = cols * rows
    const width = tileCount > 400 ? 12 : tileCount > 100 ? 8 : 4
    await Promise.all(Array.from({ length: Math.min(width, count) }, fetchNext))
    if (signal.aborted || this.dead || session !== this.session) return
    this.worker?.postMessage({ type: "load-complete", session })
  }
  setDisplay(display: DisplayState) {
    this.display = display
    if (this.snapshot.state.phase === "ready") this.render()
  }
  private render() {
    if (!this.worker || !this.snapshot.metadata) return
    const revision = ++this.revision
    this.publish({ gate: { ...this.snapshot.gate, exportReady: false, remapBusy: true, pendingRevision: revision,
      label: "Updating image — export is available when the update finishes" } })
    this.worker.postMessage({ type: "render", session: this.session, revision, display: resolveDisplay(this.display, this.snapshot.metadata) })
  }
  private receive(message: FromRawWorker) {
    if (this.dead || message.session !== this.session) return
    if (message.type === "load-progress") {
      const total = message.totalTiles
      const loaded = message.loadedTiles
      const pct = total > 0 ? Math.round((loaded / total) * 100) : 0
      this.publish({
        state: { phase: "loading", loadedTiles: loaded, totalTiles: total },
        gate: {
          ...this.snapshot.gate,
          exportReady: false,
          label: total > 0
            ? `Loading raw layers ${loaded}/${total} tiles (${pct}%) — export waits`
            : "Loading raw layers — export waits",
        },
      })
      return
    }
    if (message.type === "load-ready") { this.publish({ state: { phase: "ready" } }); this.render(); return }
    if (message.type === "error") { this.fail(new RawFailure(message.code, message.message), message.session); return }
    if (message.revision !== this.snapshot.gate.pendingRevision) return
    if (message.type === "tile-rgba") {
      const context = this.canvas.getContext("2d")
      if (!context || message.buffer.byteLength !== message.width * message.height * 4) {
        this.fail(new RawFailure("bad-tile", "A raw render tile has the wrong byte length."), message.session); return
      }
      context.putImageData(new ImageData(new Uint8ClampedArray(message.buffer), message.width, message.height), message.tx * 256, message.ty * 256)
      return
    }
    if (message.type === "render-complete") this.publish({ gate: { exportReady: true, remapBusy: false, completedRevision: message.revision, pendingRevision: null, label: "Local mix ready" } })
  }
  private fail(error: unknown, session: number) {
    if (this.dead || session !== this.session || this.ac.signal.aborted) return
    const interrupted = this.snapshot.gate.remapBusy
    this.stop()
    const detail = error instanceof RawFailure && error.code === "http-404" ? "Raw layers unavailable — the job is no longer on the server."
      : error instanceof RawFailure && (error.code === "http-409" || error.code === "http-410") ? "Raw layers are not available for this mosaic."
      : error instanceof Error ? error.message : "Local editing is unavailable in this browser."
    this.publish({ state: { phase: "unavailable", reason: interrupted
      ? `${detail} Export waits for a complete retry.`
      : `${detail} The server image is unchanged.` },
      ...(interrupted ? { gate: { ...this.snapshot.gate, exportReady: false, remapBusy: true, label: "Local layers unavailable — export waits for a complete retry" } } : this.serverImageGate()) })
  }
  /** Loading closes export, but nothing reaches the canvas before the first
   * render. A stopped load that never painted leaves the completed Composite
   * server image on screen, so export reopens for it. Color and any painted
   * or interrupted local mix keep their gate. */
  private serverImageGate(): Partial<RawSnapshot> {
    const gate = this.snapshot.gate
    const local = this.expected.channels === 3 || this.snapshot.metadata?.source_recipe.mode === "color"
    if (local || gate.remapBusy || gate.completedRevision !== 0) return {}
    return { gate: serverGate(true) }
  }
}
