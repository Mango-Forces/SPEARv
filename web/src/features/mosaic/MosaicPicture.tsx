import { useLayoutEffect, useRef, useState } from "react"
import "./mosaic.css"
import type { Mosaic, MosaicBox, MosaicStatus } from "./useMosaic"
import type { ColorDisplayView } from "./useColorDisplay"

/** `png_pixel_to_sky` and `sky_to_png_pixel` clamp their grid here. Past it the
 * server's click grid is not this picture's grid. */
const CLICK_GRID_LIMIT = 4096

export function clicksWork(mosaic: Mosaic | undefined): boolean {
  return !mosaic || mosaic.info.width <= CLICK_GRID_LIMIT && mosaic.info.height <= CLICK_GRID_LIMIT
}

/** The canvas is built by `useMosaic`, so tiles can land before this mounts. The
 * tile grid and the painter boxes are one SVG in grid pixels; their strokes
 * stay a fixed width on screen however far CSS scales the picture. The canvas
 * carries `data-export-image` so export picks the currently visible raster;
 * Specific workspace canvases set it too while they show a picture. */
export function MosaicPicture(props: {
  mosaic: Mosaic
  status: MosaicStatus | undefined
  /** The live painter boxes, one per channel, each one tile in size. */
  frontier: MosaicBox[]
  onPick: (pick: { x: number; y: number }) => void
  colorDisplay?: ColorDisplayView
}) {
  const { mosaic, status, frontier, onPick, colorDisplay } = props
  const holder = useRef<HTMLDivElement>(null)
  const [loadedFull, setLoadedFull] = useState<string>()
  const [failedFull, setFailedFull] = useState<string>()
  const [failedPreview, setFailedPreview] = useState<string>()
  const { info } = mosaic
  const pickable = clicksWork(mosaic)
  const working = !status || status.state === "planning" || status.state === "reading" || frontier.length > 0
  const currentColor = colorDisplay?.jobId === info.id ? colorDisplay : undefined
  const previewUrl = currentColor?.previewUrl
  const fullUrl = currentColor?.fullUrl
  const showingFull = !!fullUrl && loadedFull === fullUrl
  const showingPreview = !!previewUrl && !showingFull && failedPreview !== previewUrl

  useLayoutEffect(() => {
    mosaic.canvas.setAttribute("data-export-image", "true")
    holder.current?.replaceChildren(mosaic.canvas)
  }, [mosaic.canvas])

  return (
    <div
      className={pickable ? "mosaic pickable" : "mosaic"}
      role={pickable ? "button" : undefined}
      tabIndex={pickable ? 0 : undefined}
      aria-label={pickable ? "Sample the center pixel's spectrum" : undefined}
      onKeyDown={
        pickable
          ? (event) => {
              if (event.key !== "Enter" && event.key !== " ") return
              event.preventDefault()
              if (event.repeat) return
              onPick({ x: Math.floor(info.width / 2), y: Math.floor(info.height / 2) })
            }
          : undefined
      }
      onClick={(event) => {
        if (!pickable) return
        const rect = event.currentTarget.getBoundingClientRect()
        const scaled = (offset: number, span: number, size: number) =>
          Math.min(size - 1, Math.max(0, Math.floor((offset / span) * size)))
        onPick({
          x: scaled(event.clientX - rect.left, rect.width, info.width),
          y: scaled(event.clientY - rect.top, rect.height, info.height),
        })
      }}
    >
      <div className={`mosaic-canvas${showingPreview || showingFull ? " mosaic-canvas--replaced" : ""}`} ref={holder} />
      {showingPreview ? (
        <>
          <img
            className="mosaic-server-image"
            src={previewUrl}
            alt={currentColor?.fullBlob ? "Bounded Color preview; the original full-resolution PNG remains available to download." : "Bounded Color preview while the full-resolution PNG is prepared."}
            onError={() => { if (previewUrl) setFailedPreview(previewUrl) }}
          />
          <span className="mosaic-preview-label">
            {currentColor?.fullBlob ? "Preview · original PNG available for download" : "Bounded preview · preparing the full-resolution PNG"}
          </span>
        </>
      ) : null}
      {fullUrl && failedFull !== fullUrl ? (
        <img
          className={`mosaic-server-image${showingFull ? "" : " mosaic-server-image--pending"}`}
          src={fullUrl}
          alt={showingFull ? "Server-rendered full-resolution Color PNG." : ""}
          aria-hidden={!showingFull}
          onLoad={() => setLoadedFull(fullUrl)}
          onError={() => setFailedFull(fullUrl)}
        />
      ) : null}
      {working ? (
        <svg className="mosaic-grid" viewBox={`0 0 ${info.width} ${info.height}`} preserveAspectRatio="none" aria-hidden="true">
          {frontier.length > 0 ? <path className="mosaic-lines" d={gridPath(info.width, info.height, info.tile)} /> : null}
          {frontier.map((box, i) => (
            <rect
              key={`${box.label}-${i}`}
              className="mosaic-box"
              x={box.x * info.tile + 1}
              y={box.y * info.tile + 1}
              width={Math.min(info.tile, info.width - box.x * info.tile) - 2}
              height={Math.min(info.tile, info.height - box.y * info.tile) - 2}
              stroke={box.color}
            />
          ))}
        </svg>
      ) : null}
    </div>
  )
}

function gridPath(width: number, height: number, tile: number): string {
  const lines: string[] = []
  for (let x = tile; x < width; x += tile) lines.push(`M${x} 0V${height}`)
  for (let y = tile; y < height; y += tile) lines.push(`M0 ${y}H${width}`)
  return lines.join("")
}
