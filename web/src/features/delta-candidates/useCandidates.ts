import { useEffect, useState } from "react"
import type { View } from "../../url-state/view"

/** One labelled blob from `GET /api/v1/delta/candidates`. */
export type Candidate = {
  x: number
  y: number
  ra: number
  dec: number
  sign: "+" | "-"
  significance: number
  delta_mjy_sr: number
  pixels: number
  class: "star_residue" | "moving_candidate" | "unknown"
  reason: string
  exposure: string
  mjd: number
  flags: string[]
  other_exposure: string
  other_flags: string[]
}

export type CandidateBody = {
  note: string
  threshold_sigma: number
  noise_scale: number
  source_both_fraction: number
  truncated: boolean
  counts: { star_residue: number; moving_candidate: number; unknown: number }
  candidates: Candidate[]
}

export type CandidateState =
  | { status: "off" }
  | { status: "loading" }
  | { status: "ready"; body: CandidateBody }
  | { status: "failed"; error: string }

/** The API reads its own query string; this mirrors the view the server sees.
 * Display-only params (Gaia/WISE opacity) stay out, so toggling them does not
 * refetch candidates. */
function queryFor(view: View): string {
  const params = new URLSearchParams({
    ra: String(view.ra),
    dec: String(view.dec),
    fov_l: String(view.fovL),
    fov_h: String(view.fovH),
    dr: view.dr,
    mask: view.mask ? "on" : "off",
  })
  for (const pin of view.pins) {
    const visit = pin.visit === undefined ? "" : `v${pin.visit}`
    params.append("pin", `${pin.center}:${pin.width}:${visit}:${pin.sign ?? ""}`)
  }
  return params.toString()
}

/** `ready` holds the request back until the picture is finished. Candidates read
 * every cutout again, so they would otherwise compete with the tiles for the link. */
export function useCandidates(view: View, ready: boolean): CandidateState {
  const enabled = view.mode === "delta" && view.candidates && ready
  const key = enabled ? queryFor(view) : null
  const [state, setState] = useState<CandidateState>({ status: "off" })

  useEffect(() => {
    if (key === null) {
      setState({ status: "off" })
      return
    }
    const controller = new AbortController()
    let live = true
    setState({ status: "loading" })
    fetch(`/api/v1/delta/candidates?${key}`, { signal: controller.signal })
      .then(async (response) => {
        if (!response.ok) {
          const body = await response.json().catch(() => null)
          throw new Error(body?.error ?? `candidates failed (${response.status})`)
        }
        return (await response.json()) as CandidateBody
      })
      .then((body) => {
        if (live) setState({ status: "ready", body })
      })
      .catch((error: unknown) => {
        if (live && !(error instanceof DOMException && error.name === "AbortError")) {
          setState({ status: "failed", error: String(error instanceof Error ? error.message : error) })
        }
      })
    return () => {
      live = false
      controller.abort()
    }
  }, [key])

  return state
}
