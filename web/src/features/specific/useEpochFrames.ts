import { useEffect, useState } from "react"
import { client } from "../../api"
import type { View } from "../../url-state/view"
import type { Bounds } from "./bounds"
import { boundsKey } from "./link"
import { frameSearch, visitsInBounds, type EpochVisit } from "./frames"

export type EpochFrame = {
  visit: EpochVisit
  /** `empty` is a visit with no sample in the chosen bands. */
  state: "queued" | "loading" | "ready" | "empty" | "failed"
  bitmap?: ImageBitmap
  error?: string
}

export type EpochFrames = {
  phase: "idle" | "listing" | "loading" | "done" | "failed"
  frames: EpochFrame[]
  /** Visits the release has for this field, before the UTC window. */
  visitsTotal: number
  error?: string
}

const IDLE: EpochFrames = { phase: "idle", frames: [], visitsTotal: 0 }
const PARALLEL_FRAMES = 2

/** One rendered image per visit of the field. Nothing is asked of the engine
 * until `enabled`, which only Find sets; disarming aborts the open requests. */
export function useEpochFrames(view: View, bounds: Bounds, enabled: boolean, run: number): EpochFrames {
  const [state, setState] = useState<EpochFrames>(IDLE)
  const { ra, dec, fovL, fovH, dr, mask } = view
  const key = boundsKey(bounds)

  useEffect(() => {
    if (!enabled) {
      setState(IDLE)
      return
    }
    const ac = new AbortController()
    const patch = (at: number, next: Partial<EpochFrame>) => {
      if (ac.signal.aborted) return
      setState((current) => ({
        ...current,
        frames: current.frames.map((frame, index) => (index === at ? { ...frame, ...next } : frame)),
      }))
    }
    const load = async () => {
      const coverage = await client.GET("/api/v1/coverage", {
        params: { query: { ra, dec, fov_l: fovL, fov_h: fovH, dr, pin: [] } },
        signal: ac.signal,
      })
      if (!coverage.data) throw new Error(coverage.error?.error ?? "The visit list could not be read.")
      const visits = visitsInBounds(coverage.data.visits, bounds)
      if (ac.signal.aborted) return
      setState({
        phase: visits.length > 0 ? "loading" : "done",
        frames: visits.map((visit) => ({ visit, state: "queued" })),
        visitsTotal: coverage.data.visits.length,
      })
      let next = 0
      const worker = async () => {
        while (!ac.signal.aborted && next < visits.length) {
          const at = next
          next += 1
          patch(at, { state: "loading" })
          const frame = await renderFrame(frameSearch({ ra, dec, fovL, fovH, dr, mask }, bounds.bands, visits[at].index), ac.signal)
          patch(at, frame)
        }
      }
      await Promise.all(Array.from({ length: PARALLEL_FRAMES }, worker))
      if (!ac.signal.aborted) setState((current) => ({ ...current, phase: "done" }))
    }
    setState({ ...IDLE, phase: "listing" })
    load().catch((cause) => {
      if (ac.signal.aborted) return
      setState({ ...IDLE, phase: "failed", error: cause instanceof Error ? cause.message : "The epochs could not be loaded." })
    })
    return () => ac.abort()
  }, [enabled, run, ra, dec, fovL, fovH, dr, mask, key])

  return state
}

async function renderFrame(search: string, signal: AbortSignal): Promise<Partial<EpochFrame>> {
  try {
    const response = await fetch(`/api/v1/render.png?${search}`, { signal })
    if (response.status === 404) return { state: "empty" }
    if (!response.ok) {
      const body = await response.json().catch(() => undefined)
      return { state: "failed", error: typeof body?.error === "string" ? body.error : `The engine answered ${response.status}.` }
    }
    return { state: "ready", bitmap: await createImageBitmap(await response.blob()) }
  } catch (cause) {
    return { state: "failed", error: cause instanceof Error ? cause.message : "The frame could not be read." }
  }
}
