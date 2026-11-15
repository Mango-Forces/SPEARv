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
import { BASE } from "../../api.ts"

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
/** Server PNGs kept verbatim in this page, so an evicted revision or a
 * re-created display client can never lose a finished image. */
const CACHE_LIMIT = 4
const RECOVERY_ATTEMPTS = 4
const RECOVERY_DELAY_MS = 250
const CLOSED_GATE: RenderGate = {
  exportReady: false,
  remapBusy: false,
  completedRevision: 0,
  pendingRevision: null,
  label: "Preparing the server PNG…",
}

type CachedPng = { blob: Blob; url: string; manifest?: DisplayManifest }

const cachedPngs = new Map<string, CachedPng>()

/** Test helper: drop every retained PNG between cases. */
export function clearCachedPngs() {
  for (const entry of cachedPngs.values()) URL.revokeObjectURL(entry.url)
  cachedPngs.clear()
}

function pngCacheKey(jobId: string, settingsKey: string): string {
  return `${jobId}\n${settingsKey}`
}

function cachedPng(key: string): CachedPng | undefined {
  const hit = cachedPngs.get(key)
  if (!hit) return undefined
  cachedPngs.delete(key)
  cachedPngs.set(key, hit)
  return hit
}

function rememberPng(key: string, blob: Blob, manifest?: DisplayManifest): CachedPng {
  const existing = cachedPngs.get(key)
  if (existing) {
    cachedPngs.delete(key)
    cachedPngs.set(key, existing)
    return existing
  }
  const entry = { blob, url: URL.createObjectURL(blob), manifest }
  cachedPngs.set(key, entry)
  while (cachedPngs.size > CACHE_LIMIT) {
    const oldest = cachedPngs.keys().next().value
    if (oldest === undefined) break
    const gone = cachedPngs.get(oldest)
    cachedPngs.delete(oldest)
    if (gone) URL.revokeObjectURL(gone.url)
  }
  return entry
}

