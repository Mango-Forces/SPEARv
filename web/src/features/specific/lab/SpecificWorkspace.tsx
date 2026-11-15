import { useEffect, useLayoutEffect, useReducer, useRef, useState, type ReactNode } from "react"
import { EdgeSidebar } from "../../shell/edge-sidebar"
import { ImageViewport } from "../../viewport/ImageViewport"
import { BoundsCard } from "./BoundsCard"
import { formatBands, type Bounds } from "../bounds"
import { formatUtcClock, formatUtcDate } from "../date"
import { FrameConsole, type ConsoleFrame } from "../FrameConsole"
import { useSequencePlanes } from "./useSequencePlanes"
import type { SnippetSort } from "./useObservations"
import type { View } from "../../../url-state/view"
import { SectorsEditor } from "./SectorsEditor"
import { ObservationTable } from "./ObservationTable"
import { SectorTimeline, type TransportState } from "./SectorTimeline"
import { BASE, client } from "../../../api"
import { initialSectorState, sectorMembers, sectorReducer, type Region, type SectorState, type Snippet } from "./state"
import { useObservations } from "./useObservations"
import { useSnippetPreview } from "./useSnippetPreview"
import { useSequence, type SequenceInfo } from "./useSequence"
import { useFrameWindow } from "./useFrameWindow"
import { CATALOG_SOURCES, fetchCatalog, type CatalogSource } from "../../overlay/catalog"
import { Marker, skyAtPixel, type CatalogStatusReport } from "../../overlay/ViewerFrame"
import { useCatalog, type CatalogState } from "../../overlay/useCatalog"
import type { SpectrumPointTarget } from "../../spectrum-panel"
import { tickDecision } from "./playback"
import "./lab.css"

type Display = "snippet" | "sector" | "sequence" | "compare"

/** Most exposures one compare holds; each becomes its own frame. */
const COMPARE_LIMIT = 8

type QuickState =
  | { phase: "listing"; band: number }
  | { phase: "building"; band: number; frames: number }
  | { phase: "ready"; band: number; frames: number }
  | { phase: "failed"; band: number; message: string }

/** The exposure lab: list every exposure of a field with its quality, compose
 * sectors by hand, and play them as one calibrated sequence. Nothing is listed
 * until `armed`, which only Find sets. */
