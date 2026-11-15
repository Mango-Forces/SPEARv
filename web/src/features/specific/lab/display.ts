// Display mapping for native Specific grids. A 30° sequence is 17561×17561:
// a canvas at that size needs 1.2 GiB of RGBA backing store, which Chromium
// accepts but cannot paint. The backing store is therefore bounded while the
// served grid, frozen stretch and frame selection remain at native size.

/** Longest canvas backing dimension. 2048² RGBA is 16 MiB. */
export const MAX_DISPLAY_DIMENSION = 2048

/** Opaque no-data colour used by both the frame and snippet surfaces. */
export const NO_DATA_RGB = [24, 28, 36] as const
export const NO_DATA = `rgb(${NO_DATA_RGB.join(", ")})`

export type Size = { width: number; height: number }

/** Backing size for a native grid. Small grids stay native; larger ones keep
 * their aspect ratio with the longest dimension capped at 2048. */
export function displaySize(width: number, height: number): Size {
  const longest = Math.max(width, height)
  if (longest <= MAX_DISPLAY_DIMENSION) return { width, height }
  const scale = MAX_DISPLAY_DIMENSION / longest
  return {
    width: Math.max(1, Math.round(width * scale)),
    height: Math.max(1, Math.round(height * scale)),
  }
}

export type TileDestination = {
  x: number
  y: number
  width: number
  height: number
  /** Source pixels of the 256×256 tile image that lie inside the grid; the
   * remainder is transparent and must not be squeezed into the destination. */
  sourceWidth: number
  sourceHeight: number
}

/** Destination rectangle of one native data tile on the bounded surface. Both
 * edges round from the same native boundary, so adjacent tiles share an edge and
 * leave no gaps; the final row/column is cropped at the grid edge. */
export function tileDestination(tileX: number, tileY: number, tileSize: number, grid: Size, display: Size): TileDestination {
  const left = Math.round(tileX * tileSize * display.width / grid.width)
  const top = Math.round(tileY * tileSize * display.height / grid.height)
  const right = Math.round(Math.min(grid.width, (tileX + 1) * tileSize) * display.width / grid.width)
  const bottom = Math.round(Math.min(grid.height, (tileY + 1) * tileSize) * display.height / grid.height)
  return {
    x: left,
    y: top,
    width: Math.max(1, right - left),
    height: Math.max(1, bottom - top),
    sourceWidth: Math.max(1, Math.min(tileSize, grid.width - tileX * tileSize)),
    sourceHeight: Math.max(1, Math.min(tileSize, grid.height - tileY * tileSize)),
  }
}

/** Replace the server's no-data grey with a checkerboard. Missing coverage is
 * then a visible pattern in every frame, not a flat area that reads as sky. */
export function paintNoData(canvas: HTMLCanvasElement) {
  const context = canvas.getContext("2d")
  if (!context || canvas.width === 0 || canvas.height === 0) return
  const image = context.getImageData(0, 0, canvas.width, canvas.height)
  const pixels = new Uint32Array(image.data.buffer)
  // RGBA little-endian bytes [24, 28, 36, 255].
  const missing = 0xff241c18
  const dark = 0xff0d1014
  const light = 0xff3a424c
  for (let y = 0; y < canvas.height; y++) {
    for (let x = 0; x < canvas.width; x++) {
      const index = y * canvas.width + x
      if (pixels[index] !== missing) continue
      pixels[index] = (x + y) % 8 < 3 ? light : dark
    }
  }
  context.putImageData(image, 0, 0)
}

/** Canvas for one decoded frame or snippet, filled with no-data and configured
 * for exact nearest-neighbour placement of the native tiles. */
export function createDisplayCanvas(nativeWidth: number, nativeHeight: number) {
  const size = displaySize(nativeWidth, nativeHeight)
  const canvas = document.createElement("canvas")
  canvas.width = size.width
  canvas.height = size.height
  const context = canvas.getContext("2d")
  if (!context) throw new Error("Canvas is unavailable.")
  context.imageSmoothingEnabled = false
  context.fillStyle = NO_DATA
  context.fillRect(0, 0, size.width, size.height)
  return { canvas, context, size }
}
