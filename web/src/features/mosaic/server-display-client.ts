import { normalizeDisplay, type DisplayState } from "../../url-state/display.ts"
import {
  colorDisplayKey,
  sameServerColorDisplay,
  serverColorDisplay,
  validateDisplayManifest,
  type ColorDisplayPhase,
  type DisplayManifest,
} from "./server-display-protocol.ts"
import type { RenderGate } from "./raw-protocol.ts"

export type ColorDisplaySnapshot = {
  phase: ColorDisplayPhase
  settingsKey: string
  jobId: string
  revision?: string
  manifest?: DisplayManifest
  previewUrl?: string
  fullUrl?: string
  fullBlob?: Blob
  completedRows: number
  totalRows: number
  error?: string
  gate: RenderGate
}

export type DisplayFetcher = (input: RequestInfo | URL, init?: RequestInit) => Promise<Response>
type Timings = { previewDebounceMs?: number; fullDebounceMs?: number; pollMs?: number }

const PREVIEW_DEBOUNCE_MS = 200
const FULL_DEBOUNCE_MS = 1000
const POLL_MS = 500
const CLOSED_GATE: RenderGate = {
  exportReady: false,
  remapBusy: false,
  completedRevision: 0,
  pendingRevision: null,
  label: "Preparing the server PNG…",
}

/** Owns one job's display generations and the exact server PNG Blob for export. */
export class ServerDisplayClient {
  private ac = new AbortController()
  private generation = 0
  private revision?: string
  private display: DisplayState
  private dead = false
  private fullDue = false
  private fullRequested = false
  private fullFetchStarted = false
  private previewFetchStarted = false
  private previewTimer?: ReturnType<typeof setTimeout>
  private fullTimer?: ReturnType<typeof setTimeout>
  private pollTimer?: ReturnType<typeof setTimeout>
  private previewUrl?: string
  private fullUrl?: string
  private snapshot: ColorDisplaySnapshot
  private timings: Required<Timings>

  constructor(
    private expected: { jobId: string; width: number; height: number },
    display: DisplayState,
    private changed: (snapshot: ColorDisplaySnapshot) => void,
    private fetcher: DisplayFetcher = (input, init) => globalThis.fetch(input, init),
    timings: Timings = {},
  ) {
    this.display = normalizeDisplay(display)
    this.timings = {
      previewDebounceMs: timings.previewDebounceMs ?? PREVIEW_DEBOUNCE_MS,
      fullDebounceMs: timings.fullDebounceMs ?? FULL_DEBOUNCE_MS,
      pollMs: timings.pollMs ?? POLL_MS,
    }
    this.snapshot = {
      phase: "creating",
      settingsKey: colorDisplayKey(this.display),
      jobId: expected.jobId,
      completedRows: 0,
      totalRows: 0,
      gate: CLOSED_GATE,
    }
  }

  getSnapshot(): ColorDisplaySnapshot {
    return this.snapshot
  }

  prepare() {
    if (this.dead) return
    this.schedule(this.display, true)
  }

  setDisplay(display: DisplayState) {
    const normalized = normalizeDisplay(display)
    if (colorDisplayKey(normalized) === this.snapshot.settingsKey) return
    this.display = normalized
    this.schedule(normalized)
  }

  retry() {
    if (this.dead) return
    if (this.revision && this.snapshot.phase !== "expired") {
      const cached = !!this.snapshot.fullBlob
      this.fullRequested = false
      this.fullFetchStarted = cached
      this.previewFetchStarted = !!this.snapshot.previewUrl
      this.publish({
        ...this.snapshot,
        phase: cached ? "ready" : "rendering",
        error: undefined,
        gate: cached
          ? { ...this.snapshot.gate, exportReady: true, label: "Cached current server PNG remains available" }
          : { ...CLOSED_GATE, label: "Retrying the current server PNG…" },
      })
      void this.requestFull(this.generation, this.revision)
      void this.poll(this.generation, this.revision)
      return
    }
    this.schedule(this.display, true)
  }

  dispose() {
    this.dead = true
    this.generation++
    this.ac.abort()
    this.clearTimers()
    this.releaseUrls()
  }

