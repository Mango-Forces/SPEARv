import { useEffect, useLayoutEffect, useState } from "react"
import type { View } from "../../url-state/view"
import { CATALOG_SOURCES, type CatalogRow, type CatalogSource } from "./catalog"
import { useCatalog, type CatalogState } from "./useCatalog"
import { CandidatesLayer } from "../delta-candidates/CandidatesLayer"
import { ImageViewport } from "../viewport/ImageViewport"
import { useCandidates } from "../delta-candidates/useCandidates"
import { clicksWork, MosaicPicture } from "../mosaic/MosaicPicture"
import type { Mosaic, MosaicBox, MosaicStatus } from "../mosaic/useMosaic"
import type { ColorDisplayView } from "../mosaic/useColorDisplay"
import "./overlay.css"

/** Opacity applied when a catalog layer is switched on. */
export const TOGGLE_ON_OPACITY = 0.7

/** One catalog layer status for the shell layers drawer. The frame owns the
 * single fetch per layer; the drawer renders the only visible controls, so
 * there is no second hidden copy. `supported` is the shared grid flag:
 * false on a native grid above the server's 4096 px click limit, where the
 * catalog x/y would land on the wrong sky and markers stay undrawn. */
export type CatalogStatusReport = { source: CatalogSource; state: CatalogState; supported: boolean }

/** Image or mosaic canvas, plus catalog marker layers. ImageViewport fits and
 * translates the picture, so each SVG uses the image's native size as its
 * viewBox and a marker at (x, y) lands on PNG pixel (x, y): integers are
 * pixel corners, so centres sit at +.5 exactly as the pixelated image draws
 * them. The letterboxed case needs no special casing because the frame, not
 * the row, is the positioning parent. Opacity lives in the page URL, so it
 * round-trips and the back button works; the server only sees ra/dec/fov. */