export function SpecificWorkspace(props: {
  region: Region
  /** The page view, for catalog layers and pixel picks on the frame console. */
  view: View
  active: boolean
  armed: boolean
  /** True while discovery or a quick video build is running, for Find/Stop. */
  onBusy?: (busy: boolean) => void
  /** Rendered at the top of the editor, above everything else. */
  switcher?: ReactNode
  releases: string[]
  bounds: Bounds
  boundsError?: string
  onBoundsChange: (bounds: Bounds) => void
  onViewChange?: (next: { ra: number; dec: number; bounds: Bounds }) => void
  /** The quick video re-centres and narrows bands as one user action, so the
   * host commits that view and keeps the run armed instead of asking for Find. */
  onQuickView?: (next: { ra: number; dec: number; bounds: Bounds }) => void
  editorOpen: boolean
  onEditorOpenChange: (open: boolean) => void
  gaia: number | null
  wise: number | null
  onCatalogStatus?: (report: CatalogStatusReport) => void
  onPoint?: (point: SpectrumPointTarget | null) => void
  onFrameProvenance?: (frame: { release: string; epochMjd?: number } | undefined) => void
}) {
  const [state, dispatch] = useReducer(sectorReducer, props.region, initialSectorState)
  const [boundsRevision, setBoundsRevision] = useState(0)
  const [search, setSearch] = useState("")
  const [sort, setSort] = useState<SnippetSort>("time")
  const [quality, setQuality] = useState<"any" | "pass">("any")
  const [picked, setPicked] = useState<string[]>([])
  const [compareIndex, setCompareIndex] = useState(0)
  const [comparePlaying, setComparePlaying] = useState(false)
  const [compareLabels, setCompareLabels] = useState<Map<string, string>>(new Map())
  const [direction, setDirection] = useState<"asc" | "desc">("asc")
  const [offset, setOffset] = useState(0)
  const [retainIssue, setRetainIssue] = useState<{ missing: string[]; message?: string }>()
  const [display, setDisplay] = useState<Display>()
  const [frame, setFrame] = useState(0)
  const [playing, setPlaying] = useState(false)
  const [rate, setRate] = useState(2)
  const [loop, setLoop] = useState(true)
  const [discarded, setDiscarded] = useState(false)
  const [buffering, setBuffering] = useState<number>()
  const [timelineOpen, setTimelineOpen] = useState(false)
  const previousSectorCount = useRef(0)
  // The control keeps rendering while discovery is paused; only the query is
  // gated. An invalid link must never hide the workspace or query default bounds.
  const queryEnabled = props.active && props.armed && !props.boundsError
  const [quick, setQuick] = useState<QuickState>()
  const [quickBand, setQuickBand] = useState<"auto" | number>("auto")
  const [quickActive, setQuickActive] = useState(false)
  const [quickCentre, setQuickCentre] = useState<string>()
  const quickGroups = useRef<Snippet[][]>([])
  const quickBuilding = useRef(false)
  const quickTrimmed = useRef(false)
  const observations = useObservations(props.region, props.bounds, queryEnabled, search, sort, direction, offset, boundsRevision, quickActive ? 8 : undefined, quickActive ? 250 : 50, quality)
  const preview = useSnippetPreview(props.region, props.active)
  const busy = queryEnabled && (observations.loading || quick?.phase === "listing" || quick?.phase === "building")
  useEffect(() => {
    props.onBusy?.(busy)
  }, [busy, props.onBusy])
  const sectorPreview = useSequence(props.region)
  const playback = useSequence(props.region)
  const compare = useSequence(props.region)
  const canvas = useRef<HTMLCanvasElement>(null)
  const ownedRefs = JSON.stringify([...state.ownerBySnippet.keys()].map((id) => state.snippets.get(id)!.ref))
  useEffect(() => {
    const refs: string[] = JSON.parse(ownedRefs)
    if (!refs.length) { setRetainIssue(undefined); return }
    const ac = new AbortController()
    // The renew contract may be 204 (all live), 200 with an explicit missing list,
    // or 404 with the missing IDs. Parse tolerantly so a partial loss never hides
    // the refs the server did renew.
    const renew = () => void renewLeases(refs, ac.signal).then((issue) => {
      if (!ac.signal.aborted) setRetainIssue(issue)
    }).catch(() => undefined)
    renew()
    const timer = setInterval(renew, 5 * 60_000)
    return () => { ac.abort(); clearInterval(timer) }
  }, [ownedRefs])
  const retainNotice = retainIssue && (retainIssue.missing.length ? expiredMembers(retainIssue.missing, state) : retainIssue.message)

  // Membership identity only: order, names, search and bounds do not invalidate frames.
  const compositionKey = JSON.stringify([props.region, [...state.order].sort().map((id) => [id, state.sectors.get(id)?.revision])])
  // Invalidation keeps the prepared provenance for the stale labels but marks the
  // composition discarded. A reversible edit that restores the same key must not
  // make released frames valid again (for example create-empty then delete).
  const preparedValid = !!playback.prepared && playback.prepared.key === compositionKey && !discarded
  const stale = !!playback.prepared && !preparedValid
  // A ready sequence or exposure compare plays on the frame console from
  // calibrated values; the decoded-PNG window is only for single previews.
  const consoleSequence = display === "compare" ? compare.prepared : display === "sequence" && preparedValid ? playback.prepared : undefined
  const consoleActive = !!consoleSequence
  const planes = useSequencePlanes(consoleSequence?.info)
  const count = state.order.length
  useEffect(() => {
    if (count > 0 && previousSectorCount.current === 0) setTimelineOpen(true)
    previousSectorCount.current = count
  }, [count])
  const current = Math.min(frame, Math.max(0, count - 1))
  const currentId = state.order[current]
  useEffect(() => {
    playback.invalidateUnless(compositionKey)
    if (playback.prepared && playback.prepared.key !== compositionKey) setDiscarded(true)
  }, [compositionKey])
  // Sector previews are scratch compositions. Release one immediately when any
  // membership composition changes rather than waiting for another preview.
  useEffect(() => sectorPreview.discard(), [compositionKey])
  // Only the visible display decodes frames. The inactive window releases its
  // decoded canvases; both prepared sequences stay available so
  // switching displays reuses the retained server sequence instead of rebuilding.
  const frameWindow = useFrameWindow(
    playback.prepared,
    state.order,
    currentId,
    playback.loadFrame,
    props.active && display === "sequence" && preparedValid && !consoleActive,
  )
  // The frame actually on screen; it lags `current` only while `current` decodes.
  const shownIndex = frameWindow.shownId ? state.order.indexOf(frameWindow.shownId) : -1
  const shownId = shownIndex >= 0 ? frameWindow.shownId : undefined
  const currentFailed = preparedValid && (consoleActive ? planes.get(currentId ?? "")?.state === "failed" : !!frameWindow.error)
  const frameReady = preparedValid && !!frameWindow.canvas && !frameWindow.error
  const validSectorPreview = sectorPreview.prepared?.info.sectors.every((sector) => state.sectors.get(sector.id)?.revision === sector.revision)
  const previewId = sectorPreview.prepared?.info.sectors[0]?.id
  const previewWindow = useFrameWindow(
    sectorPreview.prepared,
    previewId ? [previewId] : [],
    previewId,
    sectorPreview.loadFrame,
    props.active && display === "sector" && !!validSectorPreview && !!previewId,
  )
  // Prepared playback stays reachable while another display is showing; its
  // frame window starts decoding again when the user returns to sequence.
  const consoleReady = planes.get(currentId ?? "")?.state === "ready"
  const playbackReady = preparedValid && !currentFailed && (display !== "sequence" || (consoleActive ? consoleReady : !!frameWindow.canvas))

  useEffect(() => {
    dispatch({ type: "region", region: props.region })
    setOffset(0)
    setDisplay(undefined)
  }, [props.region.ra, props.region.dec, props.region.fovL, props.region.fovH])
  useEffect(() => { dispatch({ type: "register", snippets: observations.rows }) }, [observations.rows])
  // Editing membership, reordering or leaving the mode pauses. Filtering does not.
  useEffect(() => setPlaying(false), [compositionKey, state.order.join("|"), props.active])
  // A snippet or single-sector preview is not the prepared sequence. Pause
  // rather than leave a hidden scheduler "playing" behind another display.
  useEffect(() => { if (display !== "sequence") setPlaying(false) }, [display])
  useEffect(() => { if (display !== "compare") setComparePlaying(false) }, [display])
  // Playback scheduler. It is not torn down by frame changes: it reads the current
  // frame and decode state through refs. A frame is due one period after the
  // previous one was actually drawn (lastSwap); an undecoded current or next frame
  // is buffered with the last canvas kept on screen, then shown as soon as it decodes.
  const lastSwap = useRef(0)
  const frameRef = useRef(current)
  frameRef.current = current
  const orderRef = useRef(state.order)
  orderRef.current = state.order
  const windowRef = useRef(frameWindow)
  windowRef.current = frameWindow
  const wake = useRef<(() => void) | undefined>(undefined)
  const scheduling = playing && preparedValid && display === "sequence" && count >= 2 && !consoleActive
  useEffect(() => {
    if (!scheduling) return
    let timer: ReturnType<typeof setTimeout> | undefined
    let stopped = false
    const period = 1000 / rate
    const step = () => {
      timer = undefined
      wake.current = undefined
      if (stopped) return
      const currentId = orderRef.current[frameRef.current]
      const decoder = windowRef.current
      // The workspace owns the no-skip stop: a failed current frame is never
      // advanced past. It stays selected with its error visible until rebuilt.
      if (decoder.status(currentId) === "failed") { setPlaying(false); setBuffering(undefined); return }
      const decision = tickDecision({ now: performance.now(), lastSwap: lastSwap.current, period, current: frameRef.current, count, loop, status: (index) => decoder.status(orderRef.current[index]) })
      if (decision.kind === "wait") { timer = setTimeout(step, decision.ms); return }
      if (decision.kind === "stop") { setPlaying(false); return }
      if (decision.kind === "buffer") { setBuffering(decision.index); wake.current = step; return }
      // A decoded current frame may not have reached the canvas yet. Keep both the
      // last canvas and the buffering state until the layout effect swaps it in.
      if (decoder.status(currentId) === "decoded" && canvas.current?.dataset.frame !== currentId) {
        setBuffering(frameRef.current)
        timer = setTimeout(step, 16)
        return
      }
      setBuffering(undefined)
      frameRef.current = decision.index
      setFrame(decision.index)
      // The next due time is re-derived from the measured swap time.
      timer = setTimeout(step, period)
    }
    lastSwap.current = performance.now()
    timer = setTimeout(step, period)
    return () => { stopped = true; wake.current = undefined; if (timer) clearTimeout(timer); setBuffering(undefined) }
  }, [scheduling, rate, loop, count, playback.prepared?.sequenceId])
  // A buffered frame is shown as soon as the window finishes decoding it.
  useEffect(() => { wake.current?.() }, [frameWindow.version])
  // The buffering label stays until the frame it names is actually on screen.
  useEffect(() => {
    if (buffering !== undefined && buffering === shownIndex) setBuffering(undefined)
  }, [buffering, shownIndex])
  // A frame whose tiles failed stops playback on that frame; it is never skipped.
  useEffect(() => { if (playing && display === "sequence" && frameWindow.error) setPlaying(false) }, [playing, display, frameWindow.error])

  const shown = display === "snippet" ? preview.result?.canvas
    : display === "sector" && validSectorPreview ? previewWindow.canvas
    : display === "sequence" && preparedValid ? frameWindow.canvas : undefined
  const shownFrame = display === "sequence" && shown ? shownId : undefined
  const shownSector = display === "sequence" && preparedValid && shownId && playback.prepared
    ? playback.prepared.info.sectors.find((sector) => sector.id === shownId)
    : undefined
  const shownNative = display === "snippet" && preview.result
    ? { w: preview.result.info.width, h: preview.result.info.height }
    : display === "sector" && sectorPreview.prepared
      ? { w: sectorPreview.prepared.info.width, h: sectorPreview.prepared.info.height }
      : display === "sequence" && playback.prepared
        ? { w: playback.prepared.info.width, h: playback.prepared.info.height }
        : undefined
  const shownRelease = display === "snippet" && preview.result
    ? preview.result.info.snippet.release
    : display === "sector" && sectorPreview.prepared
      ? sectorPreview.prepared.info.sectors[0]?.releases.join("+")
      : shownSector?.releases.join("+")
  const shownEpoch = display === "snippet" && preview.result
    ? preview.result.info.snippet.mjd
    : display === "sector" && sectorPreview.prepared
      ? sectorPreview.prepared.info.sectors[0]?.mjd_start
      : shownSector?.mjd_start
  const targetMissing = (display === "sequence" && preparedValid && shownSector && !shownSector.target_covered)
    || (display === "sector" && validSectorPreview && sectorPreview.prepared && !sectorPreview.prepared.info.sectors[0]?.target_covered)

  useEffect(() => {
    props.onFrameProvenance?.(shownRelease ? { release: shownRelease, epochMjd: shownEpoch } : undefined)
  }, [shownRelease, shownEpoch, props.onFrameProvenance])

  const pickFrame = (x: number, y: number) => {
    if (!shownNative || !props.onPoint) return
    const sky = skyAtPixel(x, y, shownNative.w, shownNative.h, props.region.ra, props.region.dec)
    props.onPoint({ x, y, ra: sky.ra, dec: sky.dec })
  }
  const viewportResetKey = display === "snippet"
    ? preview.result?.info.preview_id ?? "snippet"
    : display === "sector"
      ? sectorPreview.prepared?.sequenceId ?? "sector"
      : playback.prepared?.sequenceId ?? "sequence"
  useLayoutEffect(() => {
    if (!canvas.current || !shown) return
    if (canvas.current.width !== shown.width) canvas.current.width = shown.width
    if (canvas.current.height !== shown.height) canvas.current.height = shown.height
    const context = canvas.current.getContext("2d")
    context?.clearRect(0, 0, shown.width, shown.height)
    context?.drawImage(shown, 0, 0)
    // The swap time drives the scheduler and is exposed for timing tests.
    lastSwap.current = performance.now()
    canvas.current.dataset.frame = shownFrame ?? ""
  }, [shown])
  const bufferingIndex = buffering ?? (display === "sequence" && preparedValid && frameWindow.buffering && frameReady ? current : undefined)

  const submitted = (ids: string[]) => ids.map((id) => ({ id, revision: state.sectors.get(id)!.revision, snippets: sectorMembers(state, id).map((snippet) => snippet.ref) }))
  const previewSector = (id: string) => {
    setPlaying(false)
    setDisplay("sector")
    void sectorPreview.prepare(JSON.stringify([props.region, id, state.sectors.get(id)!.revision]), submitted([id]))
  }
  const emptySector = state.order.find((id) => sectorMembers(state, id).length === 0)
  const prepareBlocked = count === 0 ? "Create sectors and join snippets first."
    : emptySector ? `${state.sectors.get(emptySector)!.name} is empty; join snippets or delete it before preparing playback.` : undefined
  const decodeFailures = preparedValid
    ? state.order.flatMap((id) => {
      if (consoleActive) return planes.get(id)?.state === "failed" ? [`${state.sectors.get(id)?.name ?? id}: ${planes.get(id)?.error ?? "Frame values could not be loaded."}`] : []
      return frameWindow.status(id) === "failed" ? [`${state.sectors.get(id)?.name ?? id}: ${frameWindow.reason(id) ?? "Frame tiles could not be loaded."}`] : []
    })
    : []
  const buildFailures = playback.progress?.sectors.filter((sector) => sector.state === "failed").map((sector) => `${state.sectors.get(sector.id)?.name ?? sector.id}: ${sector.error}`) ?? []
  const failedFrames = [...new Set([...buildFailures, ...decodeFailures])]
  const frameStates = buildStates(state, playback.progress, playback.loading, preparedValid, stale, playback.prepared?.info, (id) => preparedValid && (consoleActive ? planes.get(id)?.state === "failed" : frameWindow.status(id) === "failed"))
  const showingSequence = display === "sequence" && !!shown
  const transportState: TransportState = playback.loading ? "building"
    : stale ? "stale"
    : currentFailed ? "failed"
    : playback.error ? "failed"
    : preparedValid && (display !== "sequence" || (consoleActive ? consoleReady : !!frameWindow.canvas)) ? "ready"
    : preparedValid ? "decoding"
    : "idle"
  const stepEnabled = preparedValid && !playback.loading && (transportState === "ready" || transportState === "failed")

  const caption = (() => {
    if (display === "snippet" && preview.result) {
      const info = preview.result.info
      return `Exposure · band ${info.snippet.band} · ${humanUtc(info.snippet.utc)} · ${info.snippet.release.toUpperCase()} / D${info.snippet.detector} · ${info.valid_samples} valid samples · ${info.state === "empty" ? "No valid measurements" : "IMAGE − ZODI, strict Spectrum mask, nearest-pixel sampling"} · own scale`
    }
    if (display === "sector" && validSectorPreview && sectorPreview.prepared) {
      const info = sectorPreview.prepared.info
      const sector = info.sectors[0]
      return `${state.sectors.get(sector.id)?.name ?? sector.id} preview · ${frameText(sector)} · own scale ${scale(info)} (playback uses one shared scale)`
    }
    if (display === "sequence" && preparedValid && frameWindow.canvas && shownId && playback.prepared) {
      const info = playback.prepared.info
      const sector = info.sectors.find((item) => item.id === shownId)
      if (!sector) return "Session-local composition · switching modes preserves sectors"
      const lastGood = currentFailed || shownId !== currentId
      const head = lastGood
        ? `Last good: ${state.sectors.get(shownId)?.name ?? shownId} · frame ${shownIndex + 1}/${count}`
        : `${state.sectors.get(shownId)?.name} · frame ${shownIndex + 1}/${count}`
      const failure = currentFailed ? ` · requested frame ${current + 1} failed` : ""
      return `${head}${failure} · ${frameText(sector)} · shared scale ${scale(info)} · display rev ${info.display_revision}`
    }
    return "Session-local composition · switching modes preserves sectors"
  })()
  const errors = [
    display === "snippet" && preview.error,
    display === "sector" && (sectorPreview.error ?? previewWindow.error),
    display === "sequence" && playback.error,
    display === "compare" && compare.error,
  ].filter(Boolean)
  const togglePick = (snippet: Snippet) => setPicked((list) =>
    list.includes(snippet.id) ? list.filter((id) => id !== snippet.id) : list.length >= COMPARE_LIMIT ? list : [...list, snippet.id])
  const startCompare = () => {
    const snippets = picked.flatMap((id) => state.snippets.get(id) ?? [])
    if (snippets.length < 2) return
    const ordered = [...snippets].sort((left, right) => left.mjd - right.mjd)
    const sectors = ordered.map((snippet, at) => ({ id: `exposure-${at + 1}`, revision: 1, snippets: [snippet.ref] }))
    setCompareLabels(new Map(ordered.map((snippet, at) => [`exposure-${at + 1}`, `${String.fromCharCode(65 + picked.indexOf(snippet.id))} · band ${snippet.band}`])))
    setCompareIndex(0)
    setComparePlaying(false)
    setDisplay("compare")
    void compare.prepare(JSON.stringify([props.region, sectors.map((sector) => sector.snippets[0])]), sectors)
  }

  const consoleFrames: ConsoleFrame[] = !consoleSequence ? [] : (display === "compare"
    ? consoleSequence.info.sectors.map((sector) => sector.id)
    : state.order).flatMap((id) => {
    const sector = consoleSequence.info.sectors.find((item) => item.id === id)
    if (!sector) return []
    const loaded = planes.get(id)
    return [{
      id,
      label: display === "compare" ? compareLabels.get(id) ?? id : state.sectors.get(id)?.name ?? id,
      mjd: sector.mjd_start,
      mjd_end: sector.mjd_end,
      state: loaded?.state ?? "loading",
      plane: loaded?.plane,
      error: loaded?.error,
      detail: sectorDetail(sector),
      epoch: true,
    }]
  })
  const consoleBands = consoleSequence ? [...new Set(consoleSequence.info.sectors.flatMap((sector) => sector.bands))].sort((left, right) => left - right) : []
  const consoleLoaded = consoleFrames.filter((item) => item.state !== "loading").length

  const applyBounds = (next: Bounds) => {
    setOffset(0)
    setBoundsRevision((value) => value + 1)
    props.onBoundsChange(next)
  }

  const startQuick = async () => {
    if (quick?.phase === "listing" || quick?.phase === "building") return
    quickBuilding.current = false
    quickTrimmed.current = false
    quickGroups.current = []
    setPlaying(false)
    setDiscarded(false)
    setQuickCentre(undefined)
    setQuick({ phase: "listing", band: 0 })
    const video = await pickQuickVideo(props.region, props.bounds.releases)
    const target = await resolveQuickTarget(props.region, video.epochMjd)
    const band = quickBand === "auto" ? video.band : quickBand
    const bounds = { ...props.bounds, fromUtc: null, toUtc: null, bands: [band] }
    setQuick({ phase: "listing", band })
    setQuickActive(true)
    setOffset(0)
    setBoundsRevision((value) => value + 1)
    if (target) setQuickCentre("Centred on WISE target.")
    const next = { ra: target?.ra ?? props.region.ra, dec: target?.dec ?? props.region.dec, bounds }
    if (props.onQuickView) {
      props.onQuickView(next)
      return
    }
    if (target && props.onViewChange) {
      props.onViewChange(next)
      return
    }
    applyBounds(bounds)
  }

  useEffect(() => {
    if (quick?.phase !== "listing" || quick.band === 0 || quickBuilding.current) return
    if (observations.error) {
      quickBuilding.current = true
      setQuickActive(false)
      setQuick({ phase: "failed", band: quick.band, message: observations.error })
      return
    }
    if (observations.state !== "done") return
    const groups = quickGroupsFromRows(observations.rows)
    if (groups.length < 2) {
      quickBuilding.current = true
      setQuickActive(false)
      setQuick({ phase: "failed", band: quick.band, message: "Only one visit covers this field." })
      return
    }
    quickBuilding.current = true
    quickGroups.current = groups
    const submitted = quickSubmitted(groups)
    const key = JSON.stringify([props.region, submitted.map((sector) => [sector.id, sector.revision]).sort()])
    dispatch({ type: "compose", groups })
    setQuick({ phase: "building", band: quick.band, frames: groups.length })
    setDisplay("sequence")
    setFrame(0)
    setPlaying(false)
    void playback.prepare(key, submitted)
  }, [quick?.phase, observations.state, observations.rows, observations.error])

  useEffect(() => {
    if (quick?.phase !== "building") return
    if (playback.error) {
      quickBuilding.current = false
      setQuickActive(false)
      setQuick({ phase: "failed", band: quick.band, message: playback.error })
      return
    }
    if (playback.loading || !playback.prepared || playback.prepared.key !== compositionKey) return
    const sectors = playback.prepared.info.sectors
    const covered = sectors.filter((sector) => sector.target_coverage >= 0.2)
    if (covered.length < 2) {
      quickBuilding.current = false
      setQuickActive(false)
      setQuick({ phase: "failed", band: quick.band, message: "No two frames cover the target centre." })
      return
    }
    if (covered.length < sectors.length && !quickTrimmed.current) {
      quickTrimmed.current = true
      const groups = quickGroups.current.filter((_, index) => covered.some((sector) => sector.id === `sector-${index + 1}`))
      quickGroups.current = groups
      const submitted = quickSubmitted(groups)
      const key = JSON.stringify([props.region, submitted.map((sector) => [sector.id, sector.revision]).sort()])
      dispatch({ type: "compose", groups })
      setQuick({ phase: "building", band: quick.band, frames: groups.length })
      void playback.prepare(key, submitted)
      return
    }
    quickBuilding.current = false
    setQuick({ phase: "ready", band: quick.band, frames: sectors.length })
    setQuickActive(false)
    setDisplay("sequence")
    setFrame(0)
    setDiscarded(false)
    setPlaying(true)
  }, [quick?.phase, playback.prepared, playback.loading, playback.error, compositionKey])

  const heading = (() => {
    if (display === "snippet") {
      if (preview.loading) return "Reading exact snippet…"
      if (preview.error) return "Snippet preview failed."
      return "Snippet preview unavailable."
    }
    if (display === "sector") {
      if (sectorPreview.loading) return "Building sector…"
      if (sectorPreview.error) return "Sector preview failed."
      if (previewWindow.error) return "Sector frame failed."
      if (sectorPreview.prepared && !validSectorPreview) return "Sector changed. Preview it again."
      if (validSectorPreview) return "Decoding sector preview…"
      return "Sector preview unavailable."
    }
    if (display === "compare") {
      if (compare.loading) return "Building exposure compare…"
      if (compare.error) return "Exposure compare failed."
      return "Exposure compare unavailable."
    }
    if (display === "sequence") {
      if (playback.loading) return "Building sector frames…"
      if (stale) return "Composition changed. Prepare playback again."
      if (currentFailed) return `Frame ${current + 1} failed.`
      if (playback.error) return "Playback build failed."
      if (preparedValid) return "Buffering from downloaded frame tiles…"
      return "Playback is not prepared."
    }
    return "No video yet."
  })()

  return <div className="specific-workspace" hidden={!props.active}>
    <div className="specific-main">
      <EdgeSidebar id="specific-sidebar" label="Sector editor" side="left" width="editor" layout="dock" open={props.editorOpen} onOpenChange={props.onEditorOpenChange}>
        <div className="specific-editor-content">
        {props.switcher}
        {retainNotice && <p role="status" className="specific-notice">{retainNotice}</p>}
        {props.boundsError && <p role="alert" className="specific-notice">{props.boundsError}</p>}
        <details className="specific-section quick-video" open>
          <summary>Quick video</summary>
          <div className="specific-actions">
            <button type="button" className="quick-video-start" onClick={() => void startQuick()} disabled={!props.armed || quick?.phase === "listing" || quick?.phase === "building"} title={props.armed ? undefined : "Press Find first"}>Build video</button>
            <label className="specific-inline">Band<select aria-label="Quick video band" value={String(quickBand)} onChange={(event) => setQuickBand(event.target.value === "auto" ? "auto" : Number(event.target.value))}>
              <option value="auto">Auto</option>
              <option value="90">90 · 4.7 µm</option>
              <option value="101">101 · 5.0 µm</option>
              <option value="75">75 · 4.1 µm</option>
              <option value="41">41 · 1.9 µm</option>
              <option value="57">57 · 3.2 µm</option>
            </select></label>
          </div>
          <p role="status" className="specific-note">{quickText(quick, observations.elapsedMs)}</p>
          {quickCentre && <p className="specific-note">{quickCentre}</p>}
        </details>
        <BoundsCard bounds={props.bounds} releases={props.releases} boundsError={props.boundsError} onApply={applyBounds} />
        <SectorsEditor state={state} bounds={props.bounds} search={search} dispatch={dispatch} buildStates={frameStates}
          onPreview={(snippet) => { setDisplay("snippet"); void preview.preview(snippet) }}
          onPreviewSector={previewSector} />
        <ObservationTable {...observations} discoveryState={observations.state} state={state} pageSize={observations.sampled ? 250 : 50} search={search} onSearch={(value) => { setSearch(value); setOffset(0) }} sort={sort} direction={direction} onSort={(next) => { setSort(next); setDirection(sort === next && direction === "asc" ? "desc" : "asc"); setOffset(0) }} offset={offset} onPage={setOffset} dispatch={dispatch} onPreview={(snippet) => { setDisplay("snippet"); void preview.preview(snippet) }}
          quality={quality} onQuality={(next) => { setQuality(next); setOffset(0) }}
          picked={picked} onTogglePick={togglePick} onCompare={startCompare} onClearPicks={() => setPicked([])} comparing={compare.loading} />
        <SectorTimeline state={state} dispatch={dispatch} frameStates={frameStates} open={timelineOpen} onOpenChange={setTimelineOpen} transport={{
          prepareBlocked, state: transportState, ready: playbackReady, stepEnabled, playing, showing: showingSequence, current: shownIndex >= 0 && display === "sequence" ? shownIndex : current, rate, loop,
          buffering: bufferingIndex,
          failedFrame: currentFailed ? current : undefined,
          progress: playback.loading && playback.progress ? `Building ${playback.progress.sectors.filter((sector) => sector.state === "ready").length} of ${playback.progress.sectors.length} sectors…` : playback.loading ? "Submitting sectors…" : display === "sequence" && preparedValid && !consoleActive && !frameReady && !currentFailed ? "Buffering from downloaded frame tiles…" : undefined,
          error: playback.error ?? frameWindow.error, failed: failedFrames,
          onPrepare: () => { setTimelineOpen(true); setDiscarded(false); setPlaying(false); setDisplay("sequence"); setFrame(0); void playback.prepare(compositionKey, submitted(state.order)) },
          onPlay: () => { if (currentFailed) return; setTimelineOpen(true); setDisplay("sequence"); if (!playing && !loop && current >= count - 1) setFrame(0); setPlaying(!playing) },
          onStep: (delta) => { if (!count) return; setTimelineOpen(true); setDisplay("sequence"); setPlaying(false); setFrame((current + delta + count) % count) },
          onSeek: (index) => {
            if (preparedValid) { setTimelineOpen(true); setDisplay("sequence"); setPlaying(false); setFrame(index); return }
            setPlaying(false)
            const id = state.order[index]
            // An unprepared non-empty clip uses the existing one-sector preview.
            // An empty sector is selection-only.
            if (id && sectorMembers(state, id).length > 0) previewSector(id)
          },
          onRate: setRate, onLoop: setLoop,
          consoleOwnsPlayback: consoleActive && display === "sequence",
        }} />
        </div>
      </EdgeSidebar>
      {consoleActive && consoleSequence ? <section className="stage lab-stage" aria-label="Lab frames">
        <FrameConsole
          frames={consoleFrames}
          index={display === "compare" ? compareIndex : current}
          onIndex={display === "compare" ? setCompareIndex : setFrame}
          playing={display === "compare" ? comparePlaying : playing}
          onPlaying={display === "compare" ? setComparePlaying : setPlaying}
          noun={display === "compare" ? "exposure" : "sector"}
          view={props.view}
          canvasKey={consoleSequence.sequenceId}
          bandsLabel={`bands ${formatBands(consoleBands)}`}
          bands={consoleBands}
          busy={consoleLoaded < consoleFrames.length}
          status={consoleLoaded < consoleFrames.length ? `loading values ${consoleLoaded + 1} of ${consoleFrames.length}` : `${consoleFrames.filter((item) => item.state === "ready").length} frames`}
          onCatalogStatus={props.onCatalogStatus}
          onPick={(pixel, grid) => {
            const sky = skyAtPixel(pixel.x, pixel.y, grid.width, grid.height, props.region.ra, props.region.dec)
            props.onPoint?.({ x: pixel.x, y: pixel.y, ra: sky.ra, dec: sky.dec })
          }}
          empty={<span className="rendering">Loading calibrated values · {consoleLoaded} of {consoleFrames.length} frames</span>}
        >
          {display === "compare" && <p className="release-hint">Each picked exposure is its own frame, on one shared stretch. Different bands are different wavelengths, so brightness differences between them are not necessarily changes in time.</p>}
        </FrameConsole>
      </section> : <figure className="specific-canvas" data-decoded-frames={frameWindow.decoded} data-window={frameWindow.allFit ? "all" : "rolling"} data-playing={playing && display === "sequence"}>
        <div className="specific-viewport-area">
          <ImageViewport native={shown ? { w: shown.width, h: shown.height } : undefined} resetKey={viewportResetKey} label="Specific image" hud={shown && display === "sequence" && bufferingIndex !== undefined ? <p className="specific-buffering" aria-hidden="true">Buffering frame {bufferingIndex + 1}…</p> : undefined}>
            {(ctx) => shown ? <div className="specific-frame-stack">
              <canvas
                ref={canvas}
                data-export-image=""
                data-release={shownRelease}
                data-target-covered={shownSector?.target_covered}
                className="specific-frame-canvas"
                role="img"
                aria-label={playing && display === "sequence" ? "Sector playback" : caption}
                onClick={(event) => {
                  if (!shownNative) return
                  const rect = event.currentTarget.getBoundingClientRect()
                  const x = Math.min(shownNative.w - 1, Math.max(0, Math.floor(((event.clientX - rect.left) / rect.width) * shownNative.w)))
                  const y = Math.min(shownNative.h - 1, Math.max(0, Math.floor(((event.clientY - rect.top) / rect.height) * shownNative.h)))
                  pickFrame(x, y)
                }}
              />
              <div className="specific-target" aria-hidden="true" />
              {shownNative && <SpecificCatalogOverlay
                region={props.region}
                native={shownNative}
                gaia={props.gaia}
                wise={props.wise}
                epochMjd={shownEpoch}
                scale={ctx.effectiveScale * shownNative.w / shown.width}
                onStatus={props.onCatalogStatus}
                onPick={pickFrame}
              />}
            </div> : null}
          </ImageViewport>
          {targetMissing && <p className="specific-coverage-warning" role="alert">No data at the centre in this frame.</p>}
          {!shown && <div className="specific-empty">
            <h2>{heading}</h2>
            {!display ? <>
              <p>{props.armed ? "Build a video for this field, or compose sectors yourself." : "Press Find to list every exposure of this field."}</p>
              <button type="button" onClick={() => void startQuick()} disabled={!props.armed || quick?.phase === "listing" || quick?.phase === "building"} title={props.armed ? undefined : "Press Find first"}>Build video</button>
            </> : <p>The sector editor stays available for manual composition.</p>}
          </div>}
          {errors.length > 0 && <div className="specific-canvas-errors">{errors.map((error) => <p key={String(error)} role="alert">{error}</p>)}</div>}
        </div>
        {shown ? <figcaption><span className="specific-caption-main">{caption}</span><span className="specific-caption-note">Hatch = no data.</span>{display && display !== "snippet" ? <details className="specific-mixture"><summary>Display mixture: brightness differences are not necessarily temporal</summary><p>Equal-band grayscale mean of inverse-variance band means; pixels missing any band are no-data. A display mixture, not a single wavelength. Different sectors may use different band sets and are therefore different spectral mixtures. Target coverage is the fraction of complete pixels in a 7-pixel aperture around the field centre. Native release calibration, no cross-release correction.</p></details> : null}</figcaption> : null}
      </figure>}
    </div>
  </div>
}