  private schedule(display: DisplayState, force = false) {
    const key = colorDisplayKey(display)
    if (!force && key === this.snapshot.settingsKey) return
    this.generation++
    const generation = this.generation
    this.ac.abort()
    this.ac = new AbortController()
    this.clearTimers()
    this.releaseUrls()
    this.display = display
    this.revision = undefined
    this.fullDue = false
    this.fullRequested = false
    this.fullFetchStarted = false
    this.previewFetchStarted = false
    this.publish({
      phase: "creating",
      settingsKey: key,
      jobId: this.expected.jobId,
      completedRows: 0,
      totalRows: 0,
      error: undefined,
      manifest: undefined,
      previewUrl: undefined,
      fullUrl: undefined,
      fullBlob: undefined,
      gate: { ...CLOSED_GATE, label: "Sending display settings to the server…" },
    })
    this.previewTimer = setTimeout(() => void this.createRevision(generation), this.timings.previewDebounceMs)
    this.fullTimer = setTimeout(() => {
      if (!this.isCurrent(generation)) return
      this.fullDue = true
      if (this.revision) void this.requestFull(generation, this.revision)
    }, this.timings.fullDebounceMs)
  }

  private async createRevision(generation: number) {
    if (!this.isCurrent(generation)) return
    try {
      const response = await this.fetcher(this.endpoint("display-revisions"), {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify(serverColorDisplay(this.display)),
        cache: "no-store",
        signal: this.ac.signal,
      })
      if (!this.isCurrent(generation)) return
      if (response.status !== 200 && response.status !== 201) {
        if (response.status === 404 || response.status === 410) return this.expire(generation, "This mosaic is no longer available. Retry after it reconnects.")
        throw new Error(await responseError(response, "The server could not create this display revision."))
      }
      const value: unknown = await response.json()
      if (!isRecord(value) || typeof value.revision !== "string" || value.revision.length === 0) {
        throw new Error("The server returned an invalid display revision.")
      }
      if (!this.isCurrent(generation)) return
      this.revision = value.revision
      this.publish({
        ...this.snapshot,
        revision: value.revision,
        phase: "rendering",
        gate: { ...CLOSED_GATE, label: "Rendering the server preview and PNG…" },
      })
      if (this.fullDue) void this.requestFull(generation, value.revision)
      void this.poll(generation, value.revision)
    } catch (error) {
      this.fail(generation, error)
    }
  }

  private async requestFull(generation: number, revision: string) {
    if (!this.isCurrent(generation, revision) || this.fullRequested) return
    this.fullRequested = true
    try {
      const response = await this.fetcher(this.endpoint(`display-revisions/${encodeURIComponent(revision)}/full`), {
        method: "POST",
        cache: "no-store",
        signal: this.ac.signal,
      })
      if (!this.isCurrent(generation, revision)) return
      if (response.status === 409 || response.status === 404 || response.status === 410) {
        return this.expire(generation, "This display revision expired before its PNG was ready. Retry this display.")
      }
      if (response.status !== 200 && response.status !== 202) {
        this.fullRequested = false
        throw new Error(await responseError(response, "The server could not prepare the full PNG."))
      }
      if (!this.snapshot.fullBlob) {
        this.publish({
          ...this.snapshot,
          phase: "rendering",
          gate: { ...CLOSED_GATE, label: "Preparing the full-resolution server PNG…" },
        })
      }
    } catch (error) {
      this.fullRequested = false
      this.fail(generation, error, true)
    }
  }

