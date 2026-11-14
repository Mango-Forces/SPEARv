import { useEffect, useState } from "react"
import { displayOf, viewSearch, type View } from "../../url-state/view"
import { formatNumber } from "../../url-state/position"
import { colorDisplayKey } from "../mosaic/server-display-protocol"
import type { ColorDisplayView } from "../mosaic/useColorDisplay"
import { isColorImage } from "../mosaic/color-export-mode"
import type { RawSessionState, RenderGate } from "../mosaic/raw-protocol"
import "./share.css"

const EXPORT_FORMATS = [
  { key: "png", mime: "image/png", ext: "png", label: "PNG", hint: "Lossless" },
  { key: "jpeg", mime: "image/jpeg", ext: "jpg", label: "JPEG", hint: "Smaller file" },
  { key: "webp", mime: "image/webp", ext: "webp", label: "WebP", hint: "Modern browsers" },
] as const

type ExportFormatKey = (typeof EXPORT_FORMATS)[number]["key"]

/**
 * Share content only. The shell owns the edge tab and panel frame, so this
 * component renders sections and never a modal or header action. It makes no
 * network calls: sharing must not start a render or science job. There is no
 * embed or iframe output by product rule; sharing is a link plus an image file.
 */
export function SharePanel(props: {
  view: View
  release?: string
  gate?: RenderGate
  /** Color/Composite raw-layer session; drives export hold-up progress. */
  rawSession?: RawSessionState
  /** Bytes estimate when known (from metadata), for the hold-up copy. */
  rawBytes?: number
  onRetryRaw?: () => void
  colorDisplay?: ColorDisplayView
  colorDisplayCurrent?: boolean
}) {
  const { view, gate, rawSession, rawBytes, onRetryRaw, colorDisplay } = props
  const search = viewSearch(view)
  const fullUrl =
    typeof window === "undefined"
      ? `?${search}`
      : `${window.location.origin}${window.location.pathname}?${search}`

  const [copied, setCopied] = useState(false)
  const [copying, setCopying] = useState(false)
  const [copyError, setCopyError] = useState<string>()
  const [showFallback, setShowFallback] = useState(false)

  const [formatKey, setFormatKey] = useState<ExportFormatKey>("png")
  const colorImage = isColorImage(view)
  const selected = colorImage ? EXPORT_FORMATS[0] : EXPORT_FORMATS.find((option) => option.key === formatKey) ?? EXPORT_FORMATS[0]
  const exportRelease = props.release ?? view.dr
  const filename = `${fileStem(view, exportRelease)}.${selected.ext}`
  const includes = linkIncludes(view)

  const [ready, setReady] = useState(false)
  const [exporting, setExporting] = useState<string | null>(null)
  const [exportError, setExportError] = useState<string>()
  const [exported, setExported] = useState<string>()
  const currentColorDisplay = colorImage && props.colorDisplayCurrent === true && colorDisplay?.settingsKey === colorDisplayKey(displayOf(view))
  const colorReady = currentColorDisplay && !!colorDisplay?.fullBlob && colorDisplay.gate.exportReady
  const exportReady = colorImage ? colorReady : ready && (gate?.exportReady ?? true)

  // A copied confirmation belongs to one URL. A new view resets it so the
  // panel never claims a fresh link was copied.
  useEffect(() => {
    setCopied(false)
    setCopyError(undefined)
    setShowFallback(false)
    setExportError(undefined)
    setExported(undefined)
  }, [search])

  // The export control stays disabled until the displayed raster exists. A
  // MutationObserver scoped to <main> flips readiness when the viewer mounts,
  // swaps or unmarks its canvas. No polling.
  useEffect(() => {
    const update = () => setReady(findExportCanvas() !== null)
    update()
    const root = document.querySelector("main") ?? document.body
    const observer = new MutationObserver(update)
    observer.observe(root, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["data-export-image"],
    })
    return () => observer.disconnect()
  }, [])

  const copyLink = async () => {
    if (copying) return
    setCopying(true)
    setCopyError(undefined)
    try {
      await writeClipboard(fullUrl)
      setCopied(true)
      setShowFallback(false)
    } catch (cause) {
      setCopied(false)
      setCopyError(cause instanceof Error ? cause.message : "Copy failed in this browser.")
      // Never a dead action: the full link stays visible and selectable.
      setShowFallback(true)
    } finally {
      setCopying(false)
    }
  }

  const exportRaster = async (mime: string, ext: string, label: string) => {
    if (exporting) return
    setExporting(mime)
    setExportError(undefined)
    setExported(undefined)
    try {
      // Snapshot at click: copy the live canvas now so a sequence advance or
      // a tile landing mid-encode cannot change what is saved.
      if (colorImage) {
        const original = currentColorDisplay ? colorDisplay?.fullBlob : undefined
        if (!original || !colorReady) throw new Error("The current full-resolution Color PNG is not ready yet.")
        if (original.type.split(";", 1)[0].trim().toLowerCase() !== "image/png") throw new Error("The server returned an invalid Color PNG.")
        const url = URL.createObjectURL(original)
        try {
          const anchor = document.createElement("a")
          anchor.href = url
          anchor.download = `${fileStem(view, exportRelease)}.png`
          document.body.append(anchor)
          anchor.click()
          anchor.remove()
        } finally {
          window.setTimeout(() => URL.revokeObjectURL(url), 4000)
        }
        setExported("Saved the exact full-resolution server PNG.")
        return
      }
      const source = findExportCanvas()
      if (!source) throw new Error("There is no rendered image to export yet.")
      if (source.width === 0 || source.height === 0) {
        throw new Error("There is no rendered image to export yet.")
      }
      const snapshot = document.createElement("canvas")
      snapshot.width = source.width
      snapshot.height = source.height
      const context = snapshot.getContext("2d")
      if (!context) throw new Error("Image export is unavailable in this browser.")
      try {
        context.drawImage(source, 0, 0)
      } catch {
        throw new Error("This image cannot be exported because its canvas is tainted.")
      }
      const blob = await new Promise<Blob | null>((resolve) => {
        try {
          if (mime === "image/png") snapshot.toBlob((result) => resolve(result), mime)
          else snapshot.toBlob((result) => resolve(result), mime, 0.92)
        } catch {
          resolve(null)
        }
      })
      if (!blob) {
        throw new Error(
          mime === "image/png"
            ? "The image could not be read back for export."
            : `${label} encoding is not supported in this browser. Try PNG.`,
        )
      }
      // Never mislabel: some browsers answer an unsupported type with PNG
      // bytes. Saving those as .webp or .jpg would lie about the contents.
      if (blob.type && blob.type !== mime) {
        throw new Error(`${label} encoding is not supported in this browser. Try PNG.`)
      }
      const url = URL.createObjectURL(blob)
      try {
        const anchor = document.createElement("a")
        anchor.href = url
        anchor.download = `${fileStem(view, exportRelease)}.${ext}`
        document.body.append(anchor)
        anchor.click()
        anchor.remove()
      } finally {
        window.setTimeout(() => URL.revokeObjectURL(url), 4000)
      }
      setExported(`Saved ${label}.`)
    } catch (cause) {
      setExportError(cause instanceof Error ? cause.message : "Image export failed.")
    } finally {
      setExporting(null)
    }
  }

  return (
    <div className="share-panel">
      <section className="share-section" aria-labelledby="share-link-heading">
        <h3 id="share-link-heading">Link</h3>
        <p className="share-lede">Copy this field and its image settings. Zoom and pan are not included.</p>
        <ul className="share-includes" aria-label="Link includes">
          {includes.map((item) => (
            <li key={item}>{item}</li>
          ))}
        </ul>
        <div className="share-row">
          <button type="button" onClick={() => void copyLink()} disabled={copying}>
            {copying ? "Copying…" : "Copy link"}
          </button>
          {copied ? (
            <span role="status" className="share-status">
              Link copied
            </span>
          ) : null}
        </div>
        {copyError ? (
          <p role="alert" className="share-error">
            {copyError}
          </p>
        ) : null}
        {showFallback ? (
          <label className="share-fallback">
            <span>Copy this link manually</span>
            <input
              type="text"
              readOnly
              value={fullUrl}
              aria-label="View link"
              onFocus={(event) => event.currentTarget.select()}
              onClick={(event) => event.currentTarget.select()}
            />
          </label>
        ) : null}
      </section>

      <section className="share-section" aria-labelledby="share-export-heading">
        <h3 id="share-export-heading">Export image</h3>
        <p className="share-lede">{colorImage ? "Color original · lossless server PNG. The bounded preview is not the downloadable original." : "Raster only. Gaia/WISE markers are not included."}</p>
        <fieldset className="share-format-group" disabled={!exportReady || exporting !== null}>
          <legend>{colorImage ? "Original format" : "Format"}</legend>
          <div className="share-format-row">
            {(colorImage ? [EXPORT_FORMATS[0]] : EXPORT_FORMATS).map((option) => (
              <label key={option.key} className="share-format">
                <input
                  type="radio"
                  name="share-export-format"
                  value={option.key}
                  checked={selected.key === option.key}
                  onChange={() => {
                    setFormatKey(option.key)
                    setExportError(undefined)
                    setExported(undefined)
                  }}
                />
                <span className="share-format-text">{option.label}</span>
                <span className="share-format-hint">{option.hint}</span>
              </label>
            ))}
          </div>
        </fieldset>
        <div className="share-download-row">
          <button
            type="button"
            className="share-primary"
            disabled={!exportReady || exporting !== null}
            onClick={() => void exportRaster(selected.mime, selected.ext, selected.label)}
          >
            {exporting ? (colorImage ? "Saving…" : "Preparing…") : `Download ${selected.label}`}
          </button>
        </div>
        <p className="share-filename">
          <span>Filename</span>
          <code title={filename}>{filename}</code>
        </p>
        {colorImage ? (
          <ColorExportStatus display={colorDisplay} current={currentColorDisplay} />
        ) : (
          <ExportHoldup
            ready={ready}
            exportReady={exportReady}
            gate={gate}
            session={rawSession}
            estimateBytes={rawBytes}
            onRetry={onRetryRaw}
            exporting={exporting !== null}
          />
        )}
        {exporting ? (
          <p role="status" className="share-status">
            {colorImage ? "Saving the cached original PNG…" : `Encoding ${selected.label} from the finished picture…`}
          </p>
        ) : null}
        {exported ? (
          <p role="status" className="share-status">
            {exported}
          </p>
        ) : null}
        {exportError ? (
          <p role="alert" className="share-error">
            {exportError}
          </p>
        ) : null}
      </section>
    </div>
  )
}