/** Readable UTC: "5 May 2025 16:49:00". */
function humanUtc(utc: string) {
  return `${formatUtcDate(utc)} ${formatUtcClock(utc)}`
}

/** Console caption detail for one built sector. */
function sectorDetail(sector: SequenceInfo["sectors"][number]) {
  return `${sector.releases.map((release) => release.toUpperCase()).join(" / ")} · ${sector.snippets} exposure${sector.snippets === 1 ? "" : "s"} · centre ${Math.round(sector.target_coverage * 100)}% covered`
}

function frameText(sector: SequenceInfo["sectors"][number]) {
  const span = sector.utc_start === sector.utc_end ? humanUtc(sector.utc_start) : `${humanUtc(sector.utc_start)} to ${humanUtc(sector.utc_end)}`
  const target = sector.target_covered ? "target covered" : "target no data"
  return `bands ${formatBands(sector.bands)} · ${span} · ${sector.releases.map((release) => release.toUpperCase()).join(" / ")} · ${sector.snippets} snippet${sector.snippets === 1 ? "" : "s"} · ${target} ${Math.round(sector.target_coverage * 100)}%`
}

function scale(info: SequenceInfo) {
  return `${info.stretch_low?.toPrecision(4)}–${info.stretch_high?.toPrecision(4)} MJy/sr`
}