/** Owns one job's display generations and the exact server PNG Blob for export.
 * A downloaded PNG is cached by job and display settings, so later server
 * eviction, a re-created client or a retry reads the same bytes without asking
 * the engine again. */
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
  private recoveries = 0
  private lostRequested = false
  private snapshot: ColorDisplaySnapshot
  private timings: Required<Timings>

  constructor(
    private expected: { jobId: string; width: number; height: number },
    display: DisplayState,
    private changed: (snapshot: ColorDisplaySnapshot) => void,
    private fetcher: DisplayFetcher = (input, init) => globalThis.fetch(input, init),
    timings: Timings = {},
    private onLostJob?: () => void,
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
    const cached = cachedPng(this.cacheKey())
    if (cached) {
      this.adopt(cached)
      return
    }
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
    if (this.snapshot.fullBlob) {
      this.settle(this.snapshot.fullBlob, this.snapshot.manifest)
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
    this.recoveries = 0
    this.lostRequested = false
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
    const cached = cachedPng(this.cacheKey())
    if (cached) {
      this.adopt(cached)
      return
    }
    this.previewTimer = setTimeout(() => void this.createRevision(generation), this.timings.previewDebounceMs)
    this.fullTimer = setTimeout(() => {
      if (!this.isCurrent(generation)) return
      this.fullDue = true
      if (this.revision) void this.requestFull(generation, this.revision)
    }, this.timings.fullDebounceMs)
  }

  /** Publish the cached PNG at once and ask the engine for nothing. */
  private adopt(cached: CachedPng) {
    this.revision = undefined
    this.fullRequested = true
    this.fullFetchStarted = true
    this.previewFetchStarted = true
    this.clearTimers()
    this.publish(this.ready(cached.blob, cached.manifest, cached.url))
  }

  private ready(blob: Blob, manifest: DisplayManifest | undefined, url: string): ColorDisplaySnapshot {
    return {
      ...this.snapshot,
      phase: "ready",
      manifest,
      fullUrl: url,
      fullBlob: blob,
      error: undefined,
      gate: { exportReady: true, remapBusy: false, completedRevision: this.generation, pendingRevision: null, label: "Current server PNG is ready" },
    }
  }

  private settle(blob: Blob, manifest: DisplayManifest | undefined) {
    const cached = rememberPng(this.cacheKey(), blob, manifest)
    this.clearTimers()
    this.publish(this.ready(cached.blob, cached.manifest, cached.url))
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
      if (response.status === 404 || response.status === 410) {
        this.recover(generation, true)
        return
      }
      if (response.status !== 200 && response.status !== 201) {
        throw new Error(await responseError(response, "The server could not create this display revision."))
      }
      const value: unknown = await response.json()
      if (!isRecord(value) || typeof value.revision !== "string" || value.revision.length === 0) {
        throw new Error("The server returned an invalid display revision.")
      }
      if (!this.isCurrent(generation)) return
      this.recoveries = 0
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
      if (isAbortError(error) || this.dead) return
      this.fail(generation, error, true)
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
        this.recover(generation, false)
        return
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
      if (isAbortError(error) || this.dead) return
      this.fullRequested = false
      this.fail(generation, error, true)
    }
  }

  private async poll(generation: number, revision: string) {
    if (!this.isCurrent(generation, revision)) return
    if (this.snapshot.fullBlob) {
      this.clearTimers()
      return
    }
    try {
      const response = await this.fetcher(this.endpoint(`display-revisions/${encodeURIComponent(revision)}/status`), {
        cache: "no-store",
        signal: this.ac.signal,
      })
      if (!this.isCurrent(generation, revision)) return
      if (response.status === 404 || response.status === 410 || response.status === 409) {
        this.recover(generation, false)
        return
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
      if (!manifest.current) {
        this.recover(generation, false)
        return
      }
      if (manifest.preview.state === "superseded" || manifest.preview.state === "evicted" ||
          manifest.full.state === "superseded" || manifest.full.state === "evicted") {
        this.recover(generation, false)
        return
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
      if (isAbortError(error) || this.dead) return
      if (this.snapshot.fullBlob) {
        this.settle(this.snapshot.fullBlob, this.snapshot.manifest)
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
          this.recover(generation, false)
          return
        }
        throw new Error(await responseError(response, `The server ${kind} PNG could not be fetched.`))
      }
      const artifact = manifest[kind]
      const blob = await verifiedPng(response, artifact.width, artifact.height, artifact.bytes)
      if (!this.isCurrent(generation, revision)) return
      if (kind === "preview") {
        const url = URL.createObjectURL(blob)
        if (this.previewUrl) URL.revokeObjectURL(this.previewUrl)
        this.previewUrl = url
        this.publish({ ...this.snapshot, previewUrl: url })
        return
      }
      this.settle(blob, manifest)
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

  /** The job or its revision went away. A cached PNG stays exactly as it is;
   * otherwise this quietly re-creates the revision, or asks the parent to run
   * the view again when the job itself is gone. */
  private recover(generation: number, jobLost: boolean) {
    if (this.dead || this.ac.signal.aborted) return
    if (!this.isCurrent(generation)) return
    if (this.snapshot.fullBlob) {
      this.settle(this.snapshot.fullBlob, this.snapshot.manifest)
      return
    }
    if (jobLost) {
      this.clearTimers()
      this.revision = undefined
      this.ac.abort()
      this.publish({
        ...this.snapshot,
        phase: "creating",
        revision: undefined,
        previewUrl: undefined,
        fullUrl: undefined,
        completedRows: 0,
        totalRows: 0,
        error: undefined,
        gate: { ...CLOSED_GATE, label: "Reconnecting the server image…" },
      })
      if (!this.lostRequested) {
        this.lostRequested = true
        this.onLostJob?.()
      }
      return
    }
    this.releaseUrls()
    this.revision = undefined
    this.fullRequested = false
    this.fullFetchStarted = false
    this.previewFetchStarted = false
    if (this.recoveries >= RECOVERY_ATTEMPTS) {
      this.fail(generation, new Error("The server image could not be restored."), true)
      return
    }
    this.recoveries++
    this.generation++
    const next = this.generation
    this.ac.abort()
    this.ac = new AbortController()
    this.clearTimers()
    this.publish({
      ...this.snapshot,
      phase: "creating",
      revision: undefined,
      previewUrl: undefined,
      fullUrl: undefined,
      completedRows: 0,
      totalRows: 0,
      error: undefined,
      gate: { ...CLOSED_GATE, label: "Restoring the server image…" },
    })
    this.previewTimer = setTimeout(() => void this.createRevision(next), RECOVERY_DELAY_MS)
  }

  private fail(generation: number, error: unknown, preserveBlob = false) {
    // Dispose aborts in-flight work; that is not a display failure.
    if (this.dead || isAbortError(error)) return
    if (!this.isCurrent(generation)) return
    if (preserveBlob && this.snapshot.fullBlob) {
      this.settle(this.snapshot.fullBlob, this.snapshot.manifest)
      return
    }
    this.clearTimers()
    this.generation++
    this.publish({
      ...this.snapshot,
      phase: "failed",
      error: errorText(error),
      gate: { ...CLOSED_GATE, label: "The server PNG failed; try the display again" },
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

  private cacheKey(display = this.display): string {
    return pngCacheKey(this.expected.jobId, colorDisplayKey(display))
  }

  private endpoint(path: string): string {
    return `${BASE}/api/v1/mosaic/${encodeURIComponent(this.expected.jobId)}/${path}`
  }

  private clearTimers() {
    if (this.previewTimer !== undefined) clearTimeout(this.previewTimer)
    if (this.fullTimer !== undefined) clearTimeout(this.fullTimer)
    if (this.pollTimer !== undefined) clearTimeout(this.pollTimer)
    this.previewTimer = undefined
    this.fullTimer = undefined
    this.pollTimer = undefined
  }

  /** Full PNG URLs belong to the cache and are revoked only when it evicts
   * them; the preview is owned by this client. */
  private releaseUrls() {
    if (this.previewUrl) URL.revokeObjectURL(this.previewUrl)
    this.previewUrl = undefined
  }
}

async function verifiedPng(response: Response, width: number, height: number, bytes: number | null): Promise<Blob> {
  const mime = response.headers.get("content-type")?.split(";", 1)[0].trim().toLowerCase()
  const lengthHeader = response.headers.get("content-length")
  const length = lengthHeader === null ? null : Number(lengthHeader)
  if (mime !== "image/png" || (length !== null && (!Number.isSafeInteger(length) || length <= 0)) || (bytes !== null && length !== null && bytes !== length)) {
    throw new Error("The server PNG has an invalid MIME type or byte length.")
  }
  const blob = await response.blob()
  if ((length !== null && blob.size !== length) || (bytes !== null && blob.size !== bytes) || blob.size < 57) {
    throw new Error("The server PNG is incomplete.")
  }
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

function isAbortError(error: unknown): boolean {
  return (
    (error instanceof DOMException && error.name === "AbortError") ||
    (error instanceof Error && error.name === "AbortError")
  )
}
function isRecord(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value)
}
