import { useCallback, useEffect, useLayoutEffect, useRef, useState } from "react"
import { colorDisplayKey } from "./server-display-protocol"
import { ServerDisplayClient, type ColorDisplaySnapshot } from "./server-display-client"
import type { DisplayState } from "../../url-state/display"
import type { MosaicState } from "./useMosaic"
import { serverGate } from "./raw-client"

type BoundSnapshot = ColorDisplaySnapshot & { viewKey: string }
export type ColorDisplayView = BoundSnapshot & { retry: () => void }

export function useColorDisplay(mosaic: MosaicState, display: DisplayState, enabled: boolean, viewKey: string, onLostJob?: () => void): ColorDisplayView {
  const client = useRef<ServerDisplayClient | null>(null)
  const [snapshot, setSnapshot] = useState<BoundSnapshot>()
  const info = mosaic.mosaic?.info
  const final = mosaic.final && mosaic.status?.state === "done" && (mosaic.finalRevision ?? 0) > 0
  // A render can briefly pair the new view with the old mosaic state. Do not
  // start a new display client for that old job under the new view identity.
  const staleForView = !!snapshot && !!info && snapshot.viewKey !== viewKey && snapshot.jobId === info.id
  const settingsKey = colorDisplayKey(display)
  const jobId = info?.id ?? ""
  const waiting: BoundSnapshot = {
    phase: enabled && final && info ? "creating" : "queued",
    settingsKey,
    jobId,
    viewKey,
    completedRows: 0,
    totalRows: 0,
    gate: { ...serverGate(false), label: enabled && final && info ? "Sending updated display settings to the server…" : "Settings queued while science builds" },
  }

  useEffect(() => {
    if (!enabled || !final || !info) {
      setSnapshot(waiting)
      return
    }
    if (staleForView) return
    const current = new ServerDisplayClient(
      { jobId: info.id, width: info.width, height: info.height },
      display,
      (next) => setSnapshot({ ...next, viewKey }),
      undefined,
      undefined,
      onLostJob,
    )
    client.current = current
    current.prepare()
    return () => {
      current.dispose()
      if (client.current === current) client.current = null
    }
  }, [enabled, final, info?.id, info?.width, info?.height, mosaic.finalRevision, viewKey, onLostJob])

  useLayoutEffect(() => {
    client.current?.setDisplay(display)
  }, [settingsKey])

  const retry = useCallback(() => client.current?.retry(), [])
  const current = enabled && final && snapshot?.viewKey === viewKey && snapshot?.jobId === jobId && snapshot?.settingsKey === settingsKey
  return { ...(current && snapshot ? snapshot : waiting), retry }
}