/** Renew every owned ref. A successful renew returns undefined even when the
 * server chose 200 rather than 204 for a body-less success. */
async function renewLeases(refs: string[], signal: AbortSignal): Promise<{ missing: string[]; message?: string } | undefined> {
  const response = await fetch(`${BASE}/api/v1/snippets/retain`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(refs), signal })
  if (response.status === 204) return undefined
  const body: unknown = await response.json().catch(() => undefined)
  const missing = retainMissing(body)
  if (missing.length) return { missing }
  if (response.ok) return undefined
  const message = typeof body === "object" && body !== null && !Array.isArray(body) && typeof (body as Record<string, unknown>).error === "string" ? (body as Record<string, unknown>).error as string : undefined
  return { missing: [], message: message ?? `Assigned snippet leases could not be renewed (${response.status}).` }
}

/** Accept the shapes the renew route may return: bare arrays, missing/missing_refs,
 * expired, refs/ids, and arrays of objects carrying snippet_ref/ref/id. */
function retainMissing(body: unknown): string[] {
  const values = Array.isArray(body) ? body
    : body && typeof body === "object"
      ? ["missing", "missing_refs", "missingRefs", "expired", "refs", "ids"].flatMap((field) => {
        const value = (body as Record<string, unknown>)[field]
        return Array.isArray(value) ? value : typeof value === "string" ? [value] : []
      })
      : []
  return [...new Set(values.flatMap((value) => {
    if (typeof value === "string") return [value]
    if (!value || typeof value !== "object") return []
    return ["snippet_ref", "ref", "id"].flatMap((field) => {
      const candidate = (value as Record<string, unknown>)[field]
      return typeof candidate === "string" ? [candidate] : []
    })
  }))]
}