  private async poll(generation: number, revision: string) {
    if (!this.isCurrent(generation, revision)) return
    try {
      const response = await this.fetcher(this.endpoint(`display-revisions/${encodeURIComponent(revision)}/status`), {
        cache: "no-store",
        signal: this.ac.signal,
      })
      if (!this.isCurrent(generation, revision)) return
      if (response.status === 404 || response.status === 410 || response.status === 409) {
        return this.expire(generation, "This server display revision has expired. Retry this display.")
      }
      if (!response.ok) throw new Error(await responseError(response, "The server display status could not be read."))
      const manifest = validateDisplayManifest(await response.json(), {
        jobId: this.expected.jobId,
        revision,
        width: this.expected.width,
        height: this.expected.height,
      })
      if (!this.isCurrent(generation, revision)) return
      if (!sameServerColorDisplay(manifest.display, serverColorDisplay(this.display))) {
        return this.fail(generation, new Error("The server display settings do not match this revision."), true)
      }
      if (!manifest.current) return this.expire(generation, "The server no longer lists this revision as current. Retry for fresh server output.")
      if (manifest.preview.state === "superseded" || manifest.preview.state === "evicted" ||
          manifest.full.state === "superseded" || manifest.full.state === "evicted") {
        return this.expire(generation, "The server no longer retains this revision's PNG. Retry for fresh server output.")
      }
      const progress = {
        completedRows: manifest.full.completed_rows,
        totalRows: manifest.full.total_rows,
      }
      const label = manifest.full.state === "not_requested"
        ? "Preview ready; full-resolution PNG will be requested when settings settle"
        : manifest.full.state === "queued"
          ? "Full-resolution PNG queued on the server…"
          : manifest.full.state === "ready"
            ? "Fetching and verifying the full-resolution PNG…"
            : `Rendering full-resolution PNG · ${progress.completedRows}/${progress.totalRows} rows`
      this.publish({
        ...this.snapshot,
        manifest,
        ...progress,
        phase: this.snapshot.fullBlob ? "ready" : "rendering",
        error: manifest.full.state === "failed" ? manifest.full.error ?? "The server could not render the full PNG." : undefined,
        gate: this.snapshot.fullBlob
          ? { exportReady: true, remapBusy: false, completedRevision: generation, pendingRevision: null, label: "Current server PNG is ready" }
          : { ...CLOSED_GATE, label },
      })
      if (manifest.preview.state === "ready" && !this.previewFetchStarted) {
        this.previewFetchStarted = true
        void this.fetchArtifact(generation, revision, "preview", manifest)
      }
      if (manifest.full.state === "ready" && !this.fullFetchStarted) {
        this.fullFetchStarted = true
        void this.fetchArtifact(generation, revision, "full", manifest)
      }
      if (manifest.full.state === "failed") {
        return this.fail(generation, new Error(manifest.full.error ?? "The server could not render the full PNG."), true)
      }
      this.pollTimer = setTimeout(() => void this.poll(generation, revision), this.timings.pollMs)
    } catch (error) {
      if (this.snapshot.fullBlob) {
        // A cached current Blob remains exportable during a transient outage.
        this.publish({ ...this.snapshot, phase: "ready", error: errorText(error), gate: { ...this.snapshot.gate, exportReady: true, label: "Cached current server PNG remains available" } })
        this.pollTimer = setTimeout(() => void this.poll(generation, revision), this.timings.pollMs)
        return
      }
      this.fail(generation, error)
    }
  }

  private async fetchArtifact(generation: number, revision: string, kind: "preview" | "full", manifest: DisplayManifest) {
    try {
      const response = await this.fetcher(this.endpoint(`display-revisions/${encodeURIComponent(revision)}/${kind}.png`), {
        cache: "no-store",
        signal: this.ac.signal,
      })
      if (!this.isCurrent(generation, revision)) return
      if (!response.ok) {
        if (response.status === 404 || response.status === 410 || response.status === 409) {
          return this.expire(generation, `The server ${kind} PNG expired. Retry the current display.`)
        }
        throw new Error(await responseError(response, `The server ${kind} PNG could not be fetched.`))
      }
      const artifact = manifest[kind]
      const blob = await verifiedPng(response, artifact.width, artifact.height, artifact.bytes)
      if (!this.isCurrent(generation, revision)) return
      const url = URL.createObjectURL(blob)
      if (kind === "preview") {
        if (this.previewUrl) URL.revokeObjectURL(this.previewUrl)
        this.previewUrl = url
        this.publish({ ...this.snapshot, previewUrl: url })
        return
      }
      if (this.fullUrl) URL.revokeObjectURL(this.fullUrl)
      this.fullUrl = url
      this.publish({
        ...this.snapshot,
        phase: "ready",
        fullUrl: url,
        fullBlob: blob,
        gate: { exportReady: true, remapBusy: false, completedRevision: generation, pendingRevision: null, label: "Current server PNG is ready" },
        error: undefined,
      })
    } catch (error) {
      if (kind === "preview" && this.isCurrent(generation, revision)) {
        this.previewFetchStarted = false
        if (!this.snapshot.fullBlob) this.publish({ ...this.snapshot, error: errorText(error) })
        return
      }
      if (kind === "full" && !this.snapshot.fullBlob) this.fullFetchStarted = false
      this.fail(generation, error, true)
    }
  }

  private expire(generation: number, message: string) {
    if (!this.isCurrent(generation)) return
    this.clearTimers()
    this.generation++
    if (this.snapshot.fullBlob) {
      this.publish({
        ...this.snapshot,
        phase: "ready",
        error: `${message} The cached current PNG remains available to download.`,
        gate: { ...this.snapshot.gate, exportReady: true, label: "Cached current server PNG remains available" },
      })
      return
    }
    this.releaseUrls()
    this.revision = undefined
    this.publish({
      ...this.snapshot,
      phase: "expired",
      revision: undefined,
      previewUrl: undefined,
      fullUrl: undefined,
      fullBlob: undefined,
      error: message,
      gate: { ...CLOSED_GATE, label: "Server PNG expired; retry the current display" },
    })
  }

