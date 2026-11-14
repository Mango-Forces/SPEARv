import { useEffect, useState } from "react"
import { CatalogNoDataError, fetchCatalog, type CatalogBody, type CatalogQuery, type CatalogSource } from "./catalog"

export type CatalogState = {
  status: "idle" | "loading" | "ready" | "empty" | "failed"
  body?: CatalogBody
  error?: string
}

/** Loads one catalog for the current view while its layer is on. The query
 * carries ra/dec/FOV-L/FOV-H (and the image epoch when known); opacity stays in the
 * page URL only and is never sent to the server. */
export function useCatalog(source: CatalogSource, query: CatalogQuery, enabled: boolean): CatalogState {
  const [state, setState] = useState<CatalogState>({ status: enabled ? "loading" : "idle" })
  const { ra, dec, fovL, fovH, epochMjd } = query

  useEffect(() => {
    if (!enabled) {
      setState({ status: "idle" })
      return
    }
    const controller = new AbortController()
    setState({ status: "loading" })
    fetchCatalog(source, { ra, dec, fovL, fovH, epochMjd }, controller.signal).then(
      (body) => {
        if (!controller.signal.aborted) setState({ status: "ready", body })
      },
      (error: unknown) => {
        if (controller.signal.aborted) return
        if (error instanceof CatalogNoDataError) {
          setState({ status: "empty" })
          return
        }
        setState({ status: "failed", error: error instanceof Error ? error.message : "catalog request failed" })
      },
    )
    return () => controller.abort()
  }, [source, enabled, ra, dec, fovL, fovH, epochMjd])

  return state
}
