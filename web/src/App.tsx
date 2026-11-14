import { useCallback, useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type FormEvent } from "react"
import { client } from "./api"
import { ColourConfig } from "./features/colour/ColourConfig"
import { DEFAULT_COLOUR_CONFIG, type ColourConfigValue } from "./url-state/colour"
import { clicksWork } from "./features/mosaic/MosaicPicture"
import { ColorDisplayControls } from "./features/mosaic/ColorDisplayControls"
import { CompositeDisplayControls } from "./features/mosaic/CompositeDisplayControls"
import { CompositeRangeControl, type CompositeRangeApplied } from "./features/mosaic/CompositeRangeControl"
import type { DisplayState } from "./url-state/display"
import { isBusy, statusText, useMosaic, warningText } from "./features/mosaic/useMosaic"
import { useRawLayers, type RawMetadata, type RawSessionState, type RenderGate, type ResolvedDisplay } from "./features/mosaic/useRawLayers"
import type { ColorDisplayView } from "./features/mosaic/useColorDisplay"
import { colorDisplayKey } from "./features/mosaic/server-display-protocol"
import { TOGGLE_ON_OPACITY, skyAtPixel, ViewerFrame, type CatalogStatusReport } from "./features/overlay/ViewerFrame"
import type { CatalogState } from "./features/overlay/useCatalog"
import type { CatalogSource } from "./features/overlay/catalog"
import { SharePanel } from "./features/share/SharePanel"
import { EdgeSidebar } from "./features/shell/edge-sidebar"
import { PlusIcon, ShareIcon } from "./features/shell/drawer"
import { SidebarSpectrum, type SpectrumPointTarget } from "./features/spectrum-panel"
import { FOV_WARN_DEG, MAX_FOV, formatNumber, isLargeField, parseDec, parseFov, parseRa } from "./url-state/position"
import {
  commit,
  currentView,
  setWorkspace,
  subscribe,
  workspaceOf,
  type View,
  type Workspace,
  displayOf,
  withDisplay,
} from "./url-state/view"

type VisitRow = { index: number; mjd: number; mjd_end: number; exposures: number }
type DataDrawer = "layers" | "spectrum"
const SHARE_TONGUE_OFFSET = 88
type RawControlsState = {
  state: RawSessionState
  metadata: RawMetadata | null
  gate: RenderGate
  resolved: ResolvedDisplay
  start: () => void
  cancel: () => void
  colorDisplay?: ColorDisplayView
  colorViewKey?: string
  /** The armed mosaic for the current view is still running on the engine. */
  mosaicBusy: boolean
  stopMosaic: () => void
}

const EMPTY_RAW_CONTROLS: RawControlsState = {
  state: { phase: "idle" },
  metadata: null,
  gate: { exportReady: false, remapBusy: false, completedRevision: 0, pendingRevision: null, label: "Finishing server image…" },
  resolved: { layers: [], lift: 1, invert: false },
  start: () => undefined,
  cancel: () => undefined,
  mosaicBusy: false,
  stopMosaic: () => undefined,
}

const LOGO = new URL("./assets/spear-logo.svg", import.meta.url).href
const initialSidebarOpen = () => typeof window === "undefined" || !window.matchMedia("(max-width: 900px)").matches

const MODES: { id: Workspace; label: string }[] = [
  { id: "color", label: "Color" },
  { id: "composite", label: "Composite" },
  { id: "specific", label: "Specific" },
]

export function App() {
  const view = useSyncExternalStore(subscribe, currentView, currentView)
  // Do not mount any data workspace for an invalid link: even a hidden/default
  // view would start a science request for something the user did not ask for.
  if (view.urlErrors?.length) {
    return (
      <div className="app">
        <header className="topbar">
          <a className="brand" href="/" aria-label="SPEARv home"><img src={LOGO} alt="SPEARv" /></a>
        </header>
        <main className="workspace" aria-label="Invalid link">
          <section className="stage">
            <div className="empty">
              <div className="invalid-link">
                <h1>Invalid link</h1>
                {view.urlErrors.map((error) => <p className="error" role="alert" key={error}>{error}</p>)}
              </div>
            </div>
          </section>
        </main>
      </div>
    )
  }
  // Specific is not available yet. A link that asks for it renders Composite
  // from the same coordinates instead, so no Specific data flow starts.
  const effective = view.mode === "specific" ? { ...view, mode: "composite" as const } : view
  return <ValidViewApp view={effective} />
}