/** Name only the members that are actually missing; never list renewed refs. */
function expiredMembers(refs: string[], state: SectorState) {
  const labels = refs.map((ref) => {
    const snippet = [...state.snippets.values()].find((item) => item.ref === ref)
    if (!snippet) return ref
    const sector = state.sectors.get(state.ownerBySnippet.get(snippet.id) ?? "")
    return `${sector?.name ?? "Unassigned"} · band ${snippet.band} at ${snippet.utc}`
  })
  return `Assigned snippet ${refs.length === 1 ? "ref" : "refs"} expired: ${labels.join("; ")}. Rediscover the exact data before building.`
}

/** Per-sector build label shown on sector nodes and timeline clips. */
function buildStates(state: SectorState, progress: SequenceInfo | undefined, loading: boolean, ready: boolean, stale: boolean, built: SequenceInfo | undefined, decodeFailed: (id: string) => boolean) {
  return new Map(state.order.map((id) => {
    const members = sectorMembers(state, id).length
    if (members === 0) return [id, "Empty"]
    const live = progress?.sectors.find((sector) => sector.id === id)
    if (loading && live) return [id, live.state === "ready" ? "Ready" : live.state === "failed" ? "Failed" : live.state === "building" ? "Building" : "Queued"]
    if (live?.state === "failed") return [id, "Failed"]
    if (decodeFailed(id)) return [id, "Failed"]
    if (ready) return [id, "Ready"]
    const before = built?.sectors.find((sector) => sector.id === id)
    if (stale && before) return [id, before.revision === state.sectors.get(id)!.revision ? "Ready · composition changed" : "Dirty"]
    return [id, "Unbuilt"]
  }))
}