export function ViewerFrame(props: {
  mosaic: Mosaic
  mosaicStatus?: MosaicStatus
  /** The live painter boxes, one per channel; each gets its colour. */
  mosaicFrontier?: MosaicBox[]
  colorDisplay?: ColorDisplayView
  /** The picture is finished, and on the grid candidate positions use. */
  candidatesReady: boolean
  view: View
  epochMjd: number | undefined
  /** Single-source catalog status for the shell layers drawer. */
  onCatalogStatus?: (report: CatalogStatusReport) => void
  onPick: (pick: { x: number; y: number }) => void
}) {
  const { mosaic, view, epochMjd, onPick, onCatalogStatus } = props
  const [native, setNative] = useState<{ w: number; h: number }>()
  const [hover, setHover] = useState<{ source: CatalogSource; row: CatalogRow }>()
  /** PNG pixel index under the pointer, origin top-left. */
  const [cursor, setCursor] = useState<{ x: number; y: number }>()
  const query = { ra: view.ra, dec: view.dec, fovL: view.fovL, fovH: view.fovH, epochMjd }
  // The server's catalog x/y sit on the 4096 px click grid. Above it they
  // would place markers on the wrong sky, so neither the fetch nor the
  // drawing happens there. `null` keeps meaning the layer is off.
  const markersSupported = clicksWork(mosaic)
  const gaia = useCatalog("gaia", query, view.gaia !== null && markersSupported)
  const wise = useCatalog("wise", query, view.wise !== null && markersSupported)
  const candidates = useCandidates(view, props.candidatesReady)

  useEffect(() => {
    onCatalogStatus?.({ source: "gaia", state: gaia, supported: markersSupported })
  }, [onCatalogStatus, gaia, markersSupported])
  useEffect(() => {
    onCatalogStatus?.({ source: "wise", state: wise, supported: markersSupported })
  }, [onCatalogStatus, wise, markersSupported])

  useLayoutEffect(() => {
    setNative({ w: mosaic.info.width, h: mosaic.info.height })
  }, [mosaic])

  const cursorSky = cursor && native ? skyAtPixel(cursor.x, cursor.y, native.w, native.h, view.ra, view.dec) : undefined

  return (
    <div className="viewer-frame">
      <div className="viewer-body">
        <div className="frame-row">
          <ImageViewport
            native={native}
            resetKey={mosaic.info.id}
            label="Rendered sky"
            hud={
              <>
                {cursorSky ? (
                  <div className="coord-readout" aria-hidden="true">
                    <span>RA {formatRa(cursorSky.ra)}</span>
                    <span>Dec {formatDec(cursorSky.dec)}</span>
                  </div>
                ) : null}
                {hover && native ? (
                  <div
                    className="marker-tip"
                    data-viewport-native-x={hover.row.x}
                    data-viewport-native-y={hover.row.y}
                  >
                    <strong>{hover.row.id}</strong>
                    <span>mag {hover.row.mag ?? "-"}</span>
                  </div>
                ) : null}
              </>
            }
          >
            {(ctx) => (
              <div
                className="overlay-frame"
                onPointerMove={(event) => {
                  if (!native) return
                  const rect = event.currentTarget.getBoundingClientRect()
                  if (rect.width < 1 || rect.height < 1) return
                  const x = Math.min(
                    native.w - 1,
                    Math.max(0, Math.floor(((event.clientX - rect.left) / rect.width) * native.w)),
                  )
                  const y = Math.min(
                    native.h - 1,
                    Math.max(0, Math.floor(((event.clientY - rect.top) / rect.height) * native.h)),
                  )
                  // Re-render on pixel changes only; a marker-heavy frame does not
                  // need to redraw for sub-pixel pointer motion.
                  setCursor((current) => (current?.x === x && current?.y === y ? current : { x, y }))
                }}
                onPointerLeave={() => setCursor(undefined)}
              >
                <MosaicPicture
                  mosaic={mosaic}
                  status={props.mosaicStatus}
                  frontier={props.mosaicFrontier ?? []}
                  colorDisplay={props.colorDisplay}
                  onPick={(pick) => {
                    setHover(undefined)
                    onPick(pick)
                  }}
                />
                {native
                  ? CATALOG_SOURCES.map((source) => {
                      const opacity = source === "gaia" ? view.gaia : view.wise
                      const state = source === "gaia" ? gaia : wise
                      if (opacity === null || !markersSupported || state.status !== "ready" || !state.body) return null
                      return (
                        <svg
                          key={source}
                          className={`markers ${source}`}
                          viewBox={`0 0 ${native.w} ${native.h}`}
                          preserveAspectRatio="none"
                          aria-label={`${source} markers`}
                          style={{ opacity }}
                        >
                          {state.body.rows.map((row) => (
                            <Marker
                              key={row.id}
                              source={source}
                              row={row}
                              native={native}
                              scale={ctx.effectiveScale}
                              onPick={onPick}
                              onHover={setHover}
                            />
                          ))}
                        </svg>
                      )
                    })
                  : null}
                {native ? <CandidatesLayer state={candidates} native={native} scale={ctx.effectiveScale} onPick={onPick} /> : null}
              </div>
            )}
          </ImageViewport>
        </div>
      </div>
    </div>
  )

}

/** SPHEREx's native plate scale, the same constant the server's TAN grid uses. */
const PIXEL_SCALE_DEG = 6.15 / 3600

/** PNG pixel index, origin top-left, to ICRS. Display-only; it mirrors the
 * server's `png_pixel_to_sky`. Picks send the pixel index and the server
 * owns the coordinate, so this readout can never disagree with a click. The
 * sidebar spectrum readout uses the same projection for its click. */