function ValidViewApp({ view }: { view: View }) {
  const [draft, setDraft] = useState(() => textOf(view))
  const [draftLinked, setDraftLinked] = useState(view.fovLinked)
  const [formError, setFormError] = useState<string>()
  const [releases, setReleases] = useState<string[]>([])
  const [releaseCounts, setReleaseCounts] = useState<Record<string, number>>({})
  const [countRegion, setCountRegion] = useState(() => ({ ra: view.ra, dec: view.dec, fovL: view.fovL, fovH: view.fovH }))
  const [visits, setVisits] = useState<VisitRow[]>([])
  const [exposures, setExposures] = useState<number>()
  const [controlsOpen, setControlsOpen] = useState(initialSidebarOpen)
  const [dataDrawer, setDataDrawer] = useState<DataDrawer>("spectrum")
  const [dataOpen, setDataOpen] = useState(false)
  const [shareOpen, setShareOpen] = useState(false)
  const [topRightDrawer, setTopRightDrawer] = useState<"extras" | "share">("extras")
  const [rawControls, setRawControls] = useState<RawControlsState | null>(null)
  const [find, setFind] = useState<{ key: string; run: number } | null>(null)
  const findRun = useRef(0)
  const [point, setPoint] = useState<SpectrumPointTarget | null>(null)
  const [catalogStatus, setCatalogStatus] = useState<Record<CatalogSource, CatalogState | undefined>>({
    gaia: undefined,
    wise: undefined,
  })
  const [catalogSupported, setCatalogSupported] = useState(true)
  const topbarRef = useRef<HTMLElement>(null)
  const [railCue, setRailCue] = useState(false)
  const controlsTabRef = useRef<HTMLButtonElement>(null)
  const extrasTabRef = useRef<HTMLButtonElement>(null)
  const shareTabRef = useRef<HTMLButtonElement>(null)
  const spectrumAutoOpened = useRef(false)
  const focusAfterCommit = useRef<"controls" | "data" | "share" | null>(null)
  const requestedWorkspace = workspaceOf(view)
  const workspace = requestedWorkspace === "specific" ? "composite" : requestedWorkspace
  const draftFovL = parseFov(draft.fovL)
  const draftFovH = parseFov(draft.fovH)
  const draftLarge = [draftFovL, draftFovH].some((fov) => fov !== undefined && isLargeField(fov))
  const rawEnabled = workspace === "color" || (!view.color && (view.mode === "composite" || view.mode === "range"))
  const rawDisabledReason =
    view.mode === "delta"
      ? "Display controls do not apply to Delta images."
      : view.mode === "slider"
        ? "Local raw layers are unavailable for this Slider link. Choose Composite or a band range first."
        : undefined
  const display = displayOf(view)
  const activeRaw = rawControls ?? EMPTY_RAW_CONTROLS
  const handleDisplayChange = (next: DisplayState) => commit(withDisplay(view, next), "replace")
  const onRawState = useCallback((next: RawControlsState) => setRawControls(next), [])

  const appliedColour = view.colour ?? DEFAULT_COLOUR_CONFIG
  const colorViewKey = scienceViewKey(view, appliedColour)
  const activeColorRaw = activeRaw.colorViewKey === colorViewKey && activeRaw.colorDisplay?.viewKey === colorViewKey && activeRaw.colorDisplay?.settingsKey === colorDisplayKey(display) ? activeRaw : undefined

  const handleColourChange = (next: ColourConfigValue) => commit({ ...view, colour: next })

  // Engine work starts only from Find. The key is the job identity, so moving
  // the field, changing a mode or release, or editing the colour recipe retires
  // the armed run and shows Find again.
  const armed = find?.key === colorViewKey
  // Stop for the whole armed engine job, including cancel drain. Before SkyShell
  // has published once, treat the armed run as busy so Find does not flash back.
  const finding = armed && (rawControls ? rawControls.mosaicBusy : true)
  const findNow = (next: View) => {
    findRun.current += 1
    setFind({ key: scienceViewKey(next, next.colour ?? DEFAULT_COLOUR_CONFIG), run: findRun.current })
  }
  const stopFinding = () => {
    // Do not clear `find` here: stop() keeps mosaicBusy until DELETE finishes.
    activeRaw.stopMosaic()
  }

  // A narrow header is a rail. Show a control only while content remains to
  // the right, so FOV, Find and the modes are discoverable without a scrollbar.
  useLayoutEffect(() => {
    const node = topbarRef.current
    if (!node) return
    const update = () =>
      setRailCue(node.scrollWidth > node.clientWidth + 1 && node.scrollLeft + node.clientWidth < node.scrollWidth - 1)
    update()
    node.addEventListener("scroll", update, { passive: true })
    window.addEventListener("resize", update)
    // Release counts and form warnings change the header width after mount.
    const observer = new MutationObserver(update)
    observer.observe(node, { childList: true, subtree: true, characterData: true })
    return () => {
      node.removeEventListener("scroll", update)
      window.removeEventListener("resize", update)
      observer.disconnect()
    }
  }, [])

  // The form owns its text until Find. A mode or release change must not throw
  // away a half-typed coordinate.
  useEffect(() => {
    setDraft(textOf(view))
    setDraftLinked(view.fovLinked)
  }, [view.ra, view.dec, view.fovL, view.fovH, view.fovLinked])

  // Release-option counts preview the draft form, so typing a location or
  // FOV refreshes them before Find. The draft is debounced and an invalid
  // draft is skipped quietly, keeping the last good counts.
  useEffect(() => {
    const ra = parseRa(draft.ra)
    const dec = parseDec(draft.dec)
    const fovL = parseFov(draft.fovL)
    const fovH = parseFov(draft.fovH)
    if (ra === undefined || dec === undefined || fovL === undefined || fovH === undefined) return
    const timer = setTimeout(() => {
      setCountRegion((current) =>
        current.ra === ra && current.dec === dec && current.fovL === fovL && current.fovH === fovH
          ? current
          : { ra, dec, fovL, fovH },
      )
    }, 350)
    return () => clearTimeout(timer)
  }, [draft.ra, draft.dec, draft.fovL, draft.fovH])

  // A committed view change (Find, a link) still refreshes the counts
  // immediately, even if the draft was invalid and never debounced through.
  useEffect(() => {
    setCountRegion((current) =>
      current.ra === view.ra && current.dec === view.dec && current.fovL === view.fovL && current.fovH === view.fovH
        ? current
        : { ra: view.ra, dec: view.dec, fovL: view.fovL, fovH: view.fovH },
    )
  }, [view.ra, view.dec, view.fovL, view.fovH])

  useEffect(() => {
    const ac = new AbortController()
    client
      .GET("/api/v1/coverage", {
        params: { query: { ra: view.ra, dec: view.dec, fov_l: view.fovL, fov_h: view.fovH, dr: view.dr, pin: [] } },
        signal: ac.signal,
      })
      .then((result) => {
        if (!result.data) return
        setVisits(result.data.visits)
        setExposures(result.data.exposures)
      })
      .catch(() => undefined)
    return () => ac.abort()
  }, [view.ra, view.dec, view.fovL, view.fovH, view.dr])

  useEffect(() => {
    const ac = new AbortController()
    client
      .GET("/api/v1/releases", { signal: ac.signal })
      .then((result) => setReleases(result.data?.releases.map((release) => release.id) ?? []))
      .catch(() => undefined)
    return () => ac.abort()
  }, [])

  // Releases cover different stretches of the mission, so a field can be empty
  // in one and full in another. Counting all of them is what lets the page say so.
  useEffect(() => {
    if (releases.length === 0) return
    const ac = new AbortController()
    setReleaseCounts({})
    Promise.all(
      releases.map((dr) =>
        client
          .GET("/api/v1/coverage", {
            params: { query: { ra: countRegion.ra, dec: countRegion.dec, fov_l: countRegion.fovL, fov_h: countRegion.fovH, dr, pin: [] } },
            signal: ac.signal,
          })
          .then((result) => [dr, result.data?.exposures ?? 0] as const),
      ),
    )
      .then((counts) => setReleaseCounts(Object.fromEntries(counts)))
      .catch(() => undefined)
    return () => ac.abort()
  }, [releases, countRegion.ra, countRegion.dec, countRegion.fovL, countRegion.fovH])

  // A spectrum belongs to the pixels of one view. Moving the field, release,
  // render mode or colour recipe clears it rather than leaving an old plot
  // under a new picture.
  useEffect(() => {
    setPoint(null)
  }, [view.ra, view.dec, view.fovL, view.fovH, view.dr, view.mode, view.color, view.colour])

  // The frame reports the single catalog fetch per layer; the layers drawer
  // renders the only visible status and controls.
  const onCatalogStatus = useCallback((report: CatalogStatusReport) => {
    setCatalogSupported((current) => (current === report.supported ? current : report.supported))
    setCatalogStatus((current) =>
      current[report.source] === report.state ? current : { ...current, [report.source]: report.state },
    )
  }, [])

  const openExtras = (name: DataDrawer, source: "tongue" | "pick" | "switch" = "tongue") => {
    if (source === "tongue" && dataOpen) {
      setDataOpen(false)
      return
    }
    if (source === "pick" && !spectrumAutoOpened.current) spectrumAutoOpened.current = true
    focusAfterCommit.current = source === "tongue" ? "data" : null
    setDataDrawer(name)
    setTopRightDrawer("extras")
    setShareOpen(false)
    setDataOpen(true)
  }

  const cycleExtrasPanel = (dir: -1 | 1) => {
    const order: DataDrawer[] = ["spectrum", "layers"]
    const next = order[(order.indexOf(dataDrawer) + dir + order.length) % order.length]
    setDataDrawer(next)
    setTopRightDrawer("extras")
    setShareOpen(false)
    if (!dataOpen) setDataOpen(true)
  }

  useEffect(() => {
    const target = focusAfterCommit.current
    if (!target) return
    focusAfterCommit.current = null
    if (target === "controls") controlsTabRef.current?.focus()
    if (target === "data") extrasTabRef.current?.focus()
    if (target === "share") shareTabRef.current?.focus()
  }, [controlsOpen, dataDrawer, dataOpen, shareOpen, workspace])

  // One App-owned handler closes one layer at a time.
  useEffect(() => {
    if (!shareOpen && !dataOpen && !controlsOpen) return
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return
      event.preventDefault()
      if (shareOpen) {
        focusAfterCommit.current = "share"
        setShareOpen(false)
        return
      }
      if (dataOpen) {
        focusAfterCommit.current = "data"
        setDataOpen(false)
        return
      }
      if (controlsOpen) {
        focusAfterCommit.current = "controls"
        setControlsOpen(false)
      }
    }
    window.addEventListener("keydown", onKey)
    return () => window.removeEventListener("keydown", onKey)
  }, [shareOpen, dataOpen, controlsOpen])

  const submit = (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault()
    const ra = parseRa(draft.ra)
    if (ra === undefined) {
      setFormError("RA must be 0° to 360°, or hours like 20h 30m 32.0s.")
      return
    }
    const dec = parseDec(draft.dec)
    if (dec === undefined) {
      setFormError("Dec must be −90° to 90°, or an angle like 40° 17' 50.2\".")
      return
    }
    const fovL = parseFov(draft.fovL)
    if (fovL === undefined) {
      setFormError(`FOV-L must be more than 0° and at most ${MAX_FOV}°.`)
      return
    }
    const parsedH = parseFov(draft.fovH)
    if (parsedH === undefined) {
      setFormError(`FOV-H must be more than 0° and at most ${MAX_FOV}°.`)
      return
    }
    const fovH = draftLinked ? fovL : parsedH
    setFormError(undefined)
    const next = { ...view, ra, dec, fovL, fovH, fovLinked: draftLinked }
    commit(next)
    findNow(next)
  }

  return (
    <div className="app">
      <div className="topbar-shell" data-cue={railCue}>
        <header ref={topbarRef} className="topbar">
        <a className="brand" href="/" aria-label="SPEARv home">
          <img src={LOGO} alt="SPEARv" />
        </a>
        <form id="location-form" className="location" onSubmit={submit} noValidate>
          <label className="field">
            <span title="Right ascension, 0° to 360°">RA</span>
            <input
              name="ra"
              value={draft.ra}
              onChange={(event) => setDraft({ ...draft, ra: event.target.value })}
              placeholder="20h 30m 32.0s"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <label className="field">
            <span title="Declination, −90° to 90°">Dec</span>
            <input
              name="dec"
              value={draft.dec}
              onChange={(event) => setDraft({ ...draft, dec: event.target.value })}
              placeholder="40° 17' 50.2&quot;"
              autoComplete="off"
              spellCheck={false}
            />
          </label>
          <div className="field fov fov-pair">
            <span title="Field of view length and height, degrees">FOV-L/H</span>
            <input
              name="fov_l"
              aria-label="FOV-L"
              title="Field of view length, degrees"
              value={draft.fovL}
              onChange={(event) => setDraft({ ...draft, fovL: event.target.value, ...(draftLinked ? { fovH: event.target.value } : {}) })}
              inputMode="decimal"
              autoComplete="off"
              aria-describedby={draftLarge ? "fov-warning" : undefined}
            />
            <button
              className={`fov-chain ${draftLinked ? "linked" : "unlinked"}`}
              type="button"
              aria-pressed={draftLinked}
              aria-label={draftLinked ? "Unlink FOV-L and FOV-H" : "Link FOV-L and FOV-H"}
              onClick={() => {
                if (draftLinked) {
                  setDraftLinked(false)
                  return
                }
                setDraft((current) => ({ ...current, fovH: current.fovL }))
                setDraftLinked(true)
              }}
            >
              <svg
                viewBox="0 0 24 24"
                width="18"
                height="18"
                fill="none"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
                aria-hidden="true"
                focusable="false"
              >
                <g transform="rotate(-45 12 12)">
                  <path d="M10 8H7a4 4 0 0 0 0 8h3M14 8h3a4 4 0 0 1 0 8h-3" />
                  <path d={draftLinked ? "M8 12h8" : "M8 12h1M15 12h1M12 4v2M12 18v2"} />
                </g>
              </svg>
            </button>
            <input
              name="fov_h"
              aria-label="FOV-H"
              title="Field of view height, degrees"
              value={draft.fovH}
              onChange={(event) => setDraft({ ...draft, fovH: event.target.value, ...(draftLinked ? { fovL: event.target.value } : {}) })}
              inputMode="decimal"
              autoComplete="off"
              aria-describedby={draftLarge ? "fov-warning" : undefined}
            />
          </div>
          <label className="release">
            <span>Release</span>
            <select value={view.dr} onChange={(event) => commit(withRelease(view, event.target.value))}>
              {[...new Set([...releases, view.dr])].map((id) => (
                <option key={id} value={id}>
                  {releaseName(id)}
                  {releaseCounts[id] === undefined ? "" : ` · ${releaseCounts[id].toLocaleString("en-GB")}`}
                </option>
              ))}
            </select>
          </label>
        </form>
        {draftLarge ? (
          <span id="fov-warning" className="fov-warning">
            Huge field (&gt;{FOV_WARN_DEG}°): pulls much more data.
          </span>
        ) : null}
        {formError ? (
          <span className="form-error" role="alert">
            {formError}
          </span>
        ) : null}
        <div className="modes" role="group" aria-label="Workspace">
          {MODES.map((mode) => {
            const available = mode.id !== "specific"
            return (
              <button
                key={mode.id}
                type="button"
                aria-pressed={workspace === mode.id}
                disabled={!available}
                aria-disabled={!available || undefined}
                tabIndex={available ? undefined : -1}
                title={available ? undefined : "Not available yet"}
                onClick={
                  available
                    ? () => {
                        setFormError(undefined)
                        commit(setWorkspace(view, mode.id))
                      }
                    : undefined
                }
              >
                {mode.label}
              </button>
            )
          })}
        </div>
        <button
          className={finding ? "show show--stop" : "show"}
          type="submit"
          form="location-form"
          aria-label={finding ? "Stop finding this field" : "Find this field"}
          title={finding ? "Stop the running search" : "Start the search for this field"}
          onClick={
            finding
              ? (event) => {
                  event.preventDefault()
                  stopFinding()
                }
              : undefined
          }
        >
          {finding ? "Stop" : "Find"}
        </button>
        </header>
        {railCue ? (
          <button
            type="button"
            className="rail-cue"
            aria-label="Show more controls"
            title="Show more controls"
            onClick={() =>
              topbarRef.current?.scrollBy({
                left: topbarRef.current.clientWidth || 300,
                behavior: "auto",
              })
            }
          >
            ›
          </button>
        ) : null}
      </div>

      <main
        className="workspace"
        data-mode={workspace}
        aria-label="Sky workspace"
      >
        <EdgeSidebar
          id="main-controls"
          label={workspace === "color" ? "Color controls" : "Composite controls"}
          side="left"
          width="controls"
          open={controlsOpen}
          tabRef={controlsTabRef}
          onOpenChange={setControlsOpen}
          className="edge-sidebar--controls"
        >
          <div className="leftbar-body">
            {workspace === "color" ? (
              <>
                <details className="leftbar-section" open>
                  <summary>Colour source bands</summary>
                  <ColourConfig value={appliedColour} onChange={handleColourChange} />
                </details>
                <details className="leftbar-section" open>
                  <summary>Colour display</summary>
                  <ColorDisplayControls
                    display={display}
                    resolved={activeColorRaw?.resolved ?? EMPTY_RAW_CONTROLS.resolved}
                    metadata={activeColorRaw?.colorDisplay?.manifest}
                    colorDisplay={activeColorRaw?.colorDisplay}
                    onDisplayChange={handleDisplayChange}
                  />
                </details>
              </>
            ) : (
              <>
                <details className="leftbar-section" open>
                  <summary>Band range</summary>
                  <CompositeRangeControl
                    applied={compositeRangeApplied(view)}
                    onApply={(next) => commit({ ...view, ...next })}
                  />
                </details>
                <details className="leftbar-section">
                  <summary>Advanced quality</summary>
                  <label className="mask-toggle">
                    <input
                      type="checkbox"
                      checked={view.mask}
                      onChange={(event) => commit({ ...view, mask: event.target.checked })}
                    />
                    Mask flagged pixels
                  </label>
                  <p className="slot-note">Changing this setting recomputes the server image.</p>
                </details>
                <details className="leftbar-section" open>
                  <summary>Display</summary>
                  <CompositeDisplayControls
                    display={display}
                    resolved={activeRaw.resolved}
                    metadata={activeRaw.metadata}
                    session={activeRaw.state}
                    gate={activeRaw.gate}
                    onDisplayChange={handleDisplayChange}
                    onStartRaw={activeRaw.start}
                    onCancelRaw={activeRaw.cancel}
                    disabledReason={rawEnabled ? undefined : rawDisabledReason}
                  />
                </details>
              </>
            )}
          </div>
        </EdgeSidebar>
        <SkyShell
          view={view}
          armed={armed}
          run={find?.run ?? 0}
          visits={visits}
          exposures={exposures}
          releaseCounts={releaseCounts}
          point={point}
          onPointChange={setPoint}
          colour={appliedColour}
          display={display}
          rawEnabled={rawEnabled}
          onRawState={onRawState}
          onCatalogStatus={onCatalogStatus}
          onOpenSpectrum={() => openExtras("spectrum", "pick")}
        />
        <EdgeSidebar
          id="drawer-extras"
          label="Extras"
          tabIcon={<PlusIcon />}
          side="right"
          width="drawer"
          layout="overlay"
          open={dataOpen}
          tabRef={extrasTabRef}
          tabOffset={0}
          onOpenChange={(open) => {
            if (!open) {
              setDataOpen(false)
              return
            }
            focusAfterCommit.current = "data"
            setTopRightDrawer("extras")
            setShareOpen(false)
            setDataOpen(true)
          }}
          className={`edge-sidebar--data${topRightDrawer === "extras" ? " edge-sidebar--front" : ""}`}
        >
          <div className="drawer-body">
            <div className="extras-switch" role="tablist" aria-label="Extras panel">
              <button
                type="button"
                className="extras-switch__arrow"
                aria-label="Previous extras panel"
                onClick={() => cycleExtrasPanel(-1)}
              >
                ‹
              </button>
              <span className="extras-switch__label" role="tab" aria-selected="true">
                {dataDrawer === "spectrum" ? "Spectrum" : "Layers"}
              </span>
              <button
                type="button"
                className="extras-switch__arrow"
                aria-label="Next extras panel"
                onClick={() => cycleExtrasPanel(1)}
              >
                ›
              </button>
            </div>
            {dataDrawer === "layers" ? (
              <>
                <h2 className="drawer-title">Catalog layers</h2>
                <div className="layer-row">
                  <span className="layer-key gaia" aria-hidden="true" />
                  <span className="layer-name">Gaia</span>
                  <button
                    type="button"
                    aria-pressed={view.gaia !== null}
                    onClick={() => commit({ ...view, gaia: view.gaia === null ? TOGGLE_ON_OPACITY : null })}
                  >
                    {view.gaia === null ? "Show" : "Hide"}
                  </button>
                  <input
                    type="range"
                    min={0}
                    max={1}
                    step={0.05}
                    aria-label="Gaia opacity"
                    disabled={view.gaia === null}
                    value={view.gaia ?? TOGGLE_ON_OPACITY}
                    onChange={(event) => commit({ ...view, gaia: Number(event.target.value) }, "replace")}
                  />
                  <LayerStatus source="gaia" view={view} state={catalogStatus.gaia} supported={catalogSupported} />
                </div>
                <div className="layer-row">
                  <span className="layer-key wise" aria-hidden="true" />
                  <span className="layer-name">WISE</span>
                  <button
                    type="button"
                    aria-pressed={view.wise !== null}
                    onClick={() => commit({ ...view, wise: view.wise === null ? TOGGLE_ON_OPACITY : null })}
                  >
                    {view.wise === null ? "Show" : "Hide"}
                  </button>
                  <input
                    type="range"
                    min={0}
                    max={1}
                    step={0.05}
                    aria-label="WISE opacity"
                    disabled={view.wise === null}
                    value={view.wise ?? TOGGLE_ON_OPACITY}
                    onChange={(event) => commit({ ...view, wise: Number(event.target.value) }, "replace")}
                  />
                  <LayerStatus source="wise" view={view} state={catalogStatus.wise} supported={catalogSupported} />
                </div>
                {view.mode === "delta" ? (
                  <label className="layer-check">
                    <input
                      type="checkbox"
                      checked={view.candidates}
                      onChange={(event) => commit({ ...view, candidates: event.target.checked })}
                    />
                    Candidate labels
                  </label>
                ) : null}
              </>
            ) : (
              <SidebarSpectrum ra={view.ra} dec={view.dec} release={view.dr} point={point} fovL={view.fovL} fovH={view.fovH} />
            )}
          </div>
        </EdgeSidebar>
        <EdgeSidebar
          id="drawer-share"
          label="Share"
          tabIcon={<ShareIcon />}
          side="right"
          width="drawer"
          layout="overlay"
          open={shareOpen}
          tabOffset={SHARE_TONGUE_OFFSET}
          tabRef={shareTabRef}
          onOpenChange={(open) => {
            if (!open) {
              setShareOpen(false)
              return
            }
            focusAfterCommit.current = "share"
            setTopRightDrawer("share")
            setDataOpen(false)
            setShareOpen(true)
          }}
          className={`edge-sidebar--share${topRightDrawer === "share" ? " edge-sidebar--front" : ""}`}
        >
          <div className="drawer-body">
            <SharePanel
              view={view}
              gate={rawEnabled ? activeRaw.gate : undefined}
              rawSession={rawEnabled && !view.color ? activeRaw.state : undefined}
              rawBytes={
                rawEnabled && activeRaw.metadata
                  ? activeRaw.metadata.width *
                    activeRaw.metadata.height *
                    activeRaw.metadata.channels.length *
                    (activeRaw.metadata.dtype === "u16le" ? 2 : 4)
                  : undefined
              }
              colorDisplay={rawEnabled && view.color ? activeRaw.colorDisplay : undefined}
              colorDisplayCurrent={activeRaw.colorViewKey === scienceViewKey(view, appliedColour)}
              onRetryRaw={rawEnabled && !view.color ? activeRaw.start : undefined}
            />
          </div>
        </EdgeSidebar>
      </main>
    </div>
  )
}

