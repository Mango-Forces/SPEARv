import { useEffect, useState } from "react"
import type { FramePlane } from "../calibrate"
import type { SequenceInfo } from "./useSequence"

export type SectorPlane = { state: "loading" | "ready" | "failed"; plane?: FramePlane; error?: string }

/** Concurrent tile requests per sequence; bounds network fan-out, not data. */
const PARALLEL = 6

/** Calibrated f32 planes for every sector of a ready sequence, assembled from
 * `/values` tiles. Only each sector's data tiles are fetched; the rest of the
 * grid stays NaN (no data). Nothing loads until `info` is set. */
export function useSequencePlanes(info: SequenceInfo | undefined): Map<string, SectorPlane> {
  const [planes, setPlanes] = useState<Map<string, SectorPlane>>(new Map())
  const key = info ? `${info.sequence_id}:${info.display_revision ?? ""}` : ""

  useEffect(() => {
    if (!info || info.state !== "ready") {
      setPlanes(new Map())
      return
    }
    const ac = new AbortController()
    const set = (id: string, value: SectorPlane) => {
      if (ac.signal.aborted) return
      setPlanes((current) => new Map(current).set(id, value))
    }
    setPlanes(new Map(info.sectors.map((sector) => [sector.id, { state: "loading" }])))
    void (async () => {
      for (const sector of info.sectors) {
        if (ac.signal.aborted) return
        try {
          set(sector.id, { state: "ready", plane: await loadPlane(info, sector.id, sector.data_tiles, ac.signal) })
        } catch (error) {
          if (ac.signal.aborted) return
          set(sector.id, { state: "failed", error: error instanceof Error ? error.message : "Frame values could not be loaded." })
        }
      }
    })()
    return () => ac.abort()
  }, [key])

  return planes
}

async function loadPlane(
  info: SequenceInfo,
  sectorId: string,
  tiles: { x: number; y: number }[],
  signal: AbortSignal,
): Promise<FramePlane> {
  const { width, height, tile_size: size } = info
  const values = new Float32Array(width * height).fill(Number.NaN)
  for (let start = 0; start < tiles.length; start += PARALLEL) {
    await Promise.all(
      tiles.slice(start, start + PARALLEL).map(async ({ x, y }) => {
        const url = `/api/v1/sequences/${encodeURIComponent(info.sequence_id)}/sectors/${encodeURIComponent(sectorId)}/values/${x}/${y}`
        const response = await fetch(url, { signal })
        if (!response.ok) throw new Error(`Values tile ${x},${y} failed (${response.status}).`)
        const tile = new Float32Array(await response.arrayBuffer())
        const x0 = x * size
        const y0 = y * size
        const rows = Math.min(size, height - y0)
        const cols = Math.min(size, width - x0)
        for (let row = 0; row < rows; row++) {
          values.set(tile.subarray(row * size, row * size + cols), (y0 + row) * width + x0)
        }
      }),
    )
  }
  return { width, height, values }
}
