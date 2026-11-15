import { useEffect, useRef, useState } from "react"
import { client } from "../../../api"
import type { components } from "../../../bindings/schema"
import { createDisplayCanvas, paintNoData, tileDestination } from "./display"
import { apiRegion, type Region, type Snippet } from "./state"

export type SnippetPreview = { canvas: HTMLCanvasElement; info: components["schemas"]["SnippetPreviewInfo"] }

export function useSnippetPreview(region: Region, active: boolean) {
  const key = JSON.stringify([region.ra, region.dec, region.fovL, region.fovH])
  const controller = useRef<AbortController | undefined>(undefined)
  const handle = useRef<string | undefined>(undefined)
  const [result, setResult] = useState<{ key: string; preview: SnippetPreview }>()
  const [error, setError] = useState<string>()
  const [loading, setLoading] = useState(false)
  const [selected, setSelected] = useState<Snippet>()

  useEffect(() => {
    controller.current?.abort()
    setLoading(false)
    setSelected(undefined)
    setResult(undefined)
    setError(undefined)
    return () => release()
  }, [key])
  useEffect(() => {
    if (!active) { controller.current?.abort(); release(); setLoading(false) }
    return () => controller.current?.abort()
  }, [active])

  async function preview(snippet: Snippet) {
    controller.current?.abort()
    release()
    const ac = new AbortController()
    controller.current = ac
    setSelected(snippet)
    setLoading(true)
    setResult(undefined)
    setError(undefined)
    try {
      const response = await client.POST("/api/v1/snippets/preview", { body: { region: apiRegion(region), snippet_ref: snippet.ref, mask: true } })
      if (ac.signal.aborted && response.data) void client.DELETE("/api/v1/snippets/previews/{preview_id}", { params: { path: { preview_id: response.data.preview_id } } }).catch(() => undefined)
      if (ac.signal.aborted) return
      if (!response.data) throw new Error(response.error?.error ?? `Preview failed (${response.response.status}).`)
      const info = response.data
      handle.current = info.preview_id
      const surface = createDisplayCanvas(info.width, info.height)
      const grid = { width: info.width, height: info.height }
      // Only tiles that hold finite preview data are fetched; bounded batches limit
      // in-flight decodes without changing the selected pixels.
      for (let start = 0; start < info.data_tiles.length; start += 6) {
        await Promise.all(info.data_tiles.slice(start, start + 6).map(async ({ x, y }) => {
          const tile = await client.GET("/api/v1/snippets/previews/{preview_id}/tiles/{x}/{y}", { params: { path: { preview_id: info.preview_id, x, y } }, parseAs: "blob", signal: ac.signal })
          if (!tile.data) throw new Error(`Preview tile ${x},${y} failed (${tile.response.status}).`)
          const image = await createImageBitmap(tile.data)
          if (!ac.signal.aborted) {
            const rect = tileDestination(x, y, info.tile_size, grid, surface.size)
            surface.context.drawImage(image, 0, 0, rect.sourceWidth, rect.sourceHeight, rect.x, rect.y, rect.width, rect.height)
          }
          image.close()
        }))
      }
      if (!ac.signal.aborted) {
        paintNoData(surface.canvas)
        setResult({ key, preview: { canvas: surface.canvas, info } })
      }
    } catch (cause) {
      if (!ac.signal.aborted) { ac.abort(); setError(cause instanceof Error ? cause.message : "Preview failed.") }
    } finally {
      if (controller.current === ac) { release(); setLoading(false) }
    }
  }

  function release() {
    if (!handle.current) return
    const preview_id = handle.current
    handle.current = undefined
    void client.DELETE("/api/v1/snippets/previews/{preview_id}", { params: { path: { preview_id } }, keepalive: true }).catch(() => undefined)
  }

  return { preview, selected, loading, error, result: result?.key === key ? result.preview : undefined }
}
