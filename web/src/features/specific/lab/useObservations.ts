import { useEffect, useRef, useState } from "react"
import { client } from "../../../api"
import type { Bounds } from "../bounds"
import { apiRegion, type Region, type Snippet } from "./state"

type Page = { rows: Snippet[]; matches: number; pending: number; failed: number; sampled: boolean; sampledFrom: number; revision: number; state: string }
const EMPTY: Page = { rows: [], matches: 0, pending: 0, failed: 0, sampled: false, sampledFrom: 0, revision: 0, state: "idle" }

/** Exact generated engine API. Discovery is deliberately separate from rendering.
 * `revision` changes on every explicit Apply/Reset so an unchanged bounds value
 * still retries a failed or expired query instead of reusing the broken key.
 * `perVisit` is the quick-video sample; manual browsing leaves it undefined. */
export function useObservations(region: Region, bounds: Bounds, active: boolean, search: string, sort: "time" | "band", direction: "asc" | "desc", offset: number, revision: number, perVisit?: number, pageSize = 50) {
  const key = JSON.stringify([region.ra, region.dec, region.fovL, region.fovH, bounds, revision])
  const pageKey = JSON.stringify([key, search, sort, direction, offset])
  const [dataset, setDataset] = useState<{ key: string; id: string }>()
  const [page, setPage] = useState<{ key: string; value: Page }>()
  const [error, setError] = useState<string>()
  const [loading, setLoading] = useState(false)
  const [cancelled, setCancelled] = useState(false)
  const [startedAt, setStartedAt] = useState<number>()
  const [elapsedMs, setElapsedMs] = useState(0)
  const stopped = useRef(false)
  const datasetId = useRef<string | undefined>(undefined)
  const pageAbort = useRef<AbortController | undefined>(undefined)

  useEffect(() => {
    if (!active) return
    const ac = new AbortController()
    let id: string | undefined
    let heartbeat: ReturnType<typeof setInterval> | undefined
    stopped.current = false
    datasetId.current = undefined
    setDataset(undefined)
    setPage(undefined)
    setError(undefined)
    setLoading(true)
    setCancelled(false)
    setStartedAt(Date.now())
    setElapsedMs(0)
    // Admission returns a resource handle even if the view changes meanwhile.
    // Do not abort it before we can release that handle.
    const release = (dataset_id: string) => void client.DELETE("/api/v1/observations/{dataset_id}", { params: { path: { dataset_id } }, keepalive: true }).catch(() => undefined)
    client.POST("/api/v1/observations/query", {
      body: {
        region: apiRegion(region),
        from_utc: bounds.fromUtc,
        to_utc: bounds.toUtc,
        releases: bounds.releases,
        bands: bounds.bands.join(";"),
        ...(perVisit === undefined ? {} : { per_visit: perVisit, visit_gap_days: 10 }),
      },
    })
      .then((result) => {
        id = result.data?.dataset_id
        if (ac.signal.aborted && id) release(id)
        if (ac.signal.aborted) return
        if (!result.data) throw new Error(result.response.status === 404 ? "Exact snippet discovery is not available on this engine yet. No approximate rows have been substituted." : result.error?.error ?? `Discovery failed (${result.response.status}).`)
        datasetId.current = id
        setDataset({ key, id: result.data.dataset_id })
        heartbeat = setInterval(() => { if (id) void client.GET("/api/v1/observations/{dataset_id}/status", { params: { path: { dataset_id: id } }, signal: ac.signal }).catch(() => undefined) }, 5 * 60_000)
      }).catch((cause: unknown) => {
        if (ac.signal.aborted) return
        setError(cause instanceof Error ? cause.message : "Discovery failed.")
        setLoading(false)
      })
    return () => {
      ac.abort()
      clearInterval(heartbeat)
      if (id && datasetId.current === id) datasetId.current = undefined
      if (id) release(id)
    }
    // perVisit deliberately does not restart an admitted dataset; it applies to
    // the next explicit Apply or quick-video run.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key, active])

  useEffect(() => {
    if (!active || !dataset || dataset.key !== key) return
    const ac = new AbortController()
    pageAbort.current = ac
    let timer: ReturnType<typeof setTimeout> | undefined
    setLoading(true)
    setError(undefined)
    const load = () => {
      if (stopped.current) return
      client.GET("/api/v1/observations/{dataset_id}", { params: { path: { dataset_id: dataset.id }, query: { search, sort, direction, offset, limit: pageSize } }, signal: ac.signal })
        .then((result) => {
          if (ac.signal.aborted || stopped.current) return
          if (!result.data) throw new Error(result.error?.error ?? `Discovery failed (${result.response.status}).`)
          const data = result.data
          setPage({ key: pageKey, value: { rows: data.rows.map((row) => ({ id: row.id, ref: row.snippet_ref, band: row.band, mjd: row.mjd, utc: row.utc, release: row.release, detector: row.detector, obsid: row.obsid, version: row.version, geometricSamples: row.geometric_samples, wl_min_um: row.wl_min_um, wl_max_um: row.wl_max_um, targetCovered: row.target_covered ?? false, quality: row.quality })), matches: data.matches_so_far, pending: data.pending_files, failed: data.failed_files, sampled: data.sampled, sampledFrom: data.sampled_from, revision: data.revision, state: data.state } })
          const terminal = data.state === "done" || data.state === "failed"
          setLoading(!terminal)
          setError(data.error ?? undefined)
          if (!terminal) timer = setTimeout(load, 500)
        }).catch((cause: unknown) => {
          if (ac.signal.aborted || stopped.current) return
          setError(cause instanceof Error ? cause.message : "Discovery failed.")
          setLoading(false)
        })
    }
    load()
    return () => { ac.abort(); clearTimeout(timer) }
  }, [dataset, key, pageKey, active, search, sort, direction, offset])

  useEffect(() => {
    if (startedAt === undefined || !loading) return
    const update = () => setElapsedMs(Date.now() - startedAt)
    update()
    const timer = setInterval(update, 1000)
    return () => clearInterval(timer)
  }, [startedAt, loading])

  const cancel = () => {
    stopped.current = true
    pageAbort.current?.abort()
    const id = datasetId.current
    datasetId.current = undefined
    if (id) void client.DELETE("/api/v1/observations/{dataset_id}", { params: { path: { dataset_id: id } }, keepalive: true }).catch(() => undefined)
    setLoading(false)
    setCancelled(true)
  }

  const value = page?.key === pageKey ? page.value : EMPTY
  return { ...value, loading, cancelled, error, elapsedMs, cancel }
}