async function pickQuickVideo(region: Region, releases: string[]): Promise<{ band: number; epochMjd: number }> {
  const ids = releases.length > 0 ? releases : ["qr2", "qr3"]
  const pages = await Promise.all(ids.map((dr) =>
    client.GET("/api/v1/coverage", {
      params: { query: { ra: region.ra, dec: region.dec, fov_l: region.fovL, fov_h: region.fovH, dr, pin: [] } },
    }).then((result) => result.data).catch(() => undefined),
  ))
  const totals = new Map<number, { count: number; releases: number }>()
  const visits = pages.flatMap((page) => page?.visits ?? [])
  for (const page of pages) {
    if (!page) continue
    for (const band of page.bands) {
      const item = totals.get(band.index + 1) ?? { count: 0, releases: 0 }
      if (band.exposures > 0) {
        item.count += band.exposures
        item.releases += 1
      }
      totals.set(band.index + 1, item)
    }
  }
  const ranked = [90, 41, 57, 75, 84, 101]
    .map((band) => ({ band, ...(totals.get(band) ?? { count: 0, releases: 0 }) }))
    .filter((item) => item.count >= 2)
  const band = (ranked.find((item) => item.releases >= 2) ?? ranked[0])?.band ?? 90
  const starts = visits.map((visit) => visit.mjd)
  const ends = visits.map((visit) => visit.mjd_end)
  const epochMjd = visits.length > 0
    ? (Math.min(...starts) + Math.max(...ends)) / 2
    : 40587 + Date.now() / 86400000
  return { band, epochMjd }
}