function ColorExportStatus(props: { display: ColorDisplayView | undefined; current: boolean }) {
  const display = props.display
  if (!display || !props.current) return <p role="status" className="share-status">Waiting for the current Color display. An older view cannot be downloaded.</p>
  if (display.phase === "queued") return <p role="status" className="share-status">Color settings are queued until science finishes.</p>
  if (display.phase === "failed" || display.phase === "expired") return <div className="share-holdup" role="alert">
    <p className="share-holdup-title">The current Color PNG is unavailable</p>
    <p className="share-holdup-detail">{display.error ?? display.gate.label}</p>
    <button type="button" className="share-primary" onClick={display.retry}>Retry current PNG</button>
  </div>
  if (display.fullBlob && display.gate.exportReady) return <p role="status" className="share-status share-status--ok">{display.error ? `${display.error} ` : ""}The exact current full-resolution server PNG is ready to download.</p>
  const full = display.manifest?.full
  const total = full?.total_rows ?? 0
  const completed = Math.min(full?.completed_rows ?? 0, total)
  const percent = total > 0 ? Math.round((completed / total) * 100) : 0
  const detail = full?.state === "not_requested"
    ? "The full PNG will be requested after display settings settle."
    : full?.state === "queued"
      ? "The full PNG is queued on the server."
      : full?.state === "ready"
        ? "The full PNG is ready; verifying its bytes and dimensions."
        : total > 0
          ? `Server render progress · ${completed.toLocaleString("en-US")} / ${total.toLocaleString("en-US")} rows (${percent}%)`
          : display.gate.label
  return <div className="share-holdup" role="status" aria-live="polite">
    <p className="share-holdup-title">Preparing the original full-resolution Color PNG</p>
    <p className="share-holdup-detail">{display.error ?? detail}</p>
    {total > 0 && (full?.state === "queued" || full?.state === "rendering") ? <div className="share-progress" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={percent} aria-label="Full Color PNG render progress">
      <div className="share-progress-bar" style={{ width: `${percent}%` }} />
    </div> : null}
    <p className="share-holdup-hint">The full PNG is fetched and verified before Download is enabled. This remains the original image, not the bounded on-screen preview.</p>
  </div>
}

