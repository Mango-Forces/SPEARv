import { useEffect, useState } from "react"
import { client } from "../../api"
import type { components } from "../../bindings/schema"

export type SpectrumPoint = components["schemas"]["Point"]
export type SpectrumBody = components["schemas"]["SpectrumBody"]

/** The clicked picture pixel and the sky position the page mapped it to. */
export type SpectrumPointTarget = {
  x: number
  y: number
  ra: number
  dec: number
}

export type SpectrumRequest = {
  /** View centre the click belongs to. */
  ra: number
  dec: number
  /** Release id, for example "qr2". */
  release: string
  point: SpectrumPointTarget | null
  /**
   * Optional view field of view in degrees. When present the request uses the
   * canonical rendered-grid pixel the click came from. Without it the panel
   * samples the clicked sky directly, because `/api/v1/spectrum` derives the
   * sampled sky from a pixel on a rendered grid and the panel is not handed one.
   */
  fovL?: number
  fovH?: number
}

export type SpectrumState =
  | { status: "idle" }
  | { status: "loading" }
  | { status: "ready"; body: SpectrumBody }
  | { status: "failed"; error: string }

// A rendered grid is one native SPHEREx pixel per cell (6.15"). Any odd grid
// centred on the clicked sky reads that same sky back from its middle pixel, so
// an 11 px request samples exactly the click when the view fov is not supplied.
// It is also just inside the 0.02° cutout the server takes per exposure.
const PIXEL_SCALE_DEG = 6.15 / 3600
const SAMPLE_GRID = 11
const SAMPLE_FOV_DEG = SAMPLE_GRID * PIXEL_SCALE_DEG
const SAMPLE_PIXEL = (SAMPLE_GRID - 1) / 2

export function useSpectrum(request: SpectrumRequest): SpectrumState {
  const [state, setState] = useState<SpectrumState>(() => (request.point ? { status: "loading" } : { status: "idle" }))
  const targetRa = request.point?.ra
  const targetDec = request.point?.dec
  const targetX = request.point?.x
  const targetY = request.point?.y
  const viewFovL = request.fovL != null && Number.isFinite(request.fovL) && request.fovL > 0 ? request.fovL : undefined
  const viewFovH = request.fovH != null && Number.isFinite(request.fovH) && request.fovH > 0 ? request.fovH : undefined

  useEffect(() => {
    if (targetRa === undefined || targetDec === undefined || targetX === undefined || targetY === undefined) {
      setState({ status: "idle" })
      return
    }
    const controller = new AbortController()
    setState({ status: "loading" })
    const query =
      viewFovL === undefined || viewFovH === undefined
        ? {
            ra: targetRa,
            dec: targetDec,
            fov_l: SAMPLE_FOV_DEG,
            fov_h: SAMPLE_FOV_DEG,
            x: SAMPLE_PIXEL,
            y: SAMPLE_PIXEL,
          }
        : {
            ra: request.ra,
            dec: request.dec,
            fov_l: viewFovL,
            fov_h: viewFovH,
            x: targetX,
            y: targetY,
          }
    client
      .GET("/api/v1/spectrum", {
        params: {
          query: {
            ...query,
            ...(request.release ? { dr: request.release } : {}),
          },
        },
        signal: controller.signal,
      })
      .then((result) => {
        if (controller.signal.aborted) return
        if (!result.data) {
          setState({ status: "failed", error: failureText(result.error, result.response.status) })
          return
        }
        setState({ status: "ready", body: result.data })
      })
      .catch((error: unknown) => {
        if (controller.signal.aborted) return
        setState({ status: "failed", error: error instanceof Error ? error.message : "the spectrum request failed" })
      })
    return () => controller.abort()
  }, [targetRa, targetDec, targetX, targetY, viewFovL, viewFovH, request.ra, request.dec, request.release])

  return state
}

function failureText(error: unknown, status: number): string {
  const server = error && typeof error === "object" && "error" in error && typeof error.error === "string" ? error.error : ""
  if (status === 504) {
    return `${server || "The spectrum read stopped at 45 seconds."} Try a smaller field of view.`
  }
  return server || `request failed (${status})`
}