function scienceViewKey(view: View, colour: ColourConfigValue): string {
  return JSON.stringify({
    ra: view.ra,
    dec: view.dec,
    fovL: view.fovL,
    fovH: view.fovH,
    dr: view.dr,
    mask: view.mask,
    color: view.color,
    mode: view.mode,
    pins: view.pins,
    bands: colour.colorBands,
  })
}

/** Color and Composite share one viewer, one coverage request and one drawer
 * host. */
function SkyShell(props: {
  view: View
  armed: boolean
  run: number
  visits: VisitRow[]
  exposures: number | undefined
  releaseCounts: Record<string, number>
  point: SpectrumPointTarget | null
  onPointChange: (point: SpectrumPointTarget | null) => void
  colour: ColourConfigValue
  display: DisplayState
  rawEnabled: boolean
  onRawState: (state: RawControlsState) => void
  onCatalogStatus: (report: CatalogStatusReport) => void
  onOpenSpectrum: () => void
}) {
  const { view, visits } = props
  const mosaic = useMosaic(view, props.colour.colorBands, props.armed, props.run)
  const raw = useRawLayers(mosaic, props.display, props.rawEnabled, view.color, scienceViewKey(view, props.colour), mosaic.restart)
  const rawStateKey = JSON.stringify(raw.state)
  const rawGateKey = JSON.stringify(raw.gate)
  const rawColorKey = JSON.stringify(raw.colorDisplay && {
    phase: raw.colorDisplay.phase,
    viewKey: raw.colorDisplay.viewKey,
    manifest: raw.colorDisplay.manifest,
    settingsKey: raw.colorDisplay.settingsKey,
    jobId: raw.colorDisplay.jobId,
    revision: raw.colorDisplay.revision,
    previewUrl: raw.colorDisplay.previewUrl,
    fullUrl: raw.colorDisplay.fullUrl,
    completedRows: raw.colorDisplay.completedRows,
    totalRows: raw.colorDisplay.totalRows,
    error: raw.colorDisplay.error,
    gate: raw.colorDisplay.gate,
  })
  const rawColorBlob = raw.colorDisplay?.fullBlob
  const rawColorRetry = raw.colorDisplay?.retry
  const displayKey = JSON.stringify(props.display)
  // Color sets mosaic.final as soon as science lands; the server PNG can still
  // be rendering. Keep Stop up until that PNG is cached or the display fails.
  const colorPending =
    view.color &&
    !!mosaic.final &&
    !!raw.colorDisplay &&
    !raw.colorDisplay.fullBlob &&
    raw.colorDisplay.phase !== "failed" &&
    raw.colorDisplay.phase !== "expired"
  const mosaicBusy = props.armed && (isBusy(mosaic) || !!colorPending)
  useEffect(() => {
    props.onRawState({
      state: raw.state,
      metadata: raw.metadata,
      gate: raw.gate,
      resolved: raw.resolved,
      start: raw.start,
      cancel: raw.cancel,
      colorDisplay: raw.colorDisplay,
      colorViewKey: raw.colorDisplay?.viewKey,
      mosaicBusy,
      stopMosaic: mosaic.stop,
    })
  }, [props.onRawState, props.rawEnabled, rawStateKey, rawGateKey, raw.metadata, rawColorKey, rawColorBlob, rawColorRetry, scienceViewKey(view, props.colour), displayKey, raw.start, raw.cancel, mosaicBusy, mosaic.stop, mosaic.stopping])

  // The clicked pixel is the rendered-grid index. Resolve its sky with the same
  // north-up TAN projection the server uses, so the panel can show the readout
  // without a second spectrum request just to learn the coordinate. A real sky
  // pixel opens the spectrum drawer; the drawer never steals focus.
  const pick = (pixel: { x: number; y: number }) => {
    if (!clicksWork(mosaic.mosaic)) return
    const width = mosaic.mosaic?.info.width
    const height = mosaic.mosaic?.info.height
    if (!width || !height) return
    const sky = skyAtPixel(pixel.x, pixel.y, width, height, view.ra, view.dec)
    props.onPointChange({ x: pixel.x, y: pixel.y, ra: sky.ra, dec: sky.dec })
    props.onOpenSpectrum()
  }

  const shownStatus = props.armed ? statusText(mosaic) : "idle"
  const busy = props.armed && isBusy(mosaic)
  const shownGrid = mosaic.mosaic && `${mosaic.mosaic.info.width}×${mosaic.mosaic.info.height}`
  const shownError = mosaic.error ?? warningText(mosaic)
  const counted = Object.keys(props.releaseCounts).length > 0
  const betterRelease = Object.entries(props.releaseCounts).sort((a, b) => b[1] - a[1])[0]

  return (
    <>
      <section className="stage" aria-label="Sky">
        <figure className="viewer">
          {mosaic.mosaic ? (
            <ViewerFrame
              mosaic={mosaic.mosaic}
              mosaicStatus={mosaic.status}
              mosaicFrontier={mosaic.frontier}
              colorDisplay={view.color ? raw.colorDisplay : undefined}
              candidatesReady={mosaic.status?.state === "done" && clicksWork(mosaic.mosaic)}
              view={view}
              epochMjd={visits[0]?.mjd}
              onCatalogStatus={props.onCatalogStatus}
              onPick={pick}
            />
          ) : (
            <div className="empty">
              {!props.armed ? (
                <span className="empty-find">Press Find to search this field.</span>
              ) : busy ? (
                <span className="rendering">
                  Rendering {view.color ? "RGB" : view.mode} · {shownStatus}
                </span>
              ) : (
                "No image yet"
              )}
            </div>
          )}
          <figcaption>
            {view.color ? (
              <span className="delta-legend">
                <span className="swatch rgb" aria-hidden="true" />
                {colourCaption(props.colour)}
              </span>
            ) : view.mode === "delta" ? (
              <span className="delta-legend">
                <span className="delta-key plus" aria-hidden="true" />
                brighter {visitDate(visits, view.pins.find((pin) => pin.sign === "+")?.visit)}
                <span className="delta-key minus" aria-hidden="true" />
                brighter {visitDate(visits, view.pins.find((pin) => pin.sign === "-")?.visit)}
                <span className="delta-key nodata" aria-hidden="true" />
                no overlap
              </span>
            ) : (
              <span className="delta-legend">
                <span className="swatch" aria-hidden="true" />
                no valid sample
              </span>
            )}
            <span>{shownGrid ? `${shownGrid} px` : "native scale"}</span>
            <span>{props.exposures === undefined ? "" : `${props.exposures} exposures`}</span>
            <span className={busy ? "status busy" : "status"}>{shownStatus}</span>
            {clicksWork(mosaic.mosaic) ? (
              <span className="caption-hint">Click a pixel for its spectrum.</span>
            ) : (
              <span className="caption-hint">
                Spectrum clicks are off above 6.99°: the server&apos;s click grid stops at 4096 px and would point at
                the wrong sky.
              </span>
            )}
          </figcaption>
          {isLargeField(Math.max(view.fovL, view.fovH)) ? (
            <p className="field-warning" role="note">
              Large field (&gt;{FOV_WARN_DEG}°): this pulls much more data and can take a long time.
            </p>
          ) : null}
          {counted && props.releaseCounts[view.dr] === 0 ? (
            <p className="release-hint">
              {betterRelease[1] > 0 ? (
                <>
                  {releaseName(view.dr)} has no exposures here. {releaseName(betterRelease[0])} has{" "}
                  {betterRelease[1].toLocaleString("en-GB")}.{" "}
                  <button type="button" onClick={() => commit(withRelease(view, betterRelease[0]))}>
                    Switch to {releaseName(betterRelease[0])}
                  </button>
                </>
              ) : (
                "No release covers this field yet."
              )}
            </p>
          ) : null}
          {shownError ? (
            <p className="error" role="alert">
              {shownError}
            </p>
          ) : null}
        </figure>
      </section>
    </>
  )
}