export function skyAtPixel(x: number, y: number, width: number, height: number, ra: number, dec: number): { ra: number; dec: number } {
  const crpixX = (width + 1) / 2
  const crpixY = (height + 1) / 2
  const xi = -(x + 1 - crpixX) * PIXEL_SCALE_DEG
  const eta = (height - y - crpixY) * PIXEL_SCALE_DEG
  const xiR = (xi * Math.PI) / 180
  const etaR = (eta * Math.PI) / 180
  const phi = Math.atan2(xiR, -etaR)
  const theta = Math.atan2(1, Math.hypot(xiR, etaR))
  const ra0 = (ra * Math.PI) / 180
  const dec0 = (dec * Math.PI) / 180
  const sinT = Math.sin(theta)
  const cosT = Math.cos(theta)
  const sinD0 = Math.sin(dec0)
  const cosD0 = Math.cos(dec0)
  const sinDec = Math.max(-1, Math.min(1, sinT * sinD0 - cosT * cosD0 * Math.cos(phi)))
  const skyDec = Math.asin(sinDec)
  const skyRa = ra0 + Math.atan2(cosT * Math.sin(phi), sinT * cosD0 + cosT * sinD0 * Math.cos(phi))
  return {
    ra: (((skyRa * 180) / Math.PI) % 360 + 360) % 360,
    dec: (skyDec * 180) / Math.PI,
  }
}

function formatRa(ra: number): string {
  const wrapped = ((ra % 360) + 360) % 360
  const rounded = (Math.round(wrapped * 10000) % 3600000) / 10000
  return `${rounded.toFixed(4)}°`
}

function formatDec(dec: number): string {
  const rounded = Math.round(dec * 10000) / 10000
  const sign = rounded < 0 ? "-" : "+"
  return `${sign}${Math.abs(rounded).toFixed(4)}°`
}

/** Radius in screen pixels. Magnitudes live in different ranges per catalog,
 * so each source maps its own bright-to-faint span into the same tight range. */
function markerRadius(source: CatalogSource, mag: number | null): number {
  const bright = source === "gaia" ? 14 : 11
  const faint = source === "gaia" ? 22 : 18.5
  const faded = mag === null ? 0.4 : Math.min(1, Math.max(0, (mag - bright) / (faint - bright)))
  return 4.8 - faded * 1.6
}

export function Marker(props: {
  source: CatalogSource
  row: CatalogRow
  native: { w: number; h: number }
  scale: number
  onPick: (pick: { x: number; y: number }) => void
  onHover: (hover: { source: CatalogSource; row: CatalogRow } | undefined) => void
}) {
  const { source, row, native, scale, onPick, onHover } = props
  // Native-pixel geometry, divided by the display scale so the ring stays a
  // few screen pixels across at any grid size. Hollow, with a clear centre gap.
  const radius = markerRadius(source, row.mag) / Math.max(scale, 0.01)
  const pick = {
    x: Math.min(native.w - 1, Math.max(0, Math.floor(row.x))),
    y: Math.min(native.h - 1, Math.max(0, Math.floor(row.y))),
  }
  const label = `${row.id} · mag ${row.mag ?? "-"}`
  const diamond = `0,${-radius} ${radius},0 0,${radius} ${-radius},0`
  return (
    <g
      className="marker"
      role="button"
      tabIndex={0}
      aria-label={`${source} ${label}`}
      onClick={(event) => {
        event.stopPropagation()
        onHover(undefined)
        onPick(pick)
      }}
      onKeyDown={(event) => {
        if (event.key === "Enter" || event.key === " ") {
          event.preventDefault()
          onHover(undefined)
          onPick(pick)
        }
      }}
      onMouseEnter={() => onHover({ source, row })}
      onMouseLeave={() => onHover(undefined)}
    >
      <title>{label}</title>
      {source === "gaia" ? (
        <>
          <circle className="marker-halo" cx={row.x} cy={row.y} r={radius} />
          <circle className="marker-shape" cx={row.x} cy={row.y} r={radius} />
        </>
      ) : (
        <>
          <polygon className="marker-halo" points={diamond} transform={`translate(${row.x} ${row.y})`} />
          <polygon className="marker-shape" points={diamond} transform={`translate(${row.x} ${row.y})`} />
        </>
      )}
      <circle className="marker-hit" cx={row.x} cy={row.y} r={8 / Math.max(scale, 0.01)} />
    </g>
  )
}