/** Resolve the entered ICRS position against AllWISE. A high-proper-motion
 * target entered at J2000 is moved to the video epoch so it stays in the field. */
async function resolveQuickTarget(region: Region, epochMjd: number): Promise<{ ra: number; dec: number } | undefined> {
  const fov = Math.min(4, Math.max(0.4, region.fovL * 3, region.fovH * 3))
  const controller = new AbortController()
  const timer = setTimeout(() => controller.abort(), 25000)
  try {
    const query = { ra: region.ra, dec: region.dec, fovL: fov, fovH: fov }
    const [j2000, raw] = await Promise.all([
      fetchCatalog("wise", { ...query, epochMjd: 51544.5 }, controller.signal),
      fetchCatalog("wise", query, controller.signal),
    ])
    const candidates = raw.rows
      .map((row) => ({ row, separation: separationArcsec(region.ra, region.dec, row.ra, row.dec) }))
      .filter((item) => item.separation <= 0.15 * 3600)
    const nearest = [...candidates].sort((left, right) => left.separation - right.separation)[0]
    const bright = candidates
      .filter((item) => item.row.mag !== null && item.row.mag < 8)
      .sort((left, right) => left.separation - right.separation)[0]
    const chosen = nearest && nearest.separation <= 8 ? nearest : bright ?? nearest
    if (!chosen) return undefined
    const prior = j2000.rows.find((row) => row.id === chosen.row.id)
    if (!prior) return undefined
    // The catalog's own proper motion is noisy over a decade. Anchor on the
    // entered (likely J2000) position and use the AllWISE 2010 position only to
    // estimate the motion. A current-epoch input is closer to the 2010 row and
    // is left alone.
    if (separationArcsec(region.ra, region.dec, prior.ra, prior.dec) >= chosen.separation) return undefined
    const years = (55400.5 - 51544.5) / 365.25
    const cosine = Math.cos(region.dec * Math.PI / 180)
    const deltaRa = ((chosen.row.ra - region.ra + 540) % 360 - 180) * cosine
    const deltaDec = chosen.row.dec - region.dec
    const motion = Math.hypot(deltaRa, deltaDec) / years * 3600
    if (motion < 2) return undefined
    const targetYears = (epochMjd - 51544.5) / 365.25
    return {
      ra: ((region.ra + (deltaRa / cosine) * targetYears / years) % 360 + 360) % 360,
      dec: Math.max(-90, Math.min(90, region.dec + deltaDec * targetYears / years)),
    }
  } catch {
    return undefined
  } finally {
    clearTimeout(timer)
  }
}

