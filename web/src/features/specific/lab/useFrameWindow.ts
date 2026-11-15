import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react"
import { createDisplayCanvas, displaySize, paintNoData, tileDestination } from "./display"
import { DECODED_PIXEL_BUDGET, LOADS_IN_FLIGHT, fitsBudget, planWindow, type FrameStatus } from "./playback"
import type { Prepared, SequenceInfo } from "./useSequence"

type FrameLoader = (info: SequenceInfo, sectorId: string, signal: AbortSignal) => Promise<Blob[]>
type Store = {
  decoded: Map<string, HTMLCanvasElement>
  /** Active failures for frames currently in the window. Leaving the window
   * drops the entry so a later visit retries. */
  failed: Map<string, string>
  /** Last failure reason for this prepared sequence. Retained across display
   * switches and window eviction until a retry succeeds or the sequence is
   * replaced by Rebuild/new preparation. */
  lastFailure: Map<string, string>
  loading: Map<string, AbortController>
}

/** Decoded frame canvases for a rolling window of one prepared sequence (see
 * planWindow). Frames load ahead in the background, so a playback tick only swaps
 * an already decoded canvas. The last shown frame stays visible until the
 * requested one is decoded. Loading fetches only engine frame tiles.
 *
 * `enabled` closes the window without discarding the prepared sequence or its
 * failure history, so switching to a preview and back retries from the same
 * sequence state. */
export function useFrameWindow(prepared: Prepared | undefined, order: string[], current: string | undefined, loadFrame: FrameLoader, enabled = true) {
  const sequenceId = prepared?.sequenceId
  const store = useMemo<Store>(() => ({ decoded: new Map(), failed: new Map(), lastFailure: new Map(), loading: new Map() }), [sequenceId])
  const budget = useMemo(readBudget, [sequenceId])
  const [version, setVersion] = useState(0)
  const loader = useRef(loadFrame)
  loader.current = loadFrame
  const index = current ? order.indexOf(current) : -1
  const size = prepared ? displaySize(prepared.info.width, prepared.info.height) : null
  const framePixels = size ? size.width * size.height : 0
  const active = !!prepared && enabled
  const wanted = JSON.stringify(active && index >= 0 ? planWindow(order, index, framePixels, budget) : [])
  const wantedRef = useRef<string[]>([])
  const shown = useRef<{ store: Store; id: string; canvas: HTMLCanvasElement }>(undefined)

  // A new sequence, or unmounting, releases every canvas, load and failure.
  useEffect(() => () => {
    store.loading.forEach((controller) => controller.abort())
    store.loading.clear()
    store.decoded.clear()
    store.failed.clear()
    store.lastFailure.clear()
  }, [store])

  useEffect(() => {
    const ids: string[] = JSON.parse(wanted)
    wantedRef.current = ids
    let evicted = false
    for (const id of [...store.decoded.keys()]) if (!ids.includes(id)) { store.decoded.delete(id); evicted = true }
    for (const [id, controller] of [...store.loading]) if (!ids.includes(id)) { controller.abort(); store.loading.delete(id) }
    // The active failure leaves with the window so re-entering retries; the
    // retained reason in lastFailure keeps every label truthful meanwhile.
    for (const id of [...store.failed.keys()]) if (!ids.includes(id)) store.failed.delete(id)
    if (evicted) setVersion((value) => value + 1)
    if (!prepared || !enabled) return
    pump()

    function pump() {
      for (const id of wantedRef.current) {
        if (store.loading.size >= LOADS_IN_FLIGHT) return
        if (store.decoded.has(id) || store.failed.has(id) || store.loading.has(id)) continue
        const controller = new AbortController()
        store.loading.set(id, controller)
        void decodeFrame(prepared!.info, id, controller.signal, loader.current).then(
          (canvas) => {
            if (controller.signal.aborted) return
            store.decoded.set(id, canvas)
            store.lastFailure.delete(id)
          },
          (cause: unknown) => {
            if (controller.signal.aborted) return
            const message = cause instanceof Error ? cause.message : "Frame tiles could not be loaded."
            store.failed.set(id, message)
            store.lastFailure.set(id, message)
          },
        ).finally(() => {
          if (controller.signal.aborted) return
          store.loading.delete(id)
          setVersion((value) => value + 1)
          pump()
        })
      }
    }
  }, [store, prepared, enabled, wanted])

  const decoded = current ? store.decoded.get(current) : undefined
  // Keep the last shown frame of this sequence on screen while the next decodes
  // or fails. A failed requested frame never replaces it; the workspace labels
  // the retained canvas as the last good frame.
  const display = decoded && current ? { store, id: current, canvas: decoded }
    : shown.current?.store === store ? shown.current : undefined
  useLayoutEffect(() => { shown.current = display }, [display?.canvas])

  const error = current && !decoded ? store.lastFailure.get(current) : undefined
  return {
    canvas: display?.canvas,
    shownId: display?.id,
    buffering: !!prepared && enabled && !!current && !decoded && !error,
    decoded: store.decoded.size,
    allFit: fitsBudget(order.length, framePixels, budget),
    error,
    version,
    status: (id: string | undefined): FrameStatus => !id ? "pending" : store.decoded.has(id) ? "decoded" : store.failed.has(id) || store.lastFailure.has(id) ? "failed" : "pending",
    reason: (id: string | undefined) => id ? store.lastFailure.get(id) : undefined,
  }
}

async function decodeFrame(info: SequenceInfo, id: string, signal: AbortSignal, load: FrameLoader) {
  const tiles = await load(info, id, signal)
  const sector = info.sectors.find((item) => item.id === id)
  if (!sector) throw new Error(`Sector ${id} is not part of this sequence.`)
  const surface = createDisplayCanvas(info.width, info.height)
  const grid = { width: info.width, height: info.height }
  for (const [offset, blob] of tiles.entries()) {
    const tile = sector.data_tiles[offset]
    if (!tile) break
    const image = await createImageBitmap(blob)
    if (signal.aborted) { image.close(); throw new DOMException("Aborted", "AbortError") }
    const rect = tileDestination(tile.x, tile.y, info.tile_size, grid, surface.size)
    surface.context.drawImage(image, 0, 0, rect.sourceWidth, rect.sourceHeight, rect.x, rect.y, rect.width, rect.height)
    image.close()
  }
  paintNoData(surface.canvas)
  return surface.canvas
}

/** Test hook: `localStorage["spearv.decodedPixelBudget"]` lowers the memory bound
 * to exercise the rolling window on small regions. Read once per sequence. */
function readBudget() {
  const value = Number(globalThis.localStorage?.getItem("spearv.decodedPixelBudget"))
  return Number.isFinite(value) && value > 0 ? value : DECODED_PIXEL_BUDGET
}
