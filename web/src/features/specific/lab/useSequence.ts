import { useEffect, useRef, useState } from "react"
import { client } from "../../../api"
import type { components } from "../../../bindings/schema"
import { apiRegion, type Region } from "./state"

export type SequenceInfo = components["schemas"]["SequenceInfo"]
export type SubmittedSector = { id: string; revision: number; snippets: string[] }
/** The server sequence handle and its frozen mapping; frame tiles stay on demand. */
export type Prepared = { key: string; info: SequenceInfo; sequenceId: string }

/** Fetch only one sector's data tiles, in bounded batches, aligned to data_tiles. */
export async function loadFrame(info: SequenceInfo, sectorId: string, signal: AbortSignal): Promise<Blob[]> {
  const sector = info.sectors.find((item) => item.id === sectorId)
  if (!sector) throw new Error(`Sector ${sectorId} is not part of this sequence.`)
  if (info.display_revision == null) throw new Error("The shared display mapping is not ready.")
  const displayRevision = info.display_revision
  const tiles: Blob[] = []
  // Request batching limits concurrency, not the number of selected data tiles.
  for (let start = 0; start < sector.data_tiles.length; start += 6) {
    if (signal.aborted) throw new DOMException("Aborted", "AbortError")
    const batch = await Promise.all(sector.data_tiles.slice(start, start + 6).map(async ({ x, y }) => {
      const tile = await client.GET("/api/v1/sequences/{sequence_id}/sectors/{sector_id}/tiles/{x}/{y}", {
        params: { path: { sequence_id: info.sequence_id, sector_id: sector.id, x, y }, query: { display_revision: displayRevision } },
        parseAs: "blob", signal,
      })
      if (!tile.data) throw new Error(`Frame ${sectorId} tile ${x},${y} failed (${tile.response.status}).`)
      return tile.data
    }))
    tiles.push(...batch)
  }
  return tiles
}

/** Owns one server sequence at a time: build, follow and release. The decoded
 * frame window invokes loadFrame only while filling its bounded prefetch plan. */
export function useSequence(region: Region) {
  const regionKey = JSON.stringify([region.ra, region.dec, region.fovL, region.fovH])
  const controller = useRef<AbortController | undefined>(undefined)
  const handle = useRef<string | undefined>(undefined)
  const requested = useRef<string | undefined>(undefined)
  const [prepared, setPrepared] = useState<Prepared>()
  const [progress, setProgress] = useState<SequenceInfo>()
  const [error, setError] = useState<string>()
  const [loading, setLoading] = useState(false)

  function release() {
    if (!handle.current) return
    const sequence_id = handle.current
    handle.current = undefined
    void client.DELETE("/api/v1/sequences/{sequence_id}", { params: { path: { sequence_id } }, keepalive: true }).catch(() => undefined)
  }

  function discard() {
    controller.current?.abort()
    release()
    setPrepared(undefined)
    setProgress(undefined)
    setError(undefined)
    setLoading(false)
  }

  useEffect(() => discard, [regionKey])

  async function prepare(key: string, sectors: SubmittedSector[]) {
    discard()
    requested.current = key
    const ac = new AbortController()
    controller.current = ac
    setLoading(true)
    let createdId: string | undefined
    try {
      const created = await client.POST("/api/v1/sequences", { body: { region: apiRegion(region), sectors } })
      if (!created.data) throw new Error(created.error?.error ?? `Sector build failed (${created.response.status}).`)
      createdId = created.data.sequence_id
      if (ac.signal.aborted) {
        void client.DELETE("/api/v1/sequences/{sequence_id}", { params: { path: { sequence_id: createdId } } }).catch(() => undefined)
        return
      }
      handle.current = createdId
      let info = created.data
      setProgress(info)
      while (info.state === "building") {
        await wait(400, ac.signal)
        const status = await client.GET("/api/v1/sequences/{sequence_id}/status", { params: { path: { sequence_id: info.sequence_id } }, signal: ac.signal })
        if (!status.data) throw new Error(status.error?.error ?? `Sector build status failed (${status.response.status}).`)
        info = status.data
        setProgress(info)
      }
      if (info.state !== "ready" || info.display_revision == null) throw new Error(info.error ?? "Sector build failed.")
      if (!ac.signal.aborted) {
        // Keep the server handle: frame tiles are fetched on demand from this sequence.
        setPrepared({ key, info, sequenceId: info.sequence_id })
        setLoading(false)
      }
    } catch (cause) {
      if (!ac.signal.aborted) {
        // A failed build still owns server scratch space. Release it now rather
        // than waiting for the idle lease, but never touch a newer sequence.
        if (createdId && handle.current === createdId) release()
        setError(cause instanceof Error ? cause.message : "Sector build failed.")
        ac.abort()
      }
    } finally {
      if (controller.current === ac) setLoading(false)
    }
  }

  function invalidateUnless(key: string) {
    if (!requested.current || requested.current === key) return
    controller.current?.abort()
    release()
    setLoading(false)
    setProgress(undefined)
    // Keep the prepared provenance for the stale labels; it has no frame blobs and
    // the workspace's discarded flag stops it from becoming valid again.
  }

  return { prepare, discard, invalidateUnless, loadFrame, prepared, progress, error, loading }
}

function wait(ms: number, signal: AbortSignal) {
  return new Promise<void>((resolve, reject) => {
    if (signal.aborted) { reject(new DOMException("Aborted", "AbortError")); return }
    const abort = () => { clearTimeout(timer); reject(new DOMException("Aborted", "AbortError")) }
    const timer = setTimeout(() => { signal.removeEventListener("abort", abort); resolve() }, ms)
    signal.addEventListener("abort", abort, { once: true })
  })
}