function separationArcsec(ra: number, dec: number, otherRa: number, otherDec: number): number {
  const cosine = Math.cos(((dec + otherDec) / 2) * Math.PI / 180)
  const deltaRa = ((ra - otherRa + 540) % 360 - 180) * cosine
  return Math.hypot(deltaRa, dec - otherDec) * 3600
}

/** One representative exposure per visit, spread across the full time range. */
function quickGroupsFromRows(rows: Snippet[]): Snippet[][] {
  const sorted = [...rows].sort((left, right) => left.mjd - right.mjd)
  const bins: Snippet[][] = []
  for (const row of sorted) {
    const last = bins.at(-1)
    if (!last || row.mjd - last[0].mjd > 10) bins.push([row])
    else last.push(row)
  }
  const spread = bins.length <= 10
    ? bins
    : Array.from({ length: 10 }, (_, index) => bins[Math.round((index * (bins.length - 1)) / 9)])
  return spread.map((bin) => {
    const covered = bin.filter((row) => row.targetCovered)
    const chosen = covered.length > 0 ? covered : bin
    return [...new Map(chosen.map((row) => [row.id, row])).values()]
  })
}

function quickSubmitted(groups: Snippet[][]) {
  return groups.map((group, index) => ({
    id: `sector-${index + 1}`,
    revision: group.length,
    snippets: group.map((snippet) => snippet.ref),
  }))
}

function quickText(quick: QuickState | undefined, elapsedMs: number) {
  if (!quick) return "Auto picks a band with data across visits."
  if (quick.phase === "listing") return quick.band === 0 ? "Finding target…" : `Listing visits · band ${quick.band} · ${Math.round(elapsedMs / 1000)} s`
  if (quick.phase === "building") return `Building ${quick.frames} frames…`
  if (quick.phase === "ready") return `${quick.frames} frames · band ${quick.band}`
  return quick.message
}

/** Gaia and WISE markers on a Specific frame. The layer uses the frame's own
 * epoch so a moving object is compared at the same position. */
function SpecificCatalogOverlay(props: {
  region: Region
  native: { w: number; h: number }
  gaia: number | null
  wise: number | null
  epochMjd?: number
  scale: number
  onStatus?: (report: CatalogStatusReport) => void
  onPick: (x: number, y: number) => void
}) {
  const supported = Math.max(props.region.fovL, props.region.fovH) <= 6.997
  const query = { ra: props.region.ra, dec: props.region.dec, fovL: props.region.fovL, fovH: props.region.fovH, epochMjd: props.epochMjd }
  const gaia = useCatalog("gaia", query, props.gaia !== null && supported)
  const wise = useCatalog("wise", query, props.wise !== null && supported)
  useEffect(() => {
    props.onStatus?.({ source: "gaia", state: gaia, supported })
  }, [props.onStatus, gaia, supported])
  useEffect(() => {
    props.onStatus?.({ source: "wise", state: wise, supported })
  }, [props.onStatus, wise, supported])
  if (!supported) return null
  return <>
    {CATALOG_SOURCES.map((source: CatalogSource) => {
      const opacity = source === "gaia" ? props.gaia : props.wise
      const state: CatalogState = source === "gaia" ? gaia : wise
      if (opacity === null || state.status !== "ready" || !state.body) return null
      return <svg
        key={source}
        className={`markers ${source}`}
        viewBox={`0 0 ${props.native.w} ${props.native.h}`}
        preserveAspectRatio="none"
        aria-label={`${source} markers`}
        style={{ opacity }}
      >
        {state.body.rows.map((row) => <Marker
          key={row.id}
          source={source}
          row={row}
          native={props.native}
          scale={props.scale}
          onPick={(pick) => props.onPick(pick.x, pick.y)}
          onHover={() => undefined}
        />)}
      </svg>
    })}
  </>
}