/** Why Download is disabled, with a real progress bar when layers are loading. */
function ExportHoldup(props: {
  ready: boolean
  exportReady: boolean
  gate?: RenderGate
  session?: RawSessionState
  estimateBytes?: number
  onRetry?: () => void
  exporting: boolean
}) {
  if (props.exporting) return null
  if (props.exportReady) {
    return (
      <p role="status" className="share-status share-status--ok">
        {props.gate?.label && props.gate.label !== "Server image" ? props.gate.label : "Ready to download the picture on screen."}
      </p>
    )
  }
  if (!props.ready) {
    return (
      <p role="status" className="share-status">
        No rendered image yet. Wait for the mosaic to finish painting.
      </p>
    )
  }

  const session = props.session
  const phase = session?.phase
  const mib = props.estimateBytes != null ? props.estimateBytes / 1048576 : null

  if (phase === "loading" && session && "totalTiles" in session) {
    const total = session.totalTiles
    const loaded = session.loadedTiles
    const pct = total > 0 ? Math.min(100, Math.round((loaded / total) * 100)) : 0
    return (
      <div className="share-holdup" role="status" aria-live="polite">
        <p className="share-holdup-title">Download waits for the three colour layers</p>
        <p className="share-holdup-detail">
          {total > 0
            ? `Loading scale-encoded layers · ${loaded.toLocaleString("en-US")} / ${total.toLocaleString("en-US")} tiles (${pct}%)`
            : "Reading layer information…"}
          {mib != null ? ` · about ${mib >= 1024 ? `${(mib / 1024).toFixed(1)} GiB` : `${mib.toFixed(0)} MiB`} of layer data` : ""}
        </p>
        <div
          className="share-progress"
          role="progressbar"
          aria-valuemin={0}
          aria-valuemax={100}
          aria-valuenow={pct}
          aria-label="Colour layer download progress"
        >
          <div className="share-progress-bar" style={{ width: `${pct}%` }} />
        </div>
        <p className="share-holdup-hint">
          Export stays locked until every layer tile is in memory and mixed; that is the picture Download will save. Keep this tab open, because leaving abandons the job.
        </p>
      </div>
    )
  }

  if (phase === "unavailable" && session && "reason" in session) {
    return (
      <div className="share-holdup" role="alert">
        <p className="share-holdup-title">Local layers failed; download is blocked</p>
        <p className="share-holdup-detail">{session.reason}</p>
        {props.onRetry ? (
          <button type="button" className="share-primary" onClick={props.onRetry}>
            Retry colour layers
          </button>
        ) : null}
        <p className="share-holdup-hint">
          At very large fields the browser may run out of memory (~0.8 GiB of layers at 20°). Retry once, or export a slightly smaller FOV.
        </p>
      </div>
    )
  }

  if (phase === "awaiting-opt-in" && session && "estimateBytes" in session) {
    const need = session.estimateBytes / 1048576
    return (
      <div className="share-holdup" role="status">
        <p className="share-holdup-title">Local layers not loaded yet</p>
        <p className="share-holdup-detail">
          About {need >= 1024 ? `${(need / 1024).toFixed(1)} GiB` : `${need.toFixed(0)} MiB`}. Start the download from Colour/Composite display, then return here.
        </p>
      </div>
    )
  }

  return (
    <div className="share-holdup" role="status" aria-live="polite">
      <p className="share-holdup-title">Download not ready yet</p>
      <p className="share-holdup-detail">{props.gate?.label ?? "Finishing the picture…"}</p>
      {phase === "idle" || phase === "ready" ? (
        <p className="share-holdup-hint">If this stalls on a huge field, check Colour display for layer errors or Retry.</p>
      ) : null}
    </div>
  )
}