/** The single visible catalog status readout, inside the layers drawer. */
function LayerStatus(props: { source: CatalogSource; view: View; state: CatalogState | undefined; supported: boolean }) {
  const { source, view, state, supported } = props
  const name = source === "gaia" ? "Gaia" : "WISE"
  if (view[source] === null) return <span className="overlay-status">Off</span>
  if (!supported)
    return (
      <span className="overlay-status">
        Markers stop above 6.99°: the server&rsquo;s grid stops at 4096 px and would place markers on the wrong sky.
      </span>
    )
  if (!state || state.status === "loading" || state.status === "idle")
    return <span className="overlay-status">Loading…</span>
  if (state.status === "empty") return <span className="overlay-status">No {name} data here</span>
  if (state.status === "failed") {
    return (
      <span className="overlay-failure" role="alert">
        {state.error ?? `${name} failed`}
      </span>
    )
  }
  const count = state.body?.rows.length ?? 0
  return (
    <span className="overlay-status">
      {count} marker{count === 1 ? "" : "s"}
      {state.body?.truncated ? " · list truncated; zoom in" : ""}
    </span>
  )
}

/** False-colour caption from the bands actually applied, in interface
 * numbering (1-102). Never a fixed wavelength triplet. */
function colourCaption(colour: ColourConfigValue): string {
  const bands = colour.colorBands
  return `False colour · R band ${bands.r.pin + 1}, G band ${bands.g.pin + 1}, B band ${bands.b.pin + 1}`
}