  private fail(generation: number, error: unknown, preserveBlob = false) {
    if (!this.isCurrent(generation)) return
    if (preserveBlob && this.snapshot.fullBlob) {
      this.clearTimers()
      this.generation++
      this.publish({
        ...this.snapshot,
        phase: "ready",
        error: errorText(error),
        gate: { ...this.snapshot.gate, exportReady: true, label: "Cached current server PNG remains available" },
      })
      return
    }
    this.clearTimers()
    this.generation++
    this.publish({
      ...this.snapshot,
      phase: "failed",
      error: errorText(error),
      gate: { ...CLOSED_GATE, label: "Server PNG failed; retry the current display" },
    })
  }

  private publish(snapshot: ColorDisplaySnapshot) {
    if (this.dead) return
    this.snapshot = snapshot
    this.changed(snapshot)
  }

  private isCurrent(generation: number, revision?: string): boolean {
    return !this.dead && !this.ac.signal.aborted && generation === this.generation &&
      (revision === undefined || revision === this.revision)
  }

  private endpoint(path: string): string {
    return `/api/v1/mosaic/${encodeURIComponent(this.expected.jobId)}/${path}`
  }

  private clearTimers() {
    if (this.previewTimer !== undefined) clearTimeout(this.previewTimer)
    if (this.fullTimer !== undefined) clearTimeout(this.fullTimer)
    if (this.pollTimer !== undefined) clearTimeout(this.pollTimer)
    this.previewTimer = undefined
    this.fullTimer = undefined
    this.pollTimer = undefined
  }

  private releaseUrls() {
    if (this.previewUrl) URL.revokeObjectURL(this.previewUrl)
    if (this.fullUrl) URL.revokeObjectURL(this.fullUrl)
    this.previewUrl = undefined
    this.fullUrl = undefined
  }
}

async function verifiedPng(response: Response, width: number, height: number, bytes: number | null): Promise<Blob> {
  const mime = response.headers.get("content-type")?.split(";", 1)[0].trim().toLowerCase()
  const length = Number(response.headers.get("content-length"))
  if (mime !== "image/png" || !Number.isSafeInteger(length) || length <= 0 || (bytes !== null && bytes !== length)) {
    throw new Error("The server PNG has an invalid MIME type or byte length.")
  }
  const blob = await response.blob()
  if (blob.size !== length || blob.size < 57) throw new Error("The server PNG is incomplete.")
  const head = new Uint8Array(await blob.slice(0, 33).arrayBuffer())
  const tail = new Uint8Array(await blob.slice(-12).arrayBuffer())
  const signature = [137, 80, 78, 71, 13, 10, 26, 10]
  const ihdr = [73, 72, 68, 82]
  const iend = [73, 69, 78, 68]
  const header = new DataView(head.buffer, head.byteOffset, head.byteLength)
  if (!signature.every((byte, index) => head[index] === byte) ||
      header.getUint32(8, false) !== 13 || !ihdr.every((byte, index) => head[12 + index] === byte) ||
      header.getUint32(16, false) !== width || header.getUint32(20, false) !== height ||
      head[24] !== 8 || head[25] !== 2 || head[26] !== 0 || head[27] !== 0 || head[28] !== 0) {
    throw new Error("The server PNG is not an 8-bit RGB image with a valid IHDR and matching dimensions.")
  }
  if (tail[0] !== 0 || tail[1] !== 0 || tail[2] !== 0 || tail[3] !== 0 ||
      !iend.every((byte, index) => tail[4 + index] === byte) ||
      tail[8] !== 174 || tail[9] !== 66 || tail[10] !== 96 || tail[11] !== 130) {
    throw new Error("The server PNG does not end with a valid IEND chunk.")
  }
  return blob
}

async function responseError(response: Response, fallback: string): Promise<string> {
  try {
    const value: unknown = await response.json()
    if (isRecord(value) && typeof value.error === "string") return value.error
  } catch {
    // The server may return an empty error body.
  }
  return fallback
}
function errorText(error: unknown): string {
  return error instanceof Error ? error.message : "The server display request failed."
}
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
}