/** Short read-only summary of what a copied link restores. Derived from the
 * current view only, so describing it starts no network or science work.
 * Display adjustments are listed when present because they already travel in
 * the URL; filename preview below uses the same stem as the download. */
function linkIncludes(view: View): string[] {
  const items = [`${modeLabel(view)} · ${view.dr}`, "RA/Dec/FOV"]
  if (view.gaia !== null || view.wise !== null) items.push("Catalog overlays")
  if (!view.candidates || view.movers) items.push("Marker options")
  if (view.colour) items.push("Colour recipe")
  if (view.display) items.push("Display adjustments")
  return items
}

/** The currently visible export raster. Owners mark exactly the displayed
 * picture with data-export-image; hidden, empty or zero-size canvases never
 * count, so export cannot grab a placeholder or an off-screen buffer. */
function findExportCanvas(): HTMLCanvasElement | null {
  if (typeof document === "undefined") return null
  const candidates = document.querySelectorAll("canvas[data-export-image]")
  for (const element of candidates) {
    if (!(element instanceof HTMLCanvasElement)) continue
    if (element.width === 0 || element.height === 0) continue
    if (element.closest("[hidden]")) continue
    const rect = element.getBoundingClientRect()
    if (rect.width < 1 || rect.height < 1) continue
    return element
  }
  return null
}

