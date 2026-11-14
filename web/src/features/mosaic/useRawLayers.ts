import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { normalizeDisplay, resolveDisplay, type DisplayState } from "../../url-state/display"
import { RawClient, serverGate, type RawSnapshot } from "./raw-client"
import { useColorDisplay, type ColorDisplayView } from "./useColorDisplay"
import type { MosaicState } from "./useMosaic"
export type { RenderGate, RawSessionState, RawMetadata, ResolvedDisplay } from "./raw-protocol"

/** One stable Color cancellation: this hook has no local Color worker to stop. */
const ignoreColorCancel = () => undefined

export type RawLayersView = {
  state: RawSnapshot["state"]
  metadata: RawSnapshot["metadata"]
  gate: RawSnapshot["gate"]
  resolved: ReturnType<typeof resolveDisplay>
  start: () => void
  cancel: () => void
  retry: () => void
  colorDisplay?: ColorDisplayView
}

/** Composite keeps its opt-in CPU raw layers; Color consumes server revisions. */
export function useRawLayers(mosaic: MosaicState, display: DisplayState, enabled = true, colorHint = false, viewKey = ""): RawLayersView {
  const [snapshot, setSnapshot] = useState<RawSnapshot>({ state: { phase: "idle" }, metadata: null, gate: serverGate(false) })
  const client = useRef<RawClient | null>(null)
  const owner = useRef<HTMLCanvasElement | null>(null)
  const normalized = normalizeDisplay(display)
  const latestDisplay = useRef(normalized)
  latestDisplay.current = normalized
  const displayKey = JSON.stringify(normalized)
  const canvas = mosaic.mosaic?.canvas
  const info = mosaic.mosaic?.info
  const channels = mosaic.status?.channels_total.length
  const color = colorHint || channels === 3
  const final = mosaic.final && mosaic.status?.state === "done" && (mosaic.finalRevision ?? 0) > 0
  const colorDisplay = useColorDisplay(mosaic, normalized, enabled && color, viewKey)
  const pendingGate = serverGate(false)

  useEffect(() => {
    setSnapshot({ state: { phase: "idle" }, metadata: null, gate: final && !color ? serverGate(true) : pendingGate })
    if (!enabled || !final || !canvas || !info || color) return
    const current = new RawClient(canvas, { id: info.id, width: info.width, height: info.height, channels }, latestDisplay.current, setSnapshot)
    client.current = current
    owner.current = canvas
    void current.prepare()
    return () => {
      current.dispose()
      if (client.current === current) { client.current = null; owner.current = null }
    }
  }, [canvas, info, channels, final, color, enabled, mosaic.finalRevision])

  // A local Composite edit closes export before the browser paints that edit.
  useLayoutEffect(() => {
    if (!color) client.current?.setDisplay(latestDisplay.current)
  }, [displayKey, color])

  const current = enabled && owner.current === canvas
  const gate = final ? (current ? snapshot.gate : pendingGate) : pendingGate
  useLayoutEffect(() => {
    if (!canvas || color) return
    canvas.dataset.completedRevision = String(gate.completedRevision)
    canvas.dataset.remapBusy = String(gate.remapBusy)
  }, [canvas, color, gate.completedRevision, gate.remapBusy])
  const start = useCallback(() => client.current?.start(), [])
  const cancel = useCallback(() => client.current?.cancel(), [])
  const retry = useCallback(() => client.current?.retry(), [])

  if (color) {
    const manifest = colorDisplay.manifest
    const mappedState: RawSnapshot["state"] = colorDisplay.phase === "rendering"
      ? { phase: "loading", loadedTiles: colorDisplay.completedRows, totalTiles: colorDisplay.totalRows }
      : colorDisplay.phase === "ready"
        ? { phase: "ready" }
        : colorDisplay.phase === "failed" || colorDisplay.phase === "expired"
          ? { phase: "unavailable", reason: colorDisplay.error ?? "The server display is unavailable." }
          : { phase: "idle" }
    return {
      state: mappedState,
      metadata: null,
      gate: colorDisplay.gate,
      resolved: resolveDisplay(normalized, manifest ? { source_recipe: { mode: "color" }, channels: manifest.channels } : null),
      start: colorDisplay.retry,
      cancel: ignoreColorCancel,
      retry: colorDisplay.retry,
      colorDisplay,
    }
  }

  const metadata = current ? snapshot.metadata : null
  return {
    ...snapshot,
    state: current ? snapshot.state : { phase: "idle" },
    metadata,
    gate,
    resolved: resolveDisplay(normalized, metadata),
    start,
    cancel,
    retry,
  }
}