function textOf(view: View) {
  return { ra: formatNumber(view.ra), dec: formatNumber(view.dec), fovL: formatNumber(view.fovL), fovH: formatNumber(view.fovH) }
}

function visitDate(visits: VisitRow[], index: number | undefined): string {
  const visit = visits.find((candidate) => candidate.index === index) ?? visits[0]
  if (!visit) return "visit ?"
  return new Date((visit.mjd - 40587) * 86400000).toLocaleDateString("en-GB", {
    month: "short",
    year: "numeric",
    timeZone: "UTC",
  })
}

function releaseName(id: string): string {
  return id.toUpperCase().replace("_DEEP", " deep")
}

// Visit numbers belong to one release, so a pinned visit means nothing in another.
// Delta needs a visit on each side, so it restarts from the first two.
function compositeRangeApplied(view: View): CompositeRangeApplied {
  if (view.mode !== "range") return { kind: "all" }
  const bands = view.pins.flatMap((pin) => {
    const first = pin.center - Math.floor((pin.width - 1) / 2)
    return Array.from({ length: pin.width }, (_, index) => first + index + 1)
  })
  return { kind: "range", bands }
}

function withRelease(view: View, dr: string): View {
  return {
    ...view,
    dr,
    pins: view.pins.map((pin, index) => ({ ...pin, visit: view.mode === "delta" ? index : undefined })),
  }
}