// Browsers hide the clipboard API on plain HTTP, which a tailnet address is.
async function writeClipboard(text: string) {
  if (navigator.clipboard) return navigator.clipboard.writeText(text)
  const box = document.createElement("textarea")
  box.value = text
  // Keep the fallback out of view without display:none, which some browsers
  // refuse to copy from.
  box.style.position = "fixed"
  box.style.opacity = "0"
  document.body.append(box)
  box.focus()
  box.select()
  let copied = false
  try {
    copied = document.execCommand("copy")
  } finally {
    box.remove()
  }
  if (!copied) throw new Error("Copy failed in this browser. Select the link below instead.")
}

function modeLabel(view: View): string {
  if (view.color) return "color"
  return view.mode
}

/** Position and mode in the filename, so an export stays identifiable next to
 * other fields. Release is included because QR2 and QR3 of one field differ. */
function fileStem(view: View, release: string): string {
  const fov = view.fovL === view.fovH ? `fov${formatNumber(view.fovL)}` : `fovL${formatNumber(view.fovL)}-fovH${formatNumber(view.fovH)}`
  const stem = `spearv-ra${formatNumber(view.ra)}-dec${formatNumber(view.dec)}-${fov}-${modeLabel(view)}-${release}`
  return stem.replace(/[^a-zA-Z0-9._-]+/g, "-")
}
