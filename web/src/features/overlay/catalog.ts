import { formatNumber } from "../../url-state/position"

/** Catalog marker layer. The server projects each row to PNG pixels with
 * `sky_to_png_pixel`, the inverse of the click grid, so React draws the
 * returned x,y directly and never redoes sky math. */

export type CatalogSource = "gaia" | "wise"

export const CATALOG_SOURCES: CatalogSource[] = ["gaia", "wise"]

export type CatalogRow = {
  id: string
  ra: number
  dec: number
  mag: number | null
  /** PNG pixel, top-left origin. Integers are pixel corners; centres are +.5. */
  x: number
  y: number
}

export type CatalogBody = {
  source: CatalogSource
  epoch_mjd: number
  truncated: boolean
  rows: CatalogRow[]
}

export type CatalogQuery = {
  ra: number
  dec: number
  fovL: number
  fovH: number
  /** Image epoch for proper-motion correction. Omitted when the view has no dated visit yet. */
  epochMjd?: number
}

/** A 404 means the catalog has no rows for this view, or is not installed.
 * That is an empty layer, not a failed request. */
export class CatalogNoDataError extends Error {
  constructor(source: CatalogSource) {
    super(`${source} has no data for this view`)
    this.name = "CatalogNoDataError"
  }
}

/** Plain fetch, not the generated client: the catalog route is specified by
 * contract (`GET /api/v1/catalog/{source}?ra&dec&fov_l&fov_h&epoch_mjd`) and the
 * generated bindings do not have it yet. */
export async function fetchCatalog(source: CatalogSource, query: CatalogQuery, signal: AbortSignal): Promise<CatalogBody> {
  const params = new URLSearchParams({
    ra: formatNumber(query.ra),
    dec: formatNumber(query.dec),
    fov_l: formatNumber(query.fovL),
    fov_h: formatNumber(query.fovH),
  })
  if (query.epochMjd !== undefined && Number.isFinite(query.epochMjd)) {
    params.set("epoch_mjd", formatNumber(query.epochMjd))
  }
  const label = source === "gaia" ? "Gaia" : "WISE"
  const target = `/api/v1/catalog/${source}?${params.toString()}`
  let response: Response
  try {
    response = await fetch(target, { signal })
  } catch (error) {
    if (signal.aborted) throw error
    throw new Error(`${label} markers could not load: ${error instanceof Error ? error.message : "network error"}`)
  }
  if (response.status === 404) throw new CatalogNoDataError(source)
  if (!response.ok) {
    let detail = ""
    try {
      const body = (await response.json()) as { error?: string }
      if (body && typeof body.error === "string") detail = `: ${body.error}`
    } catch {
      // A proxy or gateway error page is not JSON; the status is the message.
    }
    throw new Error(`${label} markers unavailable (HTTP ${response.status})${detail}`)
  }
  return (await response.json()) as CatalogBody
}
