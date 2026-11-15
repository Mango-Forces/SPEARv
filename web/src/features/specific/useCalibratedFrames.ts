import { useEffect, useState } from "react"
import type { View } from "../../url-state/view"
import type { Bounds } from "./bounds"
import { boundsKey } from "./link"
import type { FramePlane } from "./calibrate"
import {
  exposuresInBounds,
  frameBody,
  visitsInBounds,
  type EpochVisit,
  type ExposureRow,
} from "./frames"

export type FrameKind = "visit" | "custom"

export type CalibratedFrame = {
  id: string
  kind: FrameKind
  label: string
  /** Visit index when kind is visit; null for a custom sector. */
  visit: number | null
  mjd: number
  mjd_end: number
  exposureIds: string[]
  state: "queued" | "loading" | "ready" | "empty" | "failed"
  plane?: FramePlane
  error?: string
}

export type CalibratedRun = {
  phase: "idle" | "listing" | "loading" | "done" | "failed"
  visits: EpochVisit[]
  exposures: ExposureRow[]
  frames: CalibratedFrame[]
  error?: string
}

const IDLE: CalibratedRun = { phase: "idle", visits: [], exposures: [], frames: [] }
const PARALLEL = 2

/** One calibrated plane per visit (or custom sector). Nothing hits the engine
 * until `enabled`, which only Find sets; disarming aborts open requests. */
export function useCalibratedFrames(
  view: View,
  bounds: Bounds,
  enabled: boolean,
  run: number,
  /** Extra custom frames the user composed from exposure ids. */
  custom: { id: string; label: string; exposureIds: string[] }[] = [],
): CalibratedRun {
  const [state, setState] = useState<CalibratedRun>(IDLE)
  const { ra, dec, fovL, fovH, dr, mask } = view
  const key = boundsKey(bounds)
  const customKey = JSON.stringify(custom.map((item) => [item.id, item.exposureIds]))

  useEffect(() => {
    if (!enabled) {
      setState(IDLE)
      return
    }
    const ac = new AbortController()
    const patch = (at: number, next: Partial<CalibratedFrame>) => {
      if (ac.signal.aborted) return
      setState((current) => ({
        ...current,
        frames: current.frames.map((frame, index) => (index === at ? { ...frame, ...next } : frame)),
      }))
    }
    const load = async () => {
      const listed = await fetchExposures(view, ac.signal)
      if (ac.signal.aborted) return
      const visits = visitsInBounds(listed.visits, bounds)
      const rows = exposuresInBounds(listed.exposures, bounds)
      const visitFrames: CalibratedFrame[] = visits.map((visit) => ({
        id: `visit-${visit.index}`,
        kind: "visit",
        label: visitLabel(visit),
        visit: visit.index,
        mjd: visit.mjd,
        mjd_end: visit.mjd_end,
        exposureIds: rows.filter((row) => row.visit === visit.index).map((row) => row.id),
        state: "queued",
      }))
      const customFrames: CalibratedFrame[] = custom.map((item) => {
        const mjds = item.exposureIds
          .map((id) => rows.find((row) => row.id === id)?.mjd)
          .filter((mjd): mjd is number => mjd !== undefined)
        const mjd = mjds.length ? Math.min(...mjds) : 0
        const mjd_end = mjds.length ? Math.max(...mjds) : 0
        return {
          id: item.id,
          kind: "custom",
          label: item.label,
          visit: null,
          mjd,
          mjd_end,
          exposureIds: item.exposureIds,
          state: "queued",
        }
      })
      const frames = [...visitFrames, ...customFrames]
      setState({
        phase: frames.length > 0 ? "loading" : "done",
        visits: listed.visits,
        exposures: listed.exposures,
        frames,
      })
      let next = 0
      const worker = async () => {
        while (!ac.signal.aborted && next < frames.length) {
          const at = next
          next += 1
          const frame = frames[at]!
          patch(at, { state: "loading" })
          const selection =
            frame.kind === "custom" || frame.exposureIds.length > 0
              ? { exposures: frame.exposureIds }
              : { visit: frame.visit! }
          if (frame.kind === "custom" && frame.exposureIds.length === 0) {
            patch(at, { state: "empty" })
            continue
          }
          const result = await renderFrame(frameBody({ ra, dec, fovL, fovH, dr, mask }, bounds.bands, selection), ac.signal)
          patch(at, result)
        }
      }
      await Promise.all(Array.from({ length: PARALLEL }, () => worker()))
      if (!ac.signal.aborted) setState((current) => ({ ...current, phase: "done" }))
    }
    setState({ ...IDLE, phase: "listing" })
    load().catch((cause) => {
      if (ac.signal.aborted) return
      setState({
        ...IDLE,
        phase: "failed",
        error: cause instanceof Error ? cause.message : "The epochs could not be loaded.",
      })
    })
    return () => ac.abort()
  }, [enabled, run, ra, dec, fovL, fovH, dr, mask, key, customKey])

  return state
}

async function fetchExposures(view: View, signal: AbortSignal): Promise<{ visits: EpochVisit[]; exposures: ExposureRow[] }> {
  const params = new URLSearchParams({
    ra: String(view.ra),
    dec: String(view.dec),
    fov_l: String(view.fovL),
    fov_h: String(view.fovH),
    dr: view.dr,
  })
  const response = await fetch(`/api/v1/exposures?${params}`, { signal })
  if (!response.ok) {
    const body = await response.json().catch(() => undefined)
    throw new Error(typeof body?.error === "string" ? body.error : `Exposures failed (${response.status}).`)
  }
  const data = await response.json()
  return {
    visits: data.visits ?? [],
    exposures: (data.exposures ?? []).map((row: ExposureRow) => ({
      ...row,
      visit: row.visit ?? null,
    })),
  }
}

async function renderFrame(body: ReturnType<typeof frameBody>, signal: AbortSignal): Promise<Partial<CalibratedFrame>> {
  try {
    const response = await fetch("/api/v1/frame", {
      method: "POST",
      headers: { "Content-Type": "application/json", Accept: "application/octet-stream" },
      body: JSON.stringify(body),
      signal,
    })
    if (response.status === 404) return { state: "empty" }
    if (!response.ok) {
      const err = await response.json().catch(() => undefined)
      return { state: "failed", error: typeof err?.error === "string" ? err.error : `The engine answered ${response.status}.` }
    }
    const width = Number(response.headers.get("x-grid-width"))
    const height = Number(response.headers.get("x-grid-height"))
    if (!width || !height) return { state: "failed", error: "The frame had no grid size." }
    const buffer = await response.arrayBuffer()
    if (buffer.byteLength !== width * height * 4) {
      return { state: "failed", error: `Frame size ${buffer.byteLength} does not match ${width}×${height}.` }
    }
    return {
      state: "ready",
      plane: { width, height, values: new Float32Array(buffer.slice(0)) },
    }
  } catch (cause) {
    if (signal.aborted) return { state: "failed" }
    return { state: "failed", error: cause instanceof Error ? cause.message : "The frame could not be read." }
  }
}

function visitLabel(visit: EpochVisit): string {
  const start = new Date((visit.mjd - 40587) * 86400000).toLocaleDateString("en-GB", {
    day: "numeric",
    month: "short",
    year: "numeric",
    timeZone: "UTC",
  })
  return start
}
